//! Tray icon and disconnect notifications (doc 10 Phase 4).
//!
//! The icon is a StatusNotifierItem over D-Bus via `ksni` (pure Rust,
//! tokio-native); notifications go through `gio::Notification`. Without a
//! session bus or a tray host (headless CI, GNOME without the AppIndicator
//! extension) [`init_tray`] fails and closing the window simply quits.
//!
//! Manual check: `xvfb-run -a timeout 10 cargo run` exits 124 without a
//! panic. On a desktop with a tray: left-click restores the window, the
//! menu offers Open / New Session / Quit, closing hides the window (when
//! `minimize_to_tray` is on), and a dropped session posts
//! "`<session>` connection lost".

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;

use gtk4::glib;
use gtk4::prelude::*;
use ksni::TrayMethods as _;
use tokio::sync::mpsc;

use crate::store::SessionStore;
use crate::transport::types::TransportEvent;

/// Settings row; a missing row means on (not seeded).
pub const MINIMIZE_TO_TRAY_KEY: &str = "minimize_to_tray";
pub const NOTIFICATION_TITLE: &str = "Wayterm";
/// The SNI id: unique and stable.
pub const TRAY_ID: &str = crate::app::APP_ID;
/// Always present in hicolor; a Wayterm icon is packaging work (Phase 9).
pub const TRAY_ICON_NAME: &str = "utilities-terminal";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    /// Un-hide and present the main window.
    ShowWindow,
    /// Present the window, where the session tree creates sessions.
    NewSession,
    Quit,
}

impl TrayAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::ShowWindow => "Open Wayterm",
            Self::NewSession => "New Session",
            Self::Quit => "Quit",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuEntry {
    Item(TrayAction),
    Separator,
}

/// The tray menu, top to bottom.
pub fn tray_menu_layout() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Item(TrayAction::ShowWindow),
        MenuEntry::Item(TrayAction::NewSession),
        MenuEntry::Separator,
        MenuEntry::Item(TrayAction::Quit),
    ]
}

/// On unless explicitly off (see [`crate::store::db::flag_enabled`]).
pub fn minimize_to_tray_enabled(store: &SessionStore) -> bool {
    store.flag(MINIMIZE_TO_TRAY_KEY)
}

