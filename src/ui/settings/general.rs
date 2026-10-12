//! The General settings page: on/off switches for the doc 06 settings rows
//! the app acts on. Every flip is saved at once and applied live.
//!
//! [`TOGGLES`] and the functions over it are display-free; [`page`] only
//! renders them. A missing row reads as on
//! ([`flag_enabled`](crate::store::db::flag_enabled)), so the page shows what
//! the app does even for a row that was never written.

use std::rc::Rc;

use gtk4::prelude::*;

use crate::store::SessionStore;
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

/// Push the stored values to the modules that act on them, at startup and
/// after every change. `minimize_to_tray` needs no push: the close handler
/// reads it at each close.
pub fn apply(store: &SessionStore) {
    tab_resilience::install_settings(
        store.flag(tab_resilience::CONFIRM_CLOSE_SETTING),
        store.flag(tab_resilience::RECONNECT_ON_DROP_SETTING),
    );
    sftp_sidebar::install_settings(store.flag(sftp_sidebar::SFTP_ON_CONNECT_SETTING));
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

    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    set_margins(&column, 18);
    column.append(&heading);
    column.append(&list);

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
