//! Tray icon and desktop notifications (doc 03 §Tray, doc 02 §Desktop
//! notifications).
//!
//! The icon is a StatusNotifierItem over gio's D-Bus ([`sni`]), served
//! on the GTK main context; notifications go through `gio::Notification`.
//! Without a session bus [`init_tray`] fails; without a tray host
//! (headless CI, GNOME without the AppIndicator extension) the item never
//! becomes available. Either way closing the window simply quits.
//!
//! Manual check: `xvfb-run -a timeout 10 cargo run` exits 124 without a
//! panic. On a desktop with a tray: left-click restores the window, the
//! menu offers Open / New Session / Quit, closing hides the window (when
//! `minimize_to_tray` is on), and a dropped session posts
//! "`<session>` connection lost".

pub mod sni;

use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{gio, glib};

use crate::store::SessionStore;

/// Settings row; a missing row means on (not seeded).
pub const MINIMIZE_TO_TRAY_KEY: &str = "minimize_to_tray";
pub const NOTIFICATION_TITLE: &str = "Wayterm";
/// The least time between one tab's terminal notifications.
pub const NOTIFICATION_INTERVAL: Duration = Duration::from_secs(1);
/// The SNI id: unique and stable.
pub const TRAY_ID: &str = crate::app::APP_ID;
/// The app icon, which the packages install into hicolor.
pub const TRAY_ICON_NAME: &str = crate::app::APP_ID;
/// The same icon as pixels, for a host that cannot find the name
/// (`cargo run`, an AppImage without desktop integration).
const TRAY_ICON_PNG: &[u8] =
    include_bytes!("../../assets/icons/hicolor/64x64/apps/com.alex-eckstein.wayterm.png");

/// An SNI pixmap: ARGB32 in network byte order, row by row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrayIcon {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// `TRAY_ICON_PNG` as an SNI pixmap.
pub fn tray_icon_pixmap() -> Result<TrayIcon, glib::Error> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(TRAY_ICON_PNG));
    let pixbuf = gtk4::gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE)?;
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let row_len = width as usize * 4;
    // Pixbuf rows are RGBA, padded to the rowstride.
    let data = pixbuf
        .read_pixel_bytes()
        .chunks(pixbuf.rowstride() as usize)
        .flat_map(|row| row[..row_len].as_chunks::<4>().0)
        .flat_map(|&[r, g, b, a]| [a, r, g, b])
        .collect();
    Ok(TrayIcon {
        width,
        height,
        data,
    })
}

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
pub struct Alert {
    pub title: String,
    pub body: String,
    /// Stable per source, so repeated alerts replace each other.
    pub notification_id: String,
}

pub fn disconnect_alert(session_name: &str) -> Alert {
    Alert {
        title: NOTIFICATION_TITLE.to_string(),
        body: format!("{session_name} connection lost"),
        notification_id: format!("wayterm-disconnect-{session_name}"),
    }
}

pub fn terminal_alert(tab_id: i64, session_name: &str, title: &str, body: &str) -> Option<Alert> {
    // The text is the remote side's: control characters out entirely
    // (a NUL would cut GDK's string short, and OSC 9 has single-line
    // bodies), then trimmed. A blank title falls back to the session's
    // name, and nothing worth saying drops the alert.
    let strip = |text: &str| {
        text.chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_string()
    };
    let body = strip(body);
    if body.is_empty() {
        return None;
    }
    let title = strip(title);
    let title = if title.is_empty() {
        session_name.to_string()
    } else {
        title
    };
    Some(Alert {
        title,
        body,
        notification_id: format!("wayterm-terminal-{tab_id}"),
    })
}

/// Only a tab the user is both looking at and has focused stays quiet.
pub fn should_notify(window_active: bool, tab_current: bool) -> bool {
    !(window_active && tab_current)
}

#[derive(Debug, Default)]
pub struct NotificationLimit {
    last: Option<Instant>,
}

impl NotificationLimit {
    /// One alert per [`NOTIFICATION_INTERVAL`]; a refused one does not
    /// push the window back.
    pub fn allow(&mut self, now: Instant) -> bool {
        let allow = match self.last {
            None => true,
            Some(last) => now.duration_since(last) >= NOTIFICATION_INTERVAL,
        };
        if allow {
            self.last = Some(now);
        }
        allow
    }
}

/// Never fatal: the caller falls back to quit-on-close.
#[derive(Debug, thiserror::Error)]
pub enum TrayInitError {
    /// No session bus, or the item could not be exported on it.
    #[error("tray D-Bus service failed: {0}")]
    Dbus(#[from] glib::Error),
}

/// Keeps the item on the bus; drop it to remove the icon.
pub struct TrayHandle {
    service: sni::Service,
}

impl TrayHandle {
    /// True while a tray host shows the icon; see [`sni::Availability`].
    pub fn availability(&self) -> sni::Availability {
        self.service.availability()
    }

    pub fn bus_name(&self) -> &str {
        self.service.bus_name()
    }
}

/// Serve the tray on `connection`, running `on_action` for each click.
/// Call on the thread whose main context will answer the calls.
pub fn start_tray(
    connection: &gio::DBusConnection,
    on_action: impl Fn(TrayAction) + 'static,
) -> Result<TrayHandle, TrayInitError> {
    // Not fatal: the icon name still works where the app is installed.
    let icon_pixmap = match tray_icon_pixmap() {
        Ok(icon) => vec![icon],
        Err(err) => {
            tracing::info!(error = %err, "tray icon pixmap unavailable");
            Vec::new()
        }
    };
    let item = sni::ItemProps {
        id: TRAY_ID.to_string(),
        title: NOTIFICATION_TITLE.to_string(),
        icon_name: TRAY_ICON_NAME.to_string(),
        icon_pixmap,
    };
    let menu = sni::Menu::new(tray_menu_layout());
    let service = sni::Service::start(connection, item, menu, Rc::new(on_action))?;
    Ok(TrayHandle { service })
}

/// Start the tray on the session bus. Call once on the main thread; it
/// fails at once without a session bus, and otherwise registers with the
/// tray host in the background (see [`TrayHandle::availability`]).
pub fn init_tray(app: &gtk4::Application) -> Result<TrayHandle, TrayInitError> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let app = app.downgrade();
    start_tray(&connection, move |action| {
        if let Some(app) = app.upgrade() {
            handle_app_action(&app, action);
        }
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

/// Hide on close instead of quitting, when the tray allows it. Both the
/// tray and the setting are read at each close, so a tray host coming or
/// going, or a change in Settings, applies at once.
pub fn install_close_handler(
    window: &gtk4::ApplicationWindow,
    tray_available: sni::Availability,
    store: Rc<SessionStore>,
) {
    window.connect_close_request(move |window| {
        if should_minimize_to_tray(tray_available.get(), minimize_to_tray_enabled(&store)) {
            window.set_visible(false);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

/// Post the disconnect notification for `session_name` (its tab's state
/// drain calls this on `ConnectionLost` only). Main thread only.
pub fn send_disconnect_notification(app: &gtk4::Application, session_name: &str) {
    send_alert(app, &disconnect_alert(session_name));
}

/// Post `alert` to the desktop. Main thread only.
pub fn send_alert(app: &gtk4::Application, alert: &Alert) {
    let notification = gio::Notification::new(&alert.title);
    notification.set_body(Some(&alert.body));
    app.send_notification(Some(&alert.notification_id), &notification);
}
