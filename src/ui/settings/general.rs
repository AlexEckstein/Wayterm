//! The General settings page: on/off switches for the doc 06 settings rows
//! the app acts on, and the experimental terminal backend choice. Every
//! flip is saved at once and applied live.
//!
//! [`TOGGLES`] and the functions over it are display-free; [`page`] only
//! renders them. A missing row reads as on
//! ([`flag_enabled`](crate::store::db::flag_enabled)), so the page shows what
//! the app does even for a row that was never written.
//!
//! "Clear all reminders" turns back on every notice the user silenced with
//! "Do not show me again" ([`REMINDERS`]).
//!
//! The "Experimental Ghostty terminal" check box writes `default_backend`
//! (`ghostty`/`vte`, doc 06) for tabs opened afterwards. In a build without
//! the `backend-ghostty` feature it is shown disabled, and VTE is used
//! whatever the row says.

use std::rc::Rc;

use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::terminal::{BackendKind, DEFAULT_BACKEND_SETTING, GHOSTTY_COMPILED};
use crate::ui::{sftp_sidebar, tab, tab_resilience, tray};

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// One switch: a settings row and the text beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Toggle {
    pub key: &'static str,
    pub title: &'static str,
    /// What the switch does, and what "off" means.
    pub subtitle: &'static str,
}

/// The switches, in page order.
pub const TOGGLES: [Toggle; 4] = [
    Toggle {
        key: tab_resilience::CONFIRM_CLOSE_SETTING,
        title: "Confirm before closing a connected tab",
        subtitle: "Ask before closing a tab whose connection is live.",
    },
    Toggle {
        key: tab_resilience::RECONNECT_ON_DROP_SETTING,
        title: "Reconnect automatically",
        subtitle: "Retry a dropped connection on its own. Off: ask before each attempt.",
    },
    Toggle {
        key: sftp_sidebar::SFTP_ON_CONNECT_SETTING,
        title: "Open SFTP on connect",
        subtitle: "Switch the side panel to SFTP when an SSH session connects. Off: \
                   no SFTP channel opens until you view that tab's SFTP panel.",
    },
    Toggle {
        key: tray::MINIMIZE_TO_TRAY_KEY,
        title: "Minimize to tray on close",
        subtitle: "Closing the window hides it in the system tray. Without a tray, \
                   closing quits.",
    },
];

/// Every toggle with its stored value, in page order.
pub fn values(store: &SessionStore) -> Vec<(Toggle, bool)> {
    TOGGLES
        .iter()
        .map(|toggle| (*toggle, store.flag(toggle.key)))
        .collect()
}

/// Store one switch as `1`/`0`; [`apply`] makes it take effect.
pub fn save(store: &SessionStore, key: &str, on: bool) -> rusqlite::Result<()> {
    store.set_setting(key, if on { "1" } else { "0" })
}

/// The settings rows behind every "Do not show me again" check box; a new
/// notice adds its row here so "Clear all reminders" covers it.
pub const REMINDERS: [&str; 1] = [tab::TMUX_NOTICE_SETTING];

pub const CLEAR_REMINDERS_TITLE: &str = "Clear all reminders";
pub const CLEAR_REMINDERS_SUBTITLE: &str =
    "Show again every notice you turned off with \"Do not show me again\".";

/// Whether any reminder is silenced (the Clear button has work to do).
pub fn any_reminder_silenced(store: &SessionStore) -> bool {
    REMINDERS.iter().any(|key| !store.flag(key))
}

/// Turn every reminder back on.
pub fn clear_reminders(store: &SessionStore) -> rusqlite::Result<()> {
    REMINDERS.iter().try_for_each(|key| save(store, key, true))
}

/// The check box's text.
pub const GHOSTTY_TITLE: &str = "Experimental Ghostty terminal";
pub const GHOSTTY_SUBTITLE: &str = "Draw new tabs with libghostty-vt instead of VTE. \
                                    Experimental: no search or color schemes yet. \
                                    Open tabs keep their terminal.";
/// The tooltip of the disabled check box in a build without the backend.
pub const GHOSTTY_UNAVAILABLE: &str = "rebuild with the backend-ghostty feature to enable";

/// The stored `default_backend`; missing or unreadable is VTE.
pub fn backend(store: &SessionStore) -> BackendKind {
    BackendKind::from_setting(store.setting(DEFAULT_BACKEND_SETTING).as_deref())
}

/// Store the backend for new tabs; [`apply`] makes it take effect.
pub fn save_backend(store: &SessionStore, kind: BackendKind) -> rusqlite::Result<()> {
    store.set_setting(DEFAULT_BACKEND_SETTING, kind.as_setting())
}

