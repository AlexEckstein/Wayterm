//! The General settings page: on/off switches for the doc 06 settings rows
//! the app acts on, and the experimental terminal backend choice. Every
//! flip is saved at once and applied live.
//!
//! [`TOGGLES`] and the functions over it are display-free; [`page`] only
//! renders them. A missing row reads as on
//! ([`flag_enabled`](crate::store::db::flag_enabled)), so the page shows what
//! the app does even for a row that was never written.
//!
//! The "Experimental Ghostty terminal" check box writes `default_backend`
//! (`ghostty`/`vte`, doc 06) for tabs opened afterwards. In a build without
//! the `backend-ghostty` feature it is shown disabled, and VTE is used
//! whatever the row says.

use std::rc::Rc;

use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::terminal::{BackendKind, DEFAULT_BACKEND_SETTING, GHOSTTY_COMPILED};
use crate::ui::widgets::set_margins;
use crate::ui::{sftp_sidebar, tab_resilience, tray};

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

/// The check box's text.
pub const GHOSTTY_TITLE: &str = "Experimental Ghostty terminal";
pub const GHOSTTY_SUBTITLE: &str = "Draw new tabs with libghostty-vt instead of VTE. \
                                    Experimental: no search or color schemes yet. \
                                    Open tabs keep their terminal.";
/// The tooltip of the disabled check box in a build without the backend.
pub const GHOSTTY_UNAVAILABLE: &str = "rebuild with the backend-ghostty feature to enable";

/// The stored `default_backend`; missing or unreadable is VTE.
pub fn backend(store: &SessionStore) -> BackendKind {
    let raw = store
        .get_setting(DEFAULT_BACKEND_SETTING)
        .unwrap_or_else(|err| {
            eprintln!("Wayterm: failed to read setting '{DEFAULT_BACKEND_SETTING}': {err}");
            None
        });
    BackendKind::from_setting(raw.as_deref())
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

/// The page widget: a heading over one row per toggle.
pub fn page(store: Rc<SessionStore>) -> gtk4::Widget {
    let heading = gtk4::Label::new(Some("Behavior"));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);

    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    for (toggle, on) in values(&store) {
        list.append(&build_row(toggle, on, &store));
    }

    let terminal_heading = gtk4::Label::new(Some("Terminal"));
    terminal_heading.add_css_class("heading");
    terminal_heading.set_xalign(0.0);
    terminal_heading.set_margin_top(12);
    let terminal_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    terminal_list.add_css_class("boxed-list");
    terminal_list.append(&build_backend_row(&store));

    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    set_margins(&column, 18);
    column.append(&heading);
    column.append(&list);
    column.append(&terminal_heading);
    column.append(&terminal_list);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&column)
        .vexpand(true)
        .hexpand(true)
        .build();
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled.upcast()
}

/// title over subtitle | switch.
fn build_row(toggle: Toggle, on: bool, store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let title = gtk4::Label::new(Some(toggle.title));
    title.set_xalign(0.0);
    let subtitle = gtk4::Label::new(Some(toggle.subtitle));
    subtitle.set_xalign(0.0);
    subtitle.set_wrap(true);
    subtitle.add_css_class("dim-label");

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&subtitle);

    let switch = gtk4::Switch::builder()
        .active(on)
        .valign(gtk4::Align::Center)
        .build();
    switch.update_relation(&[
        gtk4::accessible::Relation::LabelledBy(&[title.upcast_ref()]),
        gtk4::accessible::Relation::DescribedBy(&[subtitle.upcast_ref()]),
    ]);
    let store = Rc::clone(store);
    switch.connect_active_notify(move |switch| {
        if let Err(err) = save(&store, toggle.key, switch.is_active()) {
            eprintln!("Wayterm: failed to save setting '{}': {err}", toggle.key);
            return;
        }
        apply(&store);
    });

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    set_margins(&hbox, 12);
    hbox.append(&text);
    hbox.append(&switch);

    gtk4::ListBoxRow::builder()
        .child(&hbox)
        .activatable(false)
        .build()
}

/// check box | title over subtitle. Disabled, with a tooltip, in a build
/// without the backend; then it shows VTE, the backend actually used.
fn build_backend_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let title = gtk4::Label::new(Some(GHOSTTY_TITLE));
    title.set_xalign(0.0);
    let subtitle = gtk4::Label::new(Some(GHOSTTY_SUBTITLE));
    subtitle.set_xalign(0.0);
    subtitle.set_wrap(true);
    subtitle.add_css_class("dim-label");

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&subtitle);

    let check = gtk4::CheckButton::builder()
        .active(backend(store).effective() == BackendKind::Ghostty)
        .valign(gtk4::Align::Center)
        .sensitive(GHOSTTY_COMPILED)
        .build();
    check.update_relation(&[
        gtk4::accessible::Relation::LabelledBy(&[title.upcast_ref()]),
        gtk4::accessible::Relation::DescribedBy(&[subtitle.upcast_ref()]),
    ]);
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

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    set_margins(&hbox, 12);
    hbox.append(&check);
    hbox.append(&text);
    if !GHOSTTY_COMPILED {
        hbox.set_tooltip_text(Some(GHOSTTY_UNAVAILABLE));
    }

    gtk4::ListBoxRow::builder()
        .child(&hbox)
        .activatable(false)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::backend_preference;

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
