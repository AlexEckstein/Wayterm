//! System tray icon + disconnect notifications (doc 10 Phase 4).
//!
//! Standard + crate choice are researched in `docs/tray-research.md`:
//! StatusNotifierItem (SNI) over D-Bus via the pure-Rust [`ksni`] crate
//! (tokio-native, no system libs), notifications via [`gio::Notification`]
//! (zero new deps, portal-friendly).
//!
//! The module is split in two:
//!
//! * **Display-free core** (pure functions + an async forwarder, all unit
//!   tested in `tests/tray.rs`): [`TrayAction`] mapping, [`DisconnectAlert`]
//!   formatting, [`triage_transport_event`], [`forward_disconnect_alerts`],
//!   and the `minimize_to_tray` setting logic ([`parse_setting_enabled`],
//!   [`minimize_to_tray_enabled`], [`should_minimize_to_tray`]).
//! * **Display-bound shell** (manual verification only — see the
//!   doc-comment on [`init_tray`]): [`WaytermTray`] (the ksni item),
//!   [`init_tray`], [`register_actions`], [`handle_app_action`],
//!   [`install_close_handler`], [`send_disconnect_notification`].
//!
//! Fallback contract: every display-bound entry point degrades to "no
//! tray" without panicking. [`init_tray`] returns
//! `Err(`[`TrayInitError`]`)` when D-Bus is unreachable (headless CI,
//! containers); the app then keeps its default quit-on-close behavior
//! ([`should_minimize_to_tray`] is false whenever `tray_available` is
//! false, regardless of the setting).

// Seam allowances: the bin target compiles this module tree privately,
// so the display-free core (exercised by `tests/tray.rs` through the lib
// target) and the not-yet-live-wired notifier read as dead code there.
// Same convention as `store`'s and `transport`'s `#[allow(dead_code)]`
// seams. Remove once live tabs call the forwarder + notifier directly.
#![allow(dead_code)]

use std::cell::RefCell;

use gtk4::glib;
use gtk4::prelude::*;
use ksni::TrayMethods as _;
use tokio::sync::mpsc;

use crate::store::SessionStore;
use crate::transport::types::TransportEvent;

// ---------------------------------------------------------------------------
// Display-free core
// ---------------------------------------------------------------------------

/// Settings-table key for minimize-to-tray (default ON when absent).
///
/// Deliberately NOT added to the `db.rs` seed rows: the 10-row seed is
/// pinned by doc 06 and its tests. A missing row means ON (fail-open).
pub const MINIMIZE_TO_TRAY_KEY: &str = "minimize_to_tray";

/// Desktop notification title for all Wayterm alerts.
pub const NOTIFICATION_TITLE: &str = "Wayterm";

/// SNI id: unique + stable across sessions, per the spec.
pub const TRAY_ID: &str = crate::app::APP_ID;

/// Freedesktop icon name for the tray (always in hicolor; installing a
/// `com.alex-eckstein.wayterm` icon is Phase 9 packaging work).
pub const TRAY_ICON_NAME: &str = "utilities-terminal";

/// Actions reachable from the tray menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    /// Present / un-hide the main window.
    ShowWindow,
    /// Present the main window so the user can pick a session.
    NewSession,
    /// Quit the application.
    Quit,
}

impl TrayAction {
    /// Human-readable menu label.
    pub fn label(self) -> &'static str {
        match self {
            Self::ShowWindow => "Open Wayterm",
            Self::NewSession => "New Session",
            Self::Quit => "Quit",
        }
    }
}

/// The tray menu in display order: Open, New Session, separator, Quit.
///
/// The `bool` marks separators so tests can pin the layout without GTK.
pub fn tray_menu_layout() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Item(TrayAction::ShowWindow),
        MenuEntry::Item(TrayAction::NewSession),
        MenuEntry::Separator,
        MenuEntry::Item(TrayAction::Quit),
    ]
}

/// One row of the tray menu, without any GTK/D-Bus types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuEntry {
    Item(TrayAction),
    Separator,
}

/// Parse a `settings` value for `minimize_to_tray`.
///
/// Fail-open: anything but an explicit opt-out (`0`/`false`/`no`/`off`,
/// case-insensitive, trimmed) enables minimize-to-tray. `None` (row
/// absent) is the default-ON case.
pub fn parse_setting_enabled(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Read the `minimize_to_tray` setting; DB errors fail open (ON), same as
/// a missing row — a tray preference must never break startup.
pub fn minimize_to_tray_enabled(store: &SessionStore) -> bool {
    match store.get_setting(MINIMIZE_TO_TRAY_KEY) {
        Ok(value) => parse_setting_enabled(value.as_deref()),
        Err(_) => true,
    }
}

/// Close-request decision: minimize (hide) only when the tray actually
/// initialized AND the setting is on. Any init failure → quit, i.e. the
/// pre-tray behavior is preserved exactly.
pub fn should_minimize_to_tray(tray_available: bool, setting_enabled: bool) -> bool {
    tray_available && setting_enabled
}

/// A desktop notification ready to send.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisconnectAlert {
    pub title: String,
    pub body: String,
    /// Stable per-session id so repeated alerts replace, not stack.
    pub notification_id: String,
}

