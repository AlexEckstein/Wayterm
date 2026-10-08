//! Tab-level transport resilience: confirm-before-close, the reconnect
//! prompt, and the two window actions (doc 04 §Lifecycle & Cancellation,
//! doc 07 §Default Actions).
//!
//! Doc 04 assigns the *policy* to the transport layer, which lives in
//! [`crate::transport::reconnect`]: the backoff curve, the two modes, and
//! the state transitions. This module is the GTK half — it owns the
//! dialogs and the window actions, and it keeps a small registry so the
//! display-free policy can be asked "what is this tab's transport doing?"
//! without the policy knowing anything about GTK.
//!
//! # Wiring seam (P3-3 owns the connect flow)
//!
//! Doc 04: "Reconnect = fresh `connect()` through the supervisor". The
//! supervisor does not exist in this branch, so this module does **not**
//! own a transport. Instead the connect flow registers a
//! [`LiveTab`] per tab — the connect closure, the tab's cancellation
//! token, and the last known [`TransportState`] — via
//! [`register_live_tab`], and the two window actions drive that registry.
//! With nothing registered (the pre-P3-3 shape, or a tab that never
//! connected) the actions are no-ops that log the seam, never panics.
//!
//! Disconnect deliberately does *not* call `Transport::disconnect` from
//! here: doc 04 gives the tab's cancel token to the pump task, whose last
//! act is `transport.disconnect()`. Cancelling is therefore both the
//! doc-04-conformant disconnect and the thing that stops an in-flight
//! reconnect loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gtk4::gio;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::transport::reconnect::{
    self, ConfirmFn, ConnectFn, ReconnectDriver, ReconnectEvent, ReconnectPolicy,
};
use crate::transport::types::{TransportError, TransportEvent, TransportState};

/// Doc 07's action id for Reconnect (Ctrl+Shift+R, bound by the hotkey
/// lane — this module registers the action *unbound*).
pub const RECONNECT_ACTION: &str = "reconnect";

/// Doc 07's action id for Disconnect (Ctrl+Shift+X).
pub const DISCONNECT_ACTION: &str = "disconnect";

/// The confirm-close dialog's message (doc 04: "Confirm-before-close if
/// processes running").
pub const CONFIRM_CLOSE_MESSAGE: &str = "Close this session?";

/// The confirm-close dialog's detail line.
pub const CONFIRM_CLOSE_DETAIL: &str = "Processes may be running — close anyway?";

/// The reconnect prompt's message (doc 04 §Lifecycle: "UI shows the
/// reconnect prompt").
pub const RECONNECT_PROMPT_MESSAGE: &str = "Connection lost";

/// The reconnect prompt's detail line; `{attempt}` is the 1-based attempt
/// number.
pub const RECONNECT_PROMPT_DETAIL: &str = "Reconnect to {host}? (attempt {attempt})";

/// Buttons for both dialogs, in the order doc 04 implies: cancel first,
/// the affirmative action second. [`CONFIRM_CLOSE_ANSWER`] and
/// [`RECONNECT_PROMPT_ANSWER`] are their indices.
pub const DIALOG_BUTTONS: [&str; 2] = ["Cancel", "Close"];
pub const CONFIRM_CLOSE_ANSWER: i32 = 1;
pub const RECONNECT_PROMPT_ANSWER: i32 = 1;

/// Settings keys (doc 06 §Default Data).
pub const CONFIRM_CLOSE_SETTING: &str = "confirm_close";
pub const RECONNECT_ON_DROP_SETTING: &str = "reconnect_on_drop";

/// What the connect flow registers for one live tab.
///
/// Deliberately narrow: a connect closure (doc 04's "fresh `connect()`
/// through the supervisor"), the tab's cancellation token, and the last
/// state the UI observed on the tab's second channel. No GTK types, no
/// transport ownership — the supervisor/tab keeps those.
#[derive(Clone)]
pub struct LiveTab {
    /// Doc 04: a *fresh* connect per attempt, so tmux reattaches.
    pub connect: ConnectFn,
    /// The tab's token; `close()` and Disconnect both fire it.
    pub cancel: CancellationToken,
    /// Last observed state (status bar / close guard read this).
    pub state: TransportState,
}

impl LiveTab {
    /// A live tab in the given state.
    pub fn new(connect: ConnectFn, cancel: CancellationToken, state: TransportState) -> Self {
        Self {
            connect,
            cancel,
            state,
        }
    }
}

/// Registry of live tabs, keyed by `sessions.id` (the key `tab.rs`
/// already stashes on every tab's content widget).
static LIVE_TABS: Mutex<Option<HashMap<i64, LiveTab>>> = Mutex::new(None);

