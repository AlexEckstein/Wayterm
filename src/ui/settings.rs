//! The Settings window: a sidebar of pages beside the selected one —
//! [`general`], [`credentials`] (doc 11), and Keyboard Shortcuts
//! ([`hotkeys`], doc 07). Every page
//! saves each change at once and applies it live; there is no Apply button.

pub mod credentials;
pub mod general;
pub mod hotkeys;

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use crate::hotkeys::HotkeyEngine;
use crate::store::SessionStore;

/// Stack names of the pages, in sidebar order.
pub const GENERAL_PAGE: &str = "general";
pub const CREDENTIALS_PAGE: &str = "credentials";
pub const SHORTCUTS_PAGE: &str = "keyboard-shortcuts";

/// The settings window (unpresented), opened on General.
pub fn create_settings_window(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
) -> gtk4::Window {
    let window = gtk4::Window::builder()
        .title("Settings")
        .default_width(820)
        .default_height(560)
        .modal(false)
        .build();

    let header = gtk4::HeaderBar::new();
    // The Close button is the only way out; no duplicate window X.
    header.set_show_title_buttons(false);
    let close_button = gtk4::Button::with_label("Close");
    {
        let window = window.clone();
        close_button.connect_clicked(move |_| window.close());
    }
    header.pack_end(&close_button);
    window.set_titlebar(Some(&header));

    let stack = pages(engine, store);
    let sidebar = gtk4::StackSidebar::new();
    sidebar.set_stack(&stack);

    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&sidebar);
    root.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    root.append(&stack);
    window.set_child(Some(&root));

    window
}

/// Build and present the settings window.
pub fn show_settings(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
    parent: Option<&gtk4::Window>,
) {
    let window = create_settings_window(engine, store);
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }
    window.present();
}

/// One stack page per sidebar entry; the first added is shown.
fn pages(engine: Rc<RefCell<HotkeyEngine>>, store: Rc<SessionStore>) -> gtk4::Stack {
    let stack = gtk4::Stack::new();
    stack.set_hexpand(true);
    stack.add_titled(&general::page(store.clone()), Some(GENERAL_PAGE), "General");
    stack.add_titled(
        &credentials::page(store.clone()),
        Some(CREDENTIALS_PAGE),
        "Credentials",
    );
    stack.add_titled(
        &hotkeys::page(engine, store),
        Some(SHORTCUTS_PAGE),
        "Keyboard Shortcuts",
    );
    stack
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    fn switches(widget: &gtk4::Widget, found: &mut Vec<gtk4::Switch>) {
        if let Some(switch) = widget.downcast_ref::<gtk4::Switch>() {
            found.push(switch.clone());
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            switches(&widget, found);
            child = widget.next_sibling();
        }
    }

    /// The sidebar lists General, Credentials, then Keyboard Shortcuts, and opens on
    /// General; each General switch shows and saves its own row.
    pub(in crate::ui) fn pages_and_general_switches() {
        let store = Rc::new(SessionStore::in_memory().unwrap());
        store
            .set_setting(crate::ui::tray::MINIMIZE_TO_TRAY_KEY, "0")
            .unwrap();
        let engine = Rc::new(RefCell::new(HotkeyEngine::new()));
        let stack = pages(engine, store.clone());

        let model = stack.pages();
        let listed: Vec<(String, String)> = (0..model.n_items())
            .filter_map(|index| model.item(index))
            .filter_map(|item| item.downcast::<gtk4::StackPage>().ok())
            .map(|page| {
                let name = page.name().unwrap_or_default().to_string();
                let title = page.title().unwrap_or_default().to_string();
                (name, title)
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (GENERAL_PAGE.to_string(), "General".to_string()),
                (CREDENTIALS_PAGE.to_string(), "Credentials".to_string()),
                (SHORTCUTS_PAGE.to_string(), "Keyboard Shortcuts".to_string()),
            ]
        );
        assert_eq!(stack.visible_child_name().as_deref(), Some(GENERAL_PAGE));

        let general = stack.child_by_name(GENERAL_PAGE).unwrap();
        let mut found = Vec::new();
        switches(&general, &mut found);
        assert_eq!(found.len(), general::TOGGLES.len());
        let shown: Vec<bool> = found.iter().map(|switch| switch.is_active()).collect();
        assert_eq!(shown, vec![true, true, true, false]);

        // Flipping a switch saves its row; the tray row needs no tray.
        let tray_index = general::TOGGLES
            .iter()
            .position(|toggle| toggle.key == crate::ui::tray::MINIMIZE_TO_TRAY_KEY)
            .unwrap();
        found[tray_index].set_active(true);
        assert!(crate::ui::tray::minimize_to_tray_enabled(&store));
        assert_eq!(
            store
                .get_setting(crate::ui::tray::MINIMIZE_TO_TRAY_KEY)
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }
}
