//! Confirm-before-close, the reconnect prompt, and the `reconnect` /
//! `disconnect` window actions (doc 03 §Lifecycle).
//!
//! The policy lives in [`crate::transport::reconnect`]; this is its GTK
//! side. The actions act on a registry of [`LiveTab`]s keyed by tab id (one
//! session can have several tabs open), which the connect flow (`ui::tab`)
//! fills for every streaming tab; with nothing registered they are no-ops.
//! Disconnect cancels the tab's token: the pump's last act is
//! `transport.disconnect()`, and any reconnect loop sharing the token stops.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use gtk4::gio;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::store::SessionStore;
use crate::transport::reconnect::{ConfirmFn, ConnectFn, ReconnectDriver, ReconnectPolicy};
use crate::transport::types::{TransportEvent, TransportState};
use crate::ui::widgets::confirm_with_opt_out;

/// Doc 11 action ids (the hotkey engine owns their chords).
pub const RECONNECT_ACTION: &str = "reconnect";
pub const DISCONNECT_ACTION: &str = "disconnect";

/// Settings rows (doc 13 §Settings rows).
pub const CONFIRM_CLOSE_SETTING: &str = "confirm_close";
pub const RECONNECT_ON_DROP_SETTING: &str = "reconnect_on_drop";

/// Settings row (doc 13 §Settings rows): the close warning shows unless this is off.
pub const CONFIRM_CLOSE_NOTICE_SETTING: &str = "confirm_close_notice";

pub const CONFIRM_CLOSE_DETAIL: &str = "Processes may be running — close anyway?";

/// What a connected tab registers.
#[derive(Clone)]
pub struct LiveTab {
    /// A fresh connect per attempt, so tmux reattaches.
    pub connect: ConnectFn,
    /// The tab's token; closing and Disconnect both fire it.
    pub cancel: CancellationToken,
    /// The last state seen on the tab's state channel.
    pub state: TransportState,
}

impl LiveTab {
    pub fn new(connect: ConnectFn, cancel: CancellationToken, state: TransportState) -> Self {
        Self {
            connect,
            cancel,
            state,
        }
    }
}

static LIVE_TABS: Mutex<BTreeMap<i64, LiveTab>> = Mutex::new(BTreeMap::new());
/// Tabs with a reconnect loop running (a second activation is a no-op).
static RUNNING: Mutex<BTreeSet<i64>> = Mutex::new(BTreeSet::new());
static CONFIRM_CLOSE: AtomicBool = AtomicBool::new(true);
static RECONNECT_ON_DROP: AtomicBool = AtomicBool::new(true);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The two settings, read once at startup.
pub fn install_settings(confirm_close: bool, reconnect_on_drop: bool) {
    CONFIRM_CLOSE.store(confirm_close, Ordering::SeqCst);
    RECONNECT_ON_DROP.store(reconnect_on_drop, Ordering::SeqCst);
}

pub fn confirm_close_setting() -> bool {
    CONFIRM_CLOSE.load(Ordering::SeqCst)
}

pub fn reconnect_on_drop_setting() -> bool {
    RECONNECT_ON_DROP.load(Ordering::SeqCst)
}

/// Auto-reconnect when `reconnect_on_drop` is on, prompt otherwise.
pub fn policy() -> ReconnectPolicy {
    ReconnectPolicy::for_setting(reconnect_on_drop_setting())
}

pub fn register_live_tab(tab_id: i64, tab: LiveTab) {
    lock(&LIVE_TABS).insert(tab_id, tab);
}

/// Register a tab whose transport was just wired (its first connect or a
/// redial), as `Connecting`. An uncancelled token is kept, so a running
/// reconnect loop and the tab keep sharing one; a cancelled one (after
/// Disconnect) is replaced. Returns the tab's token.
pub fn register_connected_tab(tab_id: i64, connect: ConnectFn) -> CancellationToken {
    let mut tabs = lock(&LIVE_TABS);
    let cancel = tabs
        .get(&tab_id)
        .map(|tab| tab.cancel.clone())
        .filter(|cancel| !cancel.is_cancelled())
        .unwrap_or_default();
    tabs.insert(
        tab_id,
        LiveTab::new(connect, cancel.clone(), TransportState::Connecting),
    );
    cancel
}