/// `confirm_close`, default ON (doc 06 seeds `'1'`).
static CONFIRM_CLOSE: AtomicBool = AtomicBool::new(true);

/// `reconnect_on_drop`, default ON (doc 06 seeds `'1'`).
static RECONNECT_ON_DROP: AtomicBool = AtomicBool::new(true);

fn live_tabs_lock() -> std::sync::MutexGuard<'static, Option<HashMap<i64, LiveTab>>> {
    LIVE_TABS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A connect closure that always fails, for callers that must supply one
/// before a real supervisor exists (and for the API-touch smoke path).
pub fn never_connect() -> ConnectFn {
    Arc::new(|| Box::pin(async { Err(TransportError::Disconnected) }) as reconnect::ConnectFuture)
}

/// Install the two settings the resilience policy reads, resolved once at
/// startup by the window (the only holder of the store there).
///
/// Values are the raw settings rows; missing/unparseable values keep the
/// documented default (ON) rather than silently disabling a safety
/// prompt.
pub fn install_settings(confirm_close: Option<&str>, reconnect_on_drop: Option<&str>) {
    CONFIRM_CLOSE.store(
        reconnect::confirm_close_enabled(confirm_close),
        Ordering::SeqCst,
    );
    RECONNECT_ON_DROP.store(
        reconnect::reconnect_on_drop_enabled(reconnect_on_drop),
        Ordering::SeqCst,
    );
}

/// The active `confirm_close` setting.
pub fn confirm_close_setting() -> bool {
    CONFIRM_CLOSE.load(Ordering::SeqCst)
}

/// The active `reconnect_on_drop` setting.
pub fn reconnect_on_drop_setting() -> bool {
    RECONNECT_ON_DROP.load(Ordering::SeqCst)
}

/// The policy doc 04 describes for a new drop: auto when the setting is
/// on, prompt when it is off.
pub fn policy() -> ReconnectPolicy {
    reconnect::policy_from_settings(Some(if reconnect_on_drop_setting() {
        "1"
    } else {
        "0"
    }))
}

/// Register (or update) a tab's live transport. Idempotent per session.
pub fn register_live_tab(session_id: i64, tab: LiveTab) {
    let mut guard = live_tabs_lock();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(session_id, tab);
}

/// Forget a tab's transport (tab closed).
pub fn unregister_live_tab(session_id: i64) {
    if let Some(map) = live_tabs_lock().as_mut() {
        map.remove(&session_id);
    }
}

/// The registered transport for a tab, if any.
pub fn live_tab(session_id: i64) -> Option<LiveTab> {
    live_tabs_lock()
        .as_ref()
        .and_then(|m| m.get(&session_id))
        .cloned()
}

/// Last observed state for a tab; `Disconnected` when nothing is
/// registered (a tab that never connected has nothing to confirm).
pub fn tab_state(session_id: i64) -> TransportState {
    live_tab(session_id)
        .map(|t| t.state)
        .unwrap_or(TransportState::Disconnected)
}

/// Record a new state for a registered tab (the state-channel drain
/// calls this). Unknown sessions are ignored — a tab can close while a
/// state is still in flight.
pub fn set_tab_state(session_id: i64, state: TransportState) {
    if let Some(map) = live_tabs_lock().as_mut() {
        if let Some(tab) = map.get_mut(&session_id) {
            tab.state = state;
        }
    }
}

/// Should closing `session_id` ask first?
///
/// Display-free: the decision is
/// [`reconnect::should_confirm_close`] applied to the tab's last observed
/// state and the installed `confirm_close` setting.
pub fn should_confirm_close_for_tab(session_id: i64) -> bool {
    reconnect::should_confirm_close(&tab_state(session_id), confirm_close_setting())
}

// ---------------------------------------------------------------------------
// Actions (doc 07: reconnect Ctrl+Shift+R, disconnect Ctrl+Shift+X)
// ---------------------------------------------------------------------------

/// What activating `reconnect` did — the display-free core of the action,
/// so the routing is testable without GTK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectAction {
    /// A [`ReconnectDriver`] was spawned for this tab.
    Started { session_id: i64 },
    /// A driver is already running for this tab (double activation is a
    /// no-op, so a held key cannot stack loops).
    AlreadyRunning { session_id: i64 },
    /// No transport registered for this tab — the P3-3 supervisor seam.
    NoLiveTab { session_id: Option<i64> },
}

