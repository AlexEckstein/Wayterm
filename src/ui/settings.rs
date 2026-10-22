//! The Settings window: a sidebar of pages beside the selected one —
//! [`general`], [`credentials`] (doc 05), Keyboard Shortcuts
//! ([`hotkeys`], doc 11), and Backup / Restore ([`backup`], doc 10). Every
//! page saves each change at once and applies it live; there is no Apply
//! button.

pub mod backup;
pub mod credentials;
pub mod general;
pub mod hotkeys;

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use crate::hotkeys::HotkeyEngine;
use crate::store::SessionStore;
use crate::ui::widgets::set_margins;

/// Stack names of the pages, in sidebar order.
pub const GENERAL_PAGE: &str = "general";
/// Stack name of the Credentials page.
pub const CREDENTIALS_PAGE: &str = "credentials";
/// Stack name of the Keyboard Shortcuts page.
pub const SHORTCUTS_PAGE: &str = "keyboard-shortcuts";
/// Stack name of the Backup / Restore page.
pub const BACKUP_PAGE: &str = "backup-restore";

/// The settings window (unpresented), opened on the page named `page` (a
/// `*_PAGE`). `on_import` runs after the Backup / Restore page writes an
/// import.
pub fn create_settings_window(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
    on_import: Rc<dyn Fn()>,
    page: &str,
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

    let stack = pages(engine, store, on_import);
    stack.set_visible_child_name(page);
    let sidebar = gtk4::StackSidebar::new();
    sidebar.set_stack(&stack);

    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&sidebar);
    root.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    root.append(&stack);
    window.set_child(Some(&root));

    window
}

/// Build and present the settings window (see [`create_settings_window`]).
pub fn show_settings(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
    on_import: Rc<dyn Fn()>,
    page: &str,
    parent: Option<&gtk4::Window>,
) {
    let window = create_settings_window(engine, store, on_import, page);
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }
    window.present();
}

/// One stack page per sidebar entry; the first added is shown.
fn pages(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
    on_import: Rc<dyn Fn()>,
) -> gtk4::Stack {
    let stack = gtk4::Stack::new();
    stack.set_hexpand(true);
    stack.add_titled(&general::page(store.clone()), Some(GENERAL_PAGE), "General");
    stack.add_titled(
        &credentials::page(store.clone()),
        Some(CREDENTIALS_PAGE),
        "Credentials",
    );
    stack.add_titled(
        &hotkeys::page(engine, store.clone()),
        Some(SHORTCUTS_PAGE),
        "Keyboard Shortcuts",
    );
    stack.add_titled(
        &backup::page(store, on_import),
        Some(BACKUP_PAGE),
        "Backup / Restore",
    );
    stack
}

/// A page: `children` stacked in a margined column that scrolls vertically.
fn scrolling_page(children: &[&gtk4::Widget]) -> gtk4::Widget {
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    set_margins(&column, 18);
    for child in children {
        column.append(*child);
    }
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&column)
        .vexpand(true)
        .hexpand(true)
        .build();
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled.upcast()
}

/// A section heading.
fn heading(text: &str) -> gtk4::Label {
    let heading = gtk4::Label::new(Some(text));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    heading
}

/// The boxed list a section's rows go in.
fn boxed_list() -> gtk4::ListBox {
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    list
}

/// One list row: `children` side by side.
fn row(children: &[&gtk4::Widget]) -> gtk4::ListBoxRow {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    set_margins(&hbox, 12);
    for child in children {
        hbox.append(*child);
    }
    gtk4::ListBoxRow::builder()
        .child(&hbox)
        .activatable(false)
        .build()
}

/// `title` over a dimmed `subtitle` (when given), labelling and describing
/// `control` for assistive technology.
fn described(title: &str, subtitle: Option<&str>, control: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    use gtk4::accessible::Relation;
    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let title = gtk4::Label::new(Some(title));
    title.set_xalign(0.0);
    text.append(&title);
    let title: [&gtk4::Accessible; 1] = [title.upcast_ref()];
    let control = control.upcast_ref::<gtk4::Widget>();
    match subtitle {
        Some(subtitle) => {
            let subtitle = gtk4::Label::new(Some(subtitle));
            subtitle.set_xalign(0.0);
            subtitle.set_wrap(true);
            subtitle.add_css_class("dim-label");
            text.append(&subtitle);
            let subtitle: [&gtk4::Accessible; 1] = [subtitle.upcast_ref()];
            control.update_relation(&[
                Relation::LabelledBy(&title),
                Relation::DescribedBy(&subtitle),
            ]);
        }
        None => control.update_relation(&[Relation::LabelledBy(&title)]),
    }
    text
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    /// Every descendant of type `W`, in tree order.
    fn descendants<W: IsA<gtk4::Widget>>(widget: &gtk4::Widget, found: &mut Vec<W>) {
        if let Some(hit) = widget.downcast_ref::<W>() {
            found.push(hit.clone());
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            descendants(&widget, found);
            child = widget.next_sibling();
        }
    }

    fn switches(widget: &gtk4::Widget, found: &mut Vec<gtk4::Switch>) {
        descendants(widget, found);
    }

    /// The sidebar lists General, Credentials, Keyboard Shortcuts, then
    /// Backup / Restore, and opens on
    /// General; each General switch shows and saves its own row.
    pub(in crate::ui) fn pages_and_general_switches() {
        let store = Rc::new(SessionStore::in_memory().unwrap());
        store
            .set_setting(crate::ui::tray::MINIMIZE_TO_TRAY_KEY, "0")
            .unwrap();
        let engine = Rc::new(RefCell::new(HotkeyEngine::new()));
        let stack = pages(engine, store.clone(), Rc::new(|| {}));

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
                (BACKUP_PAGE.to_string(), "Backup / Restore".to_string()),
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

        // The Ghostty check box: off (VTE) by default; usable only in a
        // build with the backend, otherwise disabled with its tooltip.
        let mut checks: Vec<gtk4::CheckButton> = Vec::new();
        descendants(&general, &mut checks);
        assert_eq!(checks.len(), 1);
        let check = &checks[0];
        assert!(!check.is_active());
        assert_eq!(check.is_sensitive(), crate::terminal::GHOSTTY_COMPILED);
        let tooltip = check.parent().and_then(|row| row.tooltip_text());
        if crate::terminal::GHOSTTY_COMPILED {
            assert_eq!(tooltip, None);
            check.set_active(true);
            assert_eq!(
                general::backend(&store),
                crate::terminal::BackendKind::Ghostty
            );
            check.set_active(false);
            assert_eq!(general::backend(&store), crate::terminal::BackendKind::Vte);
        } else {
            assert_eq!(tooltip.as_deref(), Some(general::GHOSTTY_UNAVAILABLE));
        }

        // The color scheme drop-down: opens on the seeded System and saves
        // each pick.
        use crate::terminal::SchemeChoice;
        let mut dropdowns: Vec<gtk4::DropDown> = Vec::new();
        descendants(&general, &mut dropdowns);
        assert_eq!(dropdowns.len(), 1);
        let dropdown = &dropdowns[0];
        let position = |choice| SchemeChoice::ALL.iter().position(|c| *c == choice).unwrap();
        assert_eq!(dropdown.selected() as usize, position(SchemeChoice::System));
        dropdown.set_selected(position(SchemeChoice::Light) as u32);
        assert_eq!(general::color_scheme(&store), SchemeChoice::Light);
        dropdown.set_selected(position(SchemeChoice::System) as u32);
        assert_eq!(general::color_scheme(&store), SchemeChoice::System);
    }
}