/// Build the disconnect alert for `session_name`.
///
/// Body contract (task P4-5): `"<session> connection lost"`.
pub fn disconnect_alert(session_name: &str) -> DisconnectAlert {
    DisconnectAlert {
        title: NOTIFICATION_TITLE.to_string(),
        body: format!("{session_name} connection lost"),
        notification_id: format!("wayterm-disconnect-{session_name}"),
    }
}

/// Triage one transport event: only [`TransportEvent::ConnectionLost`]
/// produces an alert; data/state/error traffic is ignored (status bar and
/// reconnect flows own those paths).
pub fn triage_transport_event(
    session_name: &str,
    event: &TransportEvent,
) -> Option<DisconnectAlert> {
    match event {
        TransportEvent::ConnectionLost => Some(disconnect_alert(session_name)),
        _ => None,
    }
}

/// Forward [`TransportEvent::ConnectionLost`] from a tab's state channel
/// (doc 04 §Wiring "second glib channel", taken via
/// `TerminalTab::take_state_rx`) to `on_alert`.
///
/// Display-free: `on_alert` is where the display-bound
/// [`send_disconnect_notification`] plugs in (via a main-thread hop).
/// Ends when the sender is dropped (tab closed).
pub async fn forward_disconnect_alerts(
    session_name: String,
    mut state_rx: mpsc::UnboundedReceiver<TransportEvent>,
    on_alert: impl Fn(DisconnectAlert),
) {
    while let Some(event) = state_rx.recv().await {
        if let Some(alert) = triage_transport_event(&session_name, &event) {
            on_alert(alert);
        }
    }
}

// ---------------------------------------------------------------------------
// Display-bound shell (manual verification — see `init_tray`)
// ---------------------------------------------------------------------------