/// What activating `disconnect` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectAction {
    /// The tab's token was fired; the pump's last act disconnects the
    /// transport (doc 04 §Lifecycle) and any reconnect loop stops.
    Cancelled { session_id: i64 },
    /// Nothing registered — the P3-3 supervisor seam.
    NoLiveTab { session_id: Option<i64> },
}

impl ReconnectAction {
    /// Did this activation start a reconnect loop?
    pub fn started(self) -> bool {
        matches!(self, Self::Started { .. })
    }
}

impl DisconnectAction {
    /// Did this activation cancel something?
    pub fn cancelled(self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }
}

/// Tracks which tabs already have a running driver, so a second
/// activation cannot stack retry loops on the same transport.
static RUNNING: Mutex<Vec<i64>> = Mutex::new(Vec::new());

fn mark_running(session_id: i64) -> bool {
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if running.contains(&session_id) {
        false
    } else {
        running.push(session_id);
        true
    }
}

fn clear_running(session_id: i64) {
    RUNNING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|id| *id != session_id);
}

/// Activate doc 07's `reconnect` action for `session_id`.
///
/// Spawns a [`ReconnectDriver`] on the ambient tokio runtime with the
/// policy [`policy`] selects. `state_tx` is the tab's second channel
/// (doc 04 §Wiring), so the status bar sees
/// `Reconnecting { attempt }` for free; `confirm` is prompt mode's
/// responder — [`reconnect_prompt`] in the GTK path.
pub fn activate_reconnect(
    session_id: Option<i64>,
    state_tx: Option<mpsc::UnboundedSender<TransportEvent>>,
    confirm: Option<ConfirmFn>,
) -> ReconnectAction {
    let Some(id) = session_id else {
        return ReconnectAction::NoLiveTab { session_id: None };
    };
    let Some(tab) = live_tab(id) else {
        return ReconnectAction::NoLiveTab {
            session_id: Some(id),
        };
    };
    if !mark_running(id) {
        return ReconnectAction::AlreadyRunning { session_id: id };
    }

    let driver = ReconnectDriver::with_cancel_token(policy(), tab.cancel.clone());
    let (events, event_rx) = mpsc::unbounded_channel::<ReconnectEvent>();
    // Drain the driver's own progress channel: the state channel already
    // carries everything the UI needs, and an unread channel would be a
    // leak, not a feature.
    tokio::spawn(async move {
        let mut event_rx = event_rx;
        while event_rx.recv().await.is_some() {}
        clear_running(id);
    });

    driver.spawn(tab.connect, events, confirm, state_tx);
    ReconnectAction::Started { session_id: id }
}

/// Activate doc 07's `disconnect` action for `session_id`.
///
/// Cancels the tab's token: the pump task exits and its last act is
/// `transport.disconnect()` (doc 04 §Lifecycle), the tab stays open and
/// the state reads `Disconnected`. An in-flight reconnect loop shares the
/// token, so it stops too.
pub fn activate_disconnect(session_id: Option<i64>) -> DisconnectAction {
    let Some(id) = session_id else {
        return DisconnectAction::NoLiveTab { session_id: None };
    };
    let Some(tab) = live_tab(id) else {
        return DisconnectAction::NoLiveTab {
            session_id: Some(id),
        };
    };
    tab.cancel.cancel();
    set_tab_state(id, TransportState::Disconnected);
    DisconnectAction::Cancelled { session_id: id }
}

/// Drop all registry state (tests; also a safe reset for a fresh window).
pub fn reset_registry() {
    live_tabs_lock().take();
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).clear();
    install_settings(None, None);
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// Ask before closing a tab with a live transport (doc 04 §Lifecycle,
/// setting `confirm_close`).
///
/// Returns immediately: `on_close` runs from the dialog's response
/// callback when the user confirms, and never when they cancel. The
/// button layout follows `session_tree.rs`'s delete-confirmation pattern
/// (Cancel first, affirmative second, Cancel as both cancel and default
/// is *not* used — closing a tab is the safer default here).
pub fn confirm_close_dialog(parent: Option<&gtk4::Window>, on_close: impl FnOnce() + 'static) {
    let dialog = gtk4::AlertDialog::builder()
        .message(CONFIRM_CLOSE_MESSAGE)
        .detail(CONFIRM_CLOSE_DETAIL)
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", DIALOG_BUTTONS[CONFIRM_CLOSE_ANSWER as usize]]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(CONFIRM_CLOSE_ANSWER);

    dialog.choose(parent, gio::Cancellable::NONE, move |res| {
        if let Ok(answer) = res {
            if answer == CONFIRM_CLOSE_ANSWER {
                on_close();
            }
        }
    });
}