/// Hide instead of quitting only when a tray exists to bring it back.
pub fn should_minimize_to_tray(tray_available: bool, setting_enabled: bool) -> bool {
    tray_available && setting_enabled
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisconnectAlert {
    pub title: String,
    pub body: String,
    /// Per session, so repeated alerts replace each other.
    pub notification_id: String,
}

pub fn disconnect_alert(session_name: &str) -> DisconnectAlert {
    DisconnectAlert {
        title: NOTIFICATION_TITLE.to_string(),
        body: format!("{session_name} connection lost"),
        notification_id: format!("wayterm-disconnect-{session_name}"),
    }
}

/// Only `ConnectionLost` alerts; everything else belongs to the status bar.
pub fn triage_transport_event(
    session_name: &str,
    event: &TransportEvent,
) -> Option<DisconnectAlert> {
    matches!(event, TransportEvent::ConnectionLost).then(|| disconnect_alert(session_name))
}

/// Call `on_alert` for each `ConnectionLost` on a tab's state channel,
/// until the channel closes. `on_alert` must hop to the main thread before
/// calling [`send_disconnect_notification`].
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

/// A tab's alert channel: every `ConnectionLost` sent on it posts the
/// disconnect notification, until the last sender is dropped. The
/// forwarder runs on the main context, so posting needs no hop. Call on
/// the main thread.
pub fn attach_disconnect_alerts(
    app: &gtk4::Application,
    session_name: String,
) -> mpsc::UnboundedSender<TransportEvent> {
    let (alert_tx, alert_rx) = mpsc::unbounded_channel();
    let app = app.clone();
    glib::MainContext::default().spawn_local(forward_disconnect_alerts(
        session_name,
        alert_rx,
        move |alert| post_disconnect_alert(&app, &alert),
    ));
    alert_tx
}

/// Never fatal: the caller falls back to quit-on-close.
#[derive(Debug, thiserror::Error)]
pub enum TrayInitError {
    #[error("tray runtime failed: {0}")]
    Runtime(#[from] std::io::Error),
    /// No D-Bus, or no StatusNotifierWatcher.
    #[error("tray D-Bus service failed: {0}")]
    Spawn(#[from] ksni::Error),
}

// ksni calls the menu on its own tasks, but the `gtk4::Application` may
// only be touched on the main thread. Menu clicks are queued and drained
// on the main thread, where the app and the receiver live.
thread_local! {
    static TRAY_DISPATCH: RefCell<Option<(gtk4::Application, std_mpsc::Receiver<TrayAction>)>> =
        const { RefCell::new(None) };
}

fn drain_queued_actions() {
    TRAY_DISPATCH.with(|cell| {
        if let Some((app, rx)) = cell.borrow().as_ref() {
            while let Ok(action) = rx.try_recv() {
                handle_app_action(app, action);
            }
        }
    });
}

/// Keeps the tray's runtime and D-Bus service alive; drop it to remove
/// the icon.
pub struct TrayHandle {
    _runtime: tokio::runtime::Runtime,
    _handle: ksni::Handle<WaytermTray>,
}

/// The status-notifier item.
pub struct WaytermTray {
    queue: std_mpsc::Sender<TrayAction>,
}

impl WaytermTray {
    fn fire(&self, action: TrayAction) {
        if self.queue.send(action).is_ok() {
            glib::MainContext::default().invoke(drain_queued_actions);
        }
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
        tray_menu_layout()
            .into_iter()
            .map(|entry| match entry {
                MenuEntry::Separator => ksni::MenuItem::Separator,
                MenuEntry::Item(action) => ksni::menu::StandardItem {
                    label: action.label().to_string(),
                    activate: Box::new(move |tray: &mut Self| tray.fire(action)),
                    ..Default::default()
                }
                .into(),
            })
            .collect()
    }

    /// Left-click restores the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.fire(TrayAction::ShowWindow);
    }
}

/// Start the tray. Call once on the main thread; it runs the D-Bus
/// handshake on a dedicated one-worker runtime, failing fast when headless.
pub fn init_tray(app: &gtk4::Application) -> Result<TrayHandle, TrayInitError> {
    let (queue, queue_rx) = std_mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("wayterm-tray")
        .enable_all()
        .build()?;
    let handle = runtime.block_on(WaytermTray { queue }.spawn())?;
    TRAY_DISPATCH.set(Some((app.clone(), queue_rx)));
    Ok(TrayHandle {
        _runtime: runtime,
        _handle: handle,
    })
}

pub fn handle_app_action(app: &gtk4::Application, action: TrayAction) {
    match action {
        TrayAction::ShowWindow | TrayAction::NewSession => {
            if let Some(window) = app.active_window() {
                window.set_visible(true);
                window.present();
            }
        }
        TrayAction::Quit => app.quit(),
    }
}

/// Hide on close instead of quitting, when the tray allows it. The setting
/// is read at each close, so a change in Settings applies at once.
pub fn install_close_handler(
    window: &gtk4::ApplicationWindow,
    tray_available: bool,
    store: Rc<SessionStore>,
) {
    window.connect_close_request(move |window| {
        if should_minimize_to_tray(tray_available, minimize_to_tray_enabled(&store)) {
            window.set_visible(false);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

/// Post the disconnect notification. Main thread only.
pub fn send_disconnect_notification(app: &gtk4::Application, session_name: &str) {
    post_disconnect_alert(app, &disconnect_alert(session_name));
}

fn post_disconnect_alert(app: &gtk4::Application, alert: &DisconnectAlert) {
    let notification = gio::Notification::new(&alert.title);
    notification.set_body(Some(&alert.body));
    app.send_notification(Some(&alert.notification_id), &notification);
}