/// Push the stored values to the modules that act on them, at startup and
/// after every change. `minimize_to_tray` needs no push: the close handler
/// reads it at each close.
pub fn apply(store: &SessionStore) {
    tab_resilience::install_settings(
        store.flag(tab_resilience::CONFIRM_CLOSE_SETTING),
        store.flag(tab_resilience::RECONNECT_ON_DROP_SETTING),
    );
    sftp_sidebar::install_settings(store.flag(sftp_sidebar::SFTP_ON_CONNECT_SETTING));
    crate::terminal::install_backend_preference(backend(store));
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// The page widget: a heading over one row per toggle and the reminders
/// reset, then the backend.
pub fn page(store: Rc<SessionStore>) -> gtk4::Widget {
    let behavior = super::boxed_list();
    for (toggle, on) in values(&store) {
        behavior.append(&toggle_row(toggle, on, &store));
    }
    behavior.append(&clear_reminders_row(&store));
    let terminal_heading = super::heading("Terminal");
    terminal_heading.set_margin_top(12);
    let terminal = super::boxed_list();
    terminal.append(&backend_row(&store));
    super::scrolling_page(&[
        super::heading("Behavior").upcast_ref(),
        behavior.upcast_ref(),
        terminal_heading.upcast_ref(),
        terminal.upcast_ref(),
    ])
}

/// title over subtitle | switch.
fn toggle_row(toggle: Toggle, on: bool, store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let switch = gtk4::Switch::builder()
        .active(on)
        .valign(gtk4::Align::Center)
        .build();
    let text = super::described(toggle.title, Some(toggle.subtitle), &switch);
    let store = Rc::clone(store);
    switch.connect_active_notify(move |switch| {
        if let Err(err) = save(&store, toggle.key, switch.is_active()) {
            eprintln!("Wayterm: failed to save setting '{}': {err}", toggle.key);
            return;
        }
        apply(&store);
    });
    super::row(&[text.upcast_ref(), switch.upcast_ref()])
}

/// title over subtitle | Clear. Insensitive while no reminder is silenced.
fn clear_reminders_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let button = gtk4::Button::builder()
        .label("Clear")
        .valign(gtk4::Align::Center)
        .sensitive(any_reminder_silenced(store))
        .build();
    let text = super::described(
        CLEAR_REMINDERS_TITLE,
        Some(CLEAR_REMINDERS_SUBTITLE),
        &button,
    );
    let store = Rc::clone(store);
    button.connect_clicked(move |button| {
        if let Err(err) = clear_reminders(&store) {
            eprintln!("Wayterm: failed to clear reminders: {err}");
            return;
        }
        button.set_sensitive(false);
    });
    super::row(&[text.upcast_ref(), button.upcast_ref()])
}

/// check box | title over subtitle. Disabled, with a tooltip, in a build
/// without the backend; then it shows VTE, the backend actually used.
fn backend_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let check = gtk4::CheckButton::builder()
        .active(backend(store).effective() == BackendKind::Ghostty)
        .valign(gtk4::Align::Center)
        .sensitive(GHOSTTY_COMPILED)
        .build();
    let text = super::described(GHOSTTY_TITLE, Some(GHOSTTY_SUBTITLE), &check);
    let store = Rc::clone(store);
    check.connect_toggled(move |check| {
        let kind = if check.is_active() {
            BackendKind::Ghostty
        } else {
            BackendKind::Vte
        };
        if let Err(err) = save_backend(&store, kind) {
            eprintln!("Wayterm: failed to save setting '{DEFAULT_BACKEND_SETTING}': {err}");
            return;
        }
        apply(&store);
    });
    let row = super::row(&[check.upcast_ref(), text.upcast_ref()]);
    if !GHOSTTY_COMPILED {
        if let Some(line) = row.child() {
            line.set_tooltip_text(Some(GHOSTTY_UNAVAILABLE));
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::backend_preference;

    /// Clearing turns every silenced reminder back on.
    #[test]
    fn clearing_reminders_turns_them_all_back_on() {
        let store = SessionStore::in_memory().unwrap();
        assert!(
            !any_reminder_silenced(&store),
            "a fresh store shows them all"
        );

        for key in REMINDERS {
            save(&store, key, false).unwrap();
        }
        assert!(any_reminder_silenced(&store));

        clear_reminders(&store).unwrap();
        assert!(!any_reminder_silenced(&store));
        assert!(REMINDERS.iter().all(|key| store.flag(key)));
    }

    /// The seeded row reads as VTE; a saved choice round-trips and `apply`
    /// installs it for `create_terminal`.
    #[test]
    fn backend_choice_round_trips_and_applies() {
        let store = SessionStore::in_memory().unwrap();
        assert_eq!(backend(&store), BackendKind::Vte);

        save_backend(&store, BackendKind::Ghostty).unwrap();
        assert_eq!(
            store
                .get_setting(DEFAULT_BACKEND_SETTING)
                .unwrap()
                .as_deref(),
            Some("ghostty")
        );
        assert_eq!(backend(&store), BackendKind::Ghostty);
        apply(&store);
        assert_eq!(backend_preference(), BackendKind::Ghostty);

        save_backend(&store, BackendKind::Vte).unwrap();
        apply(&store);
        assert_eq!(backend_preference(), BackendKind::Vte);
    }
}