pub fn unregister_live_tab(tab_id: i64) {
    lock(&LIVE_TABS).remove(&tab_id);
}

pub fn live_tab(tab_id: i64) -> Option<LiveTab> {
    lock(&LIVE_TABS).get(&tab_id).cloned()
}

/// `Disconnected` for a tab that never registered.
pub fn tab_state(tab_id: i64) -> TransportState {
    live_tab(tab_id).map_or(TransportState::Disconnected, |t| t.state)
}

/// Ignored for a tab that is not registered (it may have just closed).
pub fn set_tab_state(tab_id: i64, state: TransportState) {
    if let Some(tab) = lock(&LIVE_TABS).get_mut(&tab_id) {
        tab.state = state;
    }
}

pub fn should_confirm_close_for_tab(tab_id: i64) -> bool {
    crate::transport::reconnect::should_confirm_close(&tab_state(tab_id), confirm_close_setting())
}

/// [`should_confirm_close_for_tab`], unless the warning was silenced with
/// "Do not show me again". Without a store the warning always shows.
pub fn should_show_close_warning(tab_id: i64, store: Option<&SessionStore>) -> bool {
    should_confirm_close_for_tab(tab_id)
        && store.is_none_or(|store| store.flag(CONFIRM_CLOSE_NOTICE_SETTING))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectAction {
    Started { tab_id: i64 },
    AlreadyRunning { tab_id: i64 },
    NoLiveTab { tab_id: Option<i64> },
}

impl ReconnectAction {
    pub fn started(self) -> bool {
        matches!(self, Self::Started { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectAction {
    Cancelled { tab_id: i64 },
    NoLiveTab { tab_id: Option<i64> },
}

impl DisconnectAction {
    pub fn cancelled(self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }
}

/// `reconnect`: run a [`ReconnectDriver`] for the tab on its own token.
/// `state_tx` (the tab's state channel) sees `Reconnecting { attempt }`;
/// `confirm` answers prompt mode.
pub fn activate_reconnect(
    tab_id: Option<i64>,
    state_tx: Option<mpsc::UnboundedSender<TransportEvent>>,
    confirm: Option<ConfirmFn>,
) -> ReconnectAction {
    let Some(id) = tab_id else {
        return ReconnectAction::NoLiveTab { tab_id: None };
    };
    let Some(tab) = live_tab(id) else {
        return ReconnectAction::NoLiveTab { tab_id: Some(id) };
    };
    if !lock(&RUNNING).insert(id) {
        return ReconnectAction::AlreadyRunning { tab_id: id };
    }
    // After Disconnect the token is spent; reconnecting gets a fresh one.
    let cancel = match lock(&LIVE_TABS).get_mut(&id) {
        Some(live) if live.cancel.is_cancelled() => {
            live.cancel = CancellationToken::new();
            live.cancel.clone()
        }
        _ => tab.cancel,
    };
    // The driver's own progress events go unwatched: the state channel
    // carries what the UI shows.
    let (events, _) = mpsc::unbounded_channel();
    let run = ReconnectDriver::with_cancel_token(policy(), cancel).spawn(
        tab.connect,
        events,
        confirm,
        state_tx,
    );
    tokio::spawn(async move {
        let _ = run.await;
        lock(&RUNNING).remove(&id);
    });
    ReconnectAction::Started { tab_id: id }
}

/// A tab's state channel reported `ConnectionLost` (doc 03 §Lifecycle):
/// the tab reads as disconnected and, with `reconnect_on_drop` on, a driver
/// starts on `state_tx`. Prompt mode leaves it to the `reconnect` action.
pub fn on_connection_lost(
    tab_id: i64,
    state_tx: mpsc::UnboundedSender<TransportEvent>,
) -> Option<ReconnectAction> {
    set_tab_state(tab_id, TransportState::Disconnected);
    reconnect_on_drop_setting().then(|| activate_reconnect(Some(tab_id), Some(state_tx), None))
}

/// `disconnect`: cancel the tab's token; the tab stays open.
pub fn activate_disconnect(tab_id: Option<i64>) -> DisconnectAction {
    let Some(id) = tab_id else {
        return DisconnectAction::NoLiveTab { tab_id: None };
    };
    let Some(tab) = live_tab(id) else {
        return DisconnectAction::NoLiveTab { tab_id: Some(id) };
    };
    tab.cancel.cancel();
    set_tab_state(id, TransportState::Disconnected);
    DisconnectAction::Cancelled { tab_id: id }
}

/// Forget everything and restore the default settings (tests).
pub fn reset_registry() {
    lock(&LIVE_TABS).clear();
    lock(&RUNNING).clear();
    install_settings(true, true);
}

/// Cancel and forget a closing tab, so no reconnect loop outlives it.
pub fn teardown_tab(tab_id: i64) {
    if let Some(tab) = live_tab(tab_id) {
        tab.cancel.cancel();
    }
    unregister_live_tab(tab_id);
    lock(&RUNNING).remove(&tab_id);
}

/// Close a tab, asking first when its transport is live
/// ([`should_show_close_warning`]); `close` removes the page. Closing with
/// "Do not show me again" ticked silences the warning in `store`.
pub fn request_close(
    tab_id: i64,
    parent: Option<&gtk4::Window>,
    store: Option<Rc<SessionStore>>,
    close: impl FnOnce() + 'static,
) {
    let close = move || {
        teardown_tab(tab_id);
        close();
    };
    if should_show_close_warning(tab_id, store.as_deref()) {
        confirm_with_opt_out(
            parent,
            "Close this session?",
            CONFIRM_CLOSE_DETAIL,
            "Close",
            move |silence| {
                if let Some(store) = store.filter(|_| silence) {
                    if let Err(err) = store.set_setting(CONFIRM_CLOSE_NOTICE_SETTING, "0") {
                        eprintln!(
                            "Wayterm: failed to save '{CONFIRM_CLOSE_NOTICE_SETTING}': {err}"
                        );
                    }
                }
                close();
            },
        );
    } else {
        close();
    }
}

/// A prompt-mode responder showing an async dialog on the GTK thread and
/// answering through a oneshot, so the driver never blocks on GTK.
pub fn reconnect_prompt(host: String) -> ConfirmFn {
    Arc::new(move |attempt: u32| {
        let host = host.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        gtk4::glib::MainContext::default().invoke(move || {
            let dialog = gtk4::AlertDialog::builder()
                .message("Connection lost")
                .detail(format!("Reconnect to {host}? (attempt {attempt})"))
                .modal(true)
                .build();
            dialog.set_buttons(&["No", "Reconnect"]);
            dialog.set_cancel_button(0);
            dialog.set_default_button(1);
            dialog.choose(gtk4::Window::NONE, gio::Cancellable::NONE, move |answer| {
                let _ = tx.send(matches!(answer, Ok(1)));
            });
        });
        Box::pin(async move { rx.await.unwrap_or(false) })
    })
}

/// Add `win.reconnect` and `win.disconnect`, acting on the tab that
/// `current_tab_id` names. They are added to the window itself: a
/// separate "win" action group would hide every other `win.*` action.
pub fn install_connection_actions<F>(window: &gtk4::ApplicationWindow, current_tab_id: F)
where
    F: Fn() -> Option<i64> + Clone + 'static,
{
    let reconnect = gio::SimpleAction::new(RECONNECT_ACTION, None);
    let current = current_tab_id.clone();
    reconnect.connect_activate(move |_, _| {
        let prompt = (!reconnect_on_drop_setting()).then(|| reconnect_prompt("the session".into()));
        if let ReconnectAction::NoLiveTab { .. } = activate_reconnect(current(), None, prompt) {
            eprintln!("Wayterm: reconnect: this tab has no live transport");
        }
    });
    window.add_action(&reconnect);

    let disconnect = gio::SimpleAction::new(DISCONNECT_ACTION, None);
    disconnect.connect_activate(move |_, _| {
        if let DisconnectAction::NoLiveTab { .. } = activate_disconnect(current_tab_id()) {
            eprintln!("Wayterm: disconnect: this tab has no live transport");
        }
    });
    window.add_action(&disconnect);
}

#[cfg(test)]
mod tests;
