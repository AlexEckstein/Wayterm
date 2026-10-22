//! Duplicate tabs: a session opened while a tab for it is already open
//! gets a second, independent tab. Two tabs running `tmux new -A` on one
//! name would attach to the same tmux session, so the duplicate connects
//! without tmux and a notice says so (doc 04 §Tmux session persistence).

use std::rc::Rc;

use gtk4::prelude::*;

use crate::store::model::SessionConfig;
use crate::store::SessionStore;
use crate::ui::widgets::{button_row, set_margins, wrapped_label};

/// Settings row (doc 13 §Settings rows): the notice shows unless this is off.
pub const TMUX_NOTICE_SETTING: &str = "duplicate_tmux_notice";

/// The duplicate-tab notice's body text.
pub const TMUX_NOTICE_DETAIL: &str = "Multiple tabs of the same session bookmark cannot use \
     tmux. This tab connects without tmux; the first tab keeps it.";

/// The config a new tab of `session` connects with: `session` itself, or,
/// when a tab for it is `already_open`, a copy without tmux. `true` when
/// tmux was dropped.
pub fn tab_config(session: &SessionConfig, already_open: bool) -> (SessionConfig, bool) {
    let mut config = session.clone();
    let dropped = already_open && config.tmux_enabled;
    if dropped {
        config.tmux_enabled = false;
    }
    (config, dropped)
}

/// Whether the duplicate-tab notice shows (`duplicate_tmux_notice`).
pub fn notice_enabled(store: &SessionStore) -> bool {
    store.flag(TMUX_NOTICE_SETTING)
}

/// Tell the user the new tab runs without tmux. Non-blocking: the tab is
/// already connecting. "Do not show me again" turns the notice off for
/// every session.
pub fn show_tmux_notice(parent: Option<&gtk4::Window>, store: &Rc<SessionStore>) {
    let window = gtk4::Window::builder()
        .title("tmux disabled for this tab")
        .modal(true)
        .resizable(false)
        .default_width(420)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
        window.set_destroy_with_parent(true);
    }

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&content, 18);
    content.append(&wrapped_label(TMUX_NOTICE_DETAIL));
    let opt_out = gtk4::CheckButton::with_label("Do not show me again");
    content.append(&opt_out);
    let ok = gtk4::Button::with_label("OK");
    ok.add_css_class("suggested-action");
    content.append(&button_row(&[&ok]));
    window.set_child(Some(&content));
    window.set_default_widget(Some(&ok));

    let for_ok = window.clone();
    ok.connect_clicked(move |_| for_ok.close());
    let store = store.clone();
    window.connect_close_request(move |_| {
        if opt_out.is_active() {
            if let Err(err) = store.set_setting(TMUX_NOTICE_SETTING, "0") {
                eprintln!("Wayterm: failed to save '{TMUX_NOTICE_SETTING}': {err}");
            }
        }
        gtk4::glib::Propagation::Proceed
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux_session() -> SessionConfig {
        SessionConfig {
            id: 9,
            name: "box".into(),
            tmux_enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_first_tab_keeps_tmux() {
        let (config, dropped) = tab_config(&tmux_session(), false);
        assert!(config.tmux_enabled);
        assert!(!dropped);
    }

    #[test]
    fn a_duplicate_drops_tmux() {
        let (config, dropped) = tab_config(&tmux_session(), true);
        assert!(!config.tmux_enabled);
        assert_eq!(config.tmux_session_suffix(), None);
        assert!(dropped);
    }

    #[test]
    fn a_duplicate_without_tmux_needs_no_notice() {
        let session = SessionConfig {
            tmux_enabled: false,
            ..tmux_session()
        };
        let (config, dropped) = tab_config(&session, true);
        assert!(!config.tmux_enabled);
        assert!(!dropped);
    }

    #[test]
    fn opting_out_silences_the_notice() {
        let store = SessionStore::in_memory().expect("in-memory store");
        assert!(notice_enabled(&store), "on until the user opts out");
        store.set_setting(TMUX_NOTICE_SETTING, "0").expect("save");
        assert!(!notice_enabled(&store));
    }
}