/// Cancel this tab's transport and forget it (doc 04 §Lifecycle: "Tab
/// close: cancel token → pump task exits → `transport.disconnect()`").
///
/// Firing the token *before* the page goes away is what stops an in-flight
/// reconnect loop: the driver shares the tab's token, so the loop that doc 04
/// lets run "unlimited attempts while the tab is open" cannot outlive the
/// tab. Unregistering then lets the supervisor drop its `Arc<SshConnection>`
/// once the last consumer releases it.
///
/// A no-op for a tab that never connected (nothing registered).
pub fn teardown_tab(session_id: i64) {
    if let Some(tab) = live_tab(session_id) {
        tab.cancel.cancel();
    }
    unregister_live_tab(session_id);
    clear_running(session_id);
}

/// Tab-close entry point: confirm if the policy says so, then close.
///
/// `close` performs the actual page removal (whatever the caller's tab
/// bookkeeping is — this module never removes a page itself, so it does
/// not have to know how tabs are tracked). [`teardown_tab`] runs first, so
/// the cancel precedes the teardown in doc 04's order.
pub fn request_close(
    session_id: i64,
    parent: Option<&gtk4::Window>,
    close: impl FnOnce() + 'static,
) {
    if should_confirm_close_for_tab(session_id) {
        confirm_close_dialog(parent, move || {
            teardown_tab(session_id);
            close();
        });
    } else {
        teardown_tab(session_id);
        close();
    }
}

/// Build a prompt-mode [`ConfirmFn`] that asks on the GTK main thread.
///
/// The dialog runs asynchronously and the answer travels back over a
/// oneshot, so the driver never blocks a tokio worker on GTK (the same
/// marshalling discipline as `auth_dialog::challenge_responder_with`, but
/// with a future instead of a blocking call because the reconnect loop is
/// async).
pub fn reconnect_prompt(host: String) -> ConfirmFn {
    Arc::new(move |attempt: u32| {
        let host = host.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        // The dialog must be built on the GTK thread; the driver runs on
        // tokio.
        gtk4::glib::MainContext::default().invoke(move || {
            let dialog = gtk4::AlertDialog::builder()
                .message(RECONNECT_PROMPT_MESSAGE)
                .detail(
                    RECONNECT_PROMPT_DETAIL
                        .replace("{host}", &host)
                        .replace("{attempt}", &attempt.to_string()),
                )
                .modal(true)
                .build();
            dialog.set_buttons(&["No", "Reconnect"]);
            dialog.set_cancel_button(0);
            dialog.set_default_button(RECONNECT_PROMPT_ANSWER);
            dialog.choose(gtk4::Window::NONE, gio::Cancellable::NONE, move |res| {
                let _ = tx.send(matches!(res, Ok(RECONNECT_PROMPT_ANSWER)));
            });
        });
        Box::pin(async move { rx.await.unwrap_or(false) })
    })
}

/// Install the two doc 07 actions on `window`, driven against the tab
/// `current_session_id` reports.
///
/// Registered *unbound*: doc 07's accelerators (`Ctrl+Shift+R` /
/// `Ctrl+Shift+X`) belong to the hotkey engine, which is a separate lane;
/// `gtk4::ShortcutController` will activate these action names.
pub fn install_connection_actions<F>(window: &gtk4::ApplicationWindow, current_session_id: F)
where
    F: Fn() -> Option<i64> + Clone + 'static,
{
    let group = gio::SimpleActionGroup::new();

    let reconnect = gio::SimpleAction::new(RECONNECT_ACTION, None);
    let current_for_reconnect = current_session_id.clone();
    reconnect.connect_activate(move |_, _| {
        let session_id = current_for_reconnect();
        let result = activate_reconnect(
            session_id,
            None,
            if reconnect_on_drop_setting() {
                // Auto mode never asks.
                None
            } else {
                Some(reconnect_prompt("the session".into()))
            },
        );
        if matches!(result, ReconnectAction::NoLiveTab { .. }) {
            eprintln!(
                "Wayterm: reconnect has no live transport for this tab yet (supervisor seam)"
            );
        }
    });
    group.add_action(&reconnect);

    let disconnect = gio::SimpleAction::new(DISCONNECT_ACTION, None);
    let current_for_disconnect = current_session_id;
    disconnect.connect_activate(move |_, _| {
        let result = activate_disconnect(current_for_disconnect());
        if matches!(result, DisconnectAction::NoLiveTab { .. }) {
            eprintln!(
                "Wayterm: disconnect has no live transport for this tab yet (supervisor seam)"
            );
        }
    });
    group.add_action(&disconnect);

    window.insert_action_group("win", Some(&group));
}

#[cfg(test)]
mod tests;