/// Failure to bring up the tray. Never fatal: the caller falls back to
/// quit-on-close.
#[derive(Debug, thiserror::Error)]
pub enum TrayInitError {
    /// No Tokio runtime for the ksni service.
    #[error("tray runtime failed: {0}")]
    Runtime(#[from] std::io::Error),
    /// D-Bus unreachable or SNI registration refused — the headless-CI
    /// branch.
    #[error("tray D-Bus service failed: {0}")]
    Spawn(#[from] ksni::Error),
}

/// Wake channel from ksni menu tasks to the GTK main thread.
///
/// ksni invokes menu closures on its own async tasks while GTK objects
/// (incl. `gtk4::Application`, which is `!Send`) may only be touched on
/// the main thread. The bridge is a std mpsc queue plus
/// `MainContext::invoke` (thread-safe, `FnOnce() + Send`): `fire` queues
/// the action and invokes a capturable-free closure; the closure runs on
/// the main thread, where the app handle + receiver live in a
/// main-thread-local installed by [`init_tray`]. Both bridge halves are
/// `Send`, so the tray struct satisfies ksni's `Tray: Send` bound with no
/// GTK types crossing threads.
struct ActionBridge {
    queue: std::sync::mpsc::Sender<TrayAction>,
}

impl ActionBridge {
    fn fire(&self, action: TrayAction) {
        // Queue first so the invoked closure never wakes to an empty
        // queue; a failed send means the UI is gone, in which case the
        // invoke is a harmless no-op.
        if self.queue.send(action).is_ok() {
            glib::MainContext::default().invoke(drain_queued_actions);
        }
    }
}

// Main-thread dispatch installed by [`init_tray`].
//
// `RefCell` (not `Mutex`): only ever touched on the GTK main thread —
// written once at init, read from `invoke`d closures. Never crosses
// threads, so `!Sync` is fine.
thread_local! {
    static TRAY_DISPATCH: RefCell<
        Option<(gtk4::Application, std::sync::mpsc::Receiver<TrayAction>)>,
    > = const { RefCell::new(None) };
}

/// Runs on the GTK main thread via `MainContext::invoke`.
fn drain_queued_actions() {
    TRAY_DISPATCH.with(|cell| {
        if let Some((app, rx)) = cell.borrow().as_ref() {
            while let Ok(action) = rx.try_recv() {
                handle_app_action(app, action);
            }
        }
    });
}

/// Owns the tray's Tokio runtime + ksni service handle.
///
/// MANUAL VERIFICATION:
///
/// - `xvfb-run -a timeout 10 cargo run` must exit 124 (timeout-kill, i.e.
///   the app ran) with no panic — with or without a session bus.
/// - On a real desktop with an SNI host (Plasma panel, GNOME +
///   AppIndicator/Status-Tray extension), check: icon visible; left-click
///   restores the window; menu shows Open / New Session / Quit; closing
///   the window hides it (setting ON); `Quit` exits; disconnecting a
///   session posts a "Wayterm / `<session>` connection lost" notification.
pub struct TrayHandle {
    // Held only for lifetime: dropping the runtime shuts down the ksni
    // service, dropping the handle unregisters the SNI item.
    _runtime: tokio::runtime::Runtime,
    _handle: ksni::Handle<WaytermTray>,
}

/// The ksni status-notifier item. Menu/activate callbacks run on ksni
/// tasks (NOT the GTK main thread), so they only push through the
/// [`ActionBridge`] — never touch widgets.
pub struct WaytermTray {
    bridge: ActionBridge,
}

impl WaytermTray {
    fn fire(&self, action: TrayAction) {
        self.bridge.fire(action);
    }
}

impl ksni::Tray for WaytermTray {
    fn id(&self) -> String {
        TRAY_ID.to_string()
    }

    fn title(&self) -> String {
        NOTIFICATION_TITLE.to_string()
    }

    fn icon_name(&self) -> String {
        TRAY_ICON_NAME.to_string()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let item = |action: TrayAction| ksni::menu::StandardItem {
            label: action.label().to_string(),
            activate: Box::new(move |tray: &mut Self| tray.fire(action)),
            ..Default::default()
        };
        vec![
            item(TrayAction::ShowWindow).into(),
            item(TrayAction::NewSession).into(),
            ksni::MenuItem::Separator,
            item(TrayAction::Quit).into(),
        ]
    }

    /// Left-click restores the window (same as "Open Wayterm").
    fn activate(&mut self, _x: i32, _y: i32) {
        self.fire(TrayAction::ShowWindow);
    }
}

/// Start the tray service. Call once from `build_ui` on the GTK main
/// thread: builds a dedicated single-worker Tokio runtime, block_ons the
/// D-Bus handshake (fast fail when headless), then attaches the
/// [`ActionBridge`] drain to the main loop.
///
/// `Err` = no tray; caller MUST keep quit-on-close in that case.
pub fn init_tray(app: &gtk4::Application) -> Result<TrayHandle, TrayInitError> {
    let (queue_tx, queue_rx) = std::sync::mpsc::channel::<TrayAction>();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("wayterm-tray")
        .enable_all()
        .build()?;
    let tray = WaytermTray {
        bridge: ActionBridge { queue: queue_tx },
    };
    let handle = runtime.block_on(tray.spawn())?;

    // Main-thread dispatch for the `invoke`d drain: app + queue stay on
    // this thread for the process lifetime, so no `Send` is required.
    TRAY_DISPATCH.set(Some((app.clone(), queue_rx)));

    Ok(TrayHandle {
        _runtime: runtime,
        _handle: handle,
    })
}

/// GAction dispatch. `ShowWindow` un-hides (close-to-tray state) and
/// presents; `NewSession` presents the window where the session tree owns
/// session creation (full new-session dialog wiring is a follow-up once
/// `MainWindow` exposes it — this stays additive); `Quit` exits.
pub fn handle_app_action(app: &gtk4::Application, action: TrayAction) {
    match action {
        TrayAction::ShowWindow | TrayAction::NewSession => {
            if let Some(window) = app.active_window() {
                window.set_visible(true);
                window.present();
            }
        }
        TrayAction::Quit => {
            app.quit();
        }
    }
}

/// Install close-to-tray on `window`.
///
/// When `should_minimize_to_tray(tray_available, setting_enabled)` the
/// close request hides the window and stops the event; otherwise the
/// event proceeds (existing quit-on-close). Intentionally installed from
/// `app.rs`, not `main_window.rs` (owned by another P4 task).
pub fn install_close_handler(
    window: &gtk4::ApplicationWindow,
    tray_available: bool,
    setting_enabled: bool,
) {
    window.connect_close_request(move |win| {
        if should_minimize_to_tray(tray_available, setting_enabled) {
            win.set_visible(false);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

/// Post a disconnect notification for `session_name` on the main thread.
///
/// Must be called on the GTK main thread (the [`forward_disconnect_alerts`]
/// `on_alert` is expected to hop there, e.g. via `glib::idle_add` or a
/// `MainContext` invoke).
pub fn send_disconnect_notification(app: &gtk4::Application, session_name: &str) {
    let alert = disconnect_alert(session_name);
    let notification = gio::Notification::new(&alert.title);
    notification.set_body(Some(&alert.body));
    app.send_notification(Some(&alert.notification_id), &notification);
}
