//! The side dock (doc 01): a column of icon-only tabs on the window's left
//! edge — Sessions, Macros, SFTP — choosing the panel beside the notebook,
//! MobaXterm style. The tabs carry no text; each names itself in a tooltip
//! and to assistive technology.
//!
//! The SFTP panel follows the notebook: every SSH tab keeps its own
//! [`SftpSidebar`] (doc 07), and the panel hosts the current tab's, or a
//! placeholder for tabs without one. A sidebar counts as shown — and a
//! deferred SFTP channel opens — only once the panel has it in view.
//!
//! The Macros panel is the window's [`MacrosPanel`](super::macros_panel)
//! (doc 12). A click on the active tab collapses the panel, leaving the
//! strip, as `toggle_session_tree` can (doc 11 §Handlers); no tab shows
//! active while it is collapsed, and picking any tab shows it again.
//! [`DockPage`] and [`toggle_target`] are display-free.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use crate::ui::panels::sftp_sidebar::SftpSidebar;
use crate::ui::tabs::{current_page, SessionTab};
use crate::ui::widgets::{data, set_data, set_margins};

/// Notebook data key: the window's dock, for the tab code's notifications.
const DOCK_KEY: &str = "wayterm-side-dock";
/// The SFTP panel's two faces.
const SFTP_PLACEHOLDER: &str = "none";
const SFTP_SIDEBAR: &str = "sidebar";

/// One of the dock's panels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DockPage {
    Sessions,
    Macros,
    Sftp,
}

impl DockPage {
    /// Strip order, top to bottom.
    pub const ALL: [DockPage; 3] = [DockPage::Sessions, DockPage::Macros, DockPage::Sftp];

    /// The panel's name in the stack.
    pub fn id(self) -> &'static str {
        match self {
            DockPage::Sessions => "sessions",
            DockPage::Macros => "macros",
            DockPage::Sftp => "sftp",
        }
    }

    /// The tab's tooltip and accessible name.
    pub fn label(self) -> &'static str {
        match self {
            DockPage::Sessions => "Sessions",
            DockPage::Macros => "Macros",
            DockPage::Sftp => "SFTP",
        }
    }

    /// The tab's icon, bundled with the app (`ui::icons`).
    pub fn icon(self) -> &'static str {
        match self {
            DockPage::Sessions => "wayterm-star-symbolic",
            DockPage::Macros => "wayterm-macro-symbolic",
            DockPage::Sftp => "wayterm-sftp-symbolic",
        }
    }
}

/// Where `toggle_sftp` goes from `current`: to SFTP, or from SFTP back to
/// `previous`, the panel the user left for it.
pub fn toggle_target(current: DockPage, previous: DockPage) -> DockPage {
    if current == DockPage::Sftp {
        previous
    } else {
        DockPage::Sftp
    }
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// The dock: the icon strip and the panel stack it switches. Clones share
/// everything.
#[derive(Clone)]
pub struct SideDock {
    inner: Rc<DockInner>,
}

struct DockInner {
    strip: gtk4::Box,
    stack: gtk4::Stack,
    tabs: Vec<(DockPage, gtk4::ToggleButton)>,
    sftp: SftpPanel,
    current: Cell<DockPage>,
    /// The last panel other than SFTP, where `toggle_sftp` returns.
    previous: Cell<DockPage>,
    /// Set by a tab's activation, so the click that caused it does not
    /// also count as a click on the active tab.
    switched: Cell<bool>,
}

impl SideDock {
    /// The dock around the session tree and the macros panel, its SFTP
    /// panel following `notebook`. It starts on Sessions.
    pub fn new(
        sessions: &impl IsA<gtk4::Widget>,
        macros: &impl IsA<gtk4::Widget>,
        notebook: &gtk4::Notebook,
    ) -> Self {
        let sftp = SftpPanel::new();
        let stack = gtk4::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        for page in DockPage::ALL {
            let child: gtk4::Widget = match page {
                DockPage::Sessions => sessions.clone().upcast(),
                DockPage::Macros => macros.clone().upcast(),
                DockPage::Sftp => sftp.stack.clone().upcast(),
            };
            stack
                .add_titled(&child, Some(page.id()), page.label())
                .set_icon_name(page.icon());
        }

        let strip = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        set_margins(&strip, 4);
        let mut tabs: Vec<(DockPage, gtk4::ToggleButton)> = Vec::new();
        for page in DockPage::ALL {
            let button = gtk4::ToggleButton::new();
            button.set_icon_name(page.icon());
            button.set_tooltip_text(Some(page.label()));
            button.update_property(&[gtk4::accessible::Property::Label(page.label())]);
            button.add_css_class("flat");
            // Picking a panel leaves keyboard focus in the terminal.
            button.set_focus_on_click(false);
            button.set_group(tabs.first().map(|(_, first)| first));
            strip.append(&button);
            tabs.push((page, button));
        }
        // Before the handlers: the stack already shows its first child.
        tabs[0].1.set_active(true);

        let inner = Rc::new(DockInner {
            strip,
            stack,
            tabs,
            sftp,
            current: Cell::new(DockPage::Sessions),
            previous: Cell::new(DockPage::Sessions),
            switched: Cell::new(false),
        });
        inner.connect_signals(notebook);
        inner.host(current_page(notebook).as_ref());
        let dock = Self { inner };
        set_data(notebook, DOCK_KEY, dock.clone());
        dock
    }

    /// The icon strip, for the window's left edge.
    pub fn strip(&self) -> &gtk4::Widget {
        self.inner.strip.upcast_ref()
    }

    /// The panel stack, beside the notebook.
    pub fn panel(&self) -> &gtk4::Widget {
        self.inner.stack.upcast_ref()
    }

    /// The SFTP panel: the `sftp` shortcuts fire while focus is inside it
    /// (doc 11 §Dispatch).
    pub fn sftp_panel(&self) -> &gtk4::Widget {
        self.inner.sftp.stack.upcast_ref()
    }

    pub fn current(&self) -> DockPage {
        self.inner.current.get()
    }

    /// Switch to `page`'s panel, as a click on its tab does, showing the
    /// panel if it was hidden.
    pub fn select(&self, page: DockPage) {
        if let Some((_, button)) = self.inner.tabs.iter().find(|(p, _)| *p == page) {
            button.set_active(true);
        }
        self.inner.switched.set(false);
        self.inner.show_panel();
    }

    /// `toggle_sftp` (doc 07): to the SFTP panel, or back from it; a
    /// hidden panel comes back on SFTP.
    pub fn toggle_sftp(&self) {
        let target = if self.inner.stack.is_visible() {
            toggle_target(self.current(), self.inner.previous.get())
        } else {
            DockPage::Sftp
        };
        self.select(target);
    }

    /// `toggle_session_tree` (doc 11): hide the panel while it shows
    /// Sessions, otherwise show it on Sessions. `true` when Sessions is now
    /// in view (the caller moves focus into the tree).
    pub fn toggle_sessions(&self) -> bool {
        if self.inner.stack.is_visible() && self.current() == DockPage::Sessions {
            self.inner.hide_panel();
            false
        } else {
            self.select(DockPage::Sessions);
            true
        }
    }
}

impl DockInner {
    fn connect_signals(self: &Rc<Self>, notebook: &gtk4::Notebook) {
        for (page, button) in &self.tabs {
            let (page, weak) = (*page, Rc::downgrade(self));
            button.connect_toggled(move |button| {
                if let (true, Some(inner)) = (button.is_active(), weak.upgrade()) {
                    inner.switched.set(true);
                    inner.on_selected(page);
                }
            });
            // Runs after the toggle: a click that switched panels shows the
            // panel, a click on the active tab collapses it.
            let (weak, notebook) = (Rc::downgrade(self), notebook.downgrade());
            button.connect_clicked(move |_| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.switched.replace(false) || !inner.stack.is_visible() {
                    inner.show_panel();
                    return;
                }
                inner.hide_panel();
                // The collapsed panel took focus with it.
                let terminal = notebook
                    .upgrade()
                    .and_then(|n| SessionTab::active_terminal(&n));
                if let Some(terminal) = terminal {
                    terminal.widget().grab_focus();
                }
            });
        }
        // `switch-page` runs before the notebook updates its current page,
        // so the page comes from the signal.
        let weak = Rc::downgrade(self);
        notebook.connect_switch_page(move |_, page, _| {
            if let Some(inner) = weak.upgrade() {
                inner.host(Some(page));
            }
        });
        // A closed tab's sidebar is let go even when the current page
        // stays (a background tab closed).
        let weak = Rc::downgrade(self);
        notebook.connect_page_removed(move |notebook, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.host(current_page(notebook).as_ref());
            }
        });
    }

    /// Collapse the panel to the strip, no tab active, until a tab is picked.
    fn hide_panel(&self) {
        self.stack.set_visible(false);
        for (_, button) in &self.tabs {
            button.set_active(false);
        }
    }

    /// Show a hidden panel; SFTP, if current, comes into view.
    fn show_panel(&self) {
        if self.stack.is_visible() {
            return;
        }
        self.stack.set_visible(true);
        if self.current.get() == DockPage::Sftp {
            self.sftp.show_hosted();
        }
    }

    fn on_selected(&self, page: DockPage) {
        self.stack.set_visible_child_name(page.id());
        self.current.set(page);
        if page == DockPage::Sftp {
            self.sftp.show_hosted();
        } else {
            self.previous.set(page);
        }
    }

    /// Host `page`'s sidebar in the SFTP panel (a placeholder without one).
    fn host(&self, page: Option<&gtk4::Widget>) {
        let sidebar = page.and_then(SessionTab::sidebar_for_page);
        let in_view = self.current.get() == DockPage::Sftp && self.stack.is_visible();
        self.sftp.host(sidebar, in_view);
    }
}

/// The SFTP panel: the current tab's sidebar, or a placeholder.
struct SftpPanel {
    stack: gtk4::Stack,
    /// Holds the hosted sidebar's widget, at most one.
    slot: gtk4::Box,
    hosted: RefCell<Option<SftpSidebar>>,
}

impl SftpPanel {
    fn new() -> Self {
        let slot = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let stack = gtk4::Stack::new();
        stack.add_named(
            &empty_state(
                DockPage::Sftp.icon(),
                "Select an SSH tab to browse its files",
            ),
            Some(SFTP_PLACEHOLDER),
        );
        stack.add_named(&slot, Some(SFTP_SIDEBAR));
        stack.set_visible_child_name(SFTP_PLACEHOLDER);
        Self {
            stack,
            slot,
            hosted: RefCell::default(),
        }
    }

    /// Host `sidebar` in place of the previous one, showing it when the
    /// panel is `in_view`.
    fn host(&self, sidebar: Option<SftpSidebar>, in_view: bool) {
        let unchanged = match (&*self.hosted.borrow(), &sidebar) {
            (Some(old), Some(new)) => old.widget() == new.widget(),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            if let Some(old) = self.hosted.borrow_mut().take() {
                self.slot.remove(old.widget());
            }
            if let Some(new) = &sidebar {
                self.slot.append(new.widget());
            }
            *self.hosted.borrow_mut() = sidebar;
        }
        let face = match &*self.hosted.borrow() {
            Some(_) => SFTP_SIDEBAR,
            None => SFTP_PLACEHOLDER,
        };
        self.stack.set_visible_child_name(face);
        if in_view {
            self.show_hosted();
        }
    }

    /// The panel came into view: show the hosted sidebar (its first show
    /// may start the SFTP task).
    fn show_hosted(&self) {
        let hosted = self.hosted.borrow().clone();
        if let Some(sidebar) = hosted {
            sidebar.show();
        }
    }
}

/// A dim icon over a line of text, centered: a panel with nothing to show.
pub(in crate::ui) fn empty_state(icon: &str, text: &str) -> gtk4::Widget {
    let image = gtk4::Image::from_icon_name(icon);
    image.set_pixel_size(48);
    let label = gtk4::Label::new(Some(text));
    label.set_wrap(true);
    label.set_justify(gtk4::Justification::Center);
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    column.set_valign(gtk4::Align::Center);
    column.set_halign(gtk4::Align::Center);
    column.set_vexpand(true);
    column.set_hexpand(true);
    column.add_css_class("dim-label");
    set_margins(&column, 12);
    column.append(&image);
    column.append(&label);
    column.upcast()
}

/// The tab code replaced a page's sidebar (a redial lets the old one's
/// SFTP task go): host the current page's anew. No-op without a dock.
pub(crate) fn sidebar_replaced(notebook: &gtk4::Notebook) {
    if let Some(dock) = data::<SideDock>(notebook, DOCK_KEY) {
        dock.inner.host(current_page(notebook).as_ref());
    }
}

/// Bring the SFTP panel forward for `page` when it is the current page
/// (`sftp_on_connect`). No-op without a dock, or while the user keeps the
/// panel hidden.
pub(crate) fn reveal_sftp(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    if current_page(notebook).as_ref() != Some(page) {
        return;
    }
    if let Some(dock) = data::<SideDock>(notebook, DOCK_KEY) {
        if dock.inner.stack.is_visible() {
            dock.select(DockPage::Sftp);
        }
    }
}

#[cfg(test)]
mod tests;

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(in crate::ui) mod gtk_tests {
    use gtk4::glib;

    use super::*;

    fn spin() {
        for _ in 0..10 {
            glib::MainContext::default().iteration(false);
        }
    }

    /// One icon-only tab per panel, named by its tooltip; a tab switches
    /// the panel, a click on the active tab collapses it, and
    /// `toggle_sftp` goes to SFTP and back.
    pub(in crate::ui) fn icon_tabs_switch_the_panel() {
        let notebook = gtk4::Notebook::new();
        let tree = gtk4::Label::new(Some("tree"));
        let macros = gtk4::Label::new(Some("macros"));
        let dock = SideDock::new(&tree, &macros, &notebook);
        let visible = || dock.inner.stack.visible_child_name().map(|n| n.to_string());

        let tabs = &dock.inner.tabs;
        let pages: Vec<DockPage> = tabs.iter().map(|(page, _)| *page).collect();
        assert_eq!(pages, DockPage::ALL);
        for (page, button) in tabs {
            assert_eq!(button.tooltip_text().as_deref(), Some(page.label()));
            assert_eq!(button.icon_name().as_deref(), Some(page.icon()));
            assert_eq!(button.label(), None, "{page:?}: no text on the tab");
            assert!(button
                .child()
                .is_some_and(|child| child.is::<gtk4::Image>()));
        }
        assert_eq!(dock.current(), DockPage::Sessions);
        assert_eq!(visible().as_deref(), Some("sessions"));
        assert_eq!(
            dock.inner.stack.visible_child().as_ref(),
            Some(tree.upcast_ref())
        );

        tabs[1].1.emit_clicked();
        assert_eq!(dock.current(), DockPage::Macros);
        assert_eq!(visible().as_deref(), Some("macros"));
        assert_eq!(
            dock.inner.stack.visible_child().as_ref(),
            Some(macros.upcast_ref())
        );
        assert!(!tabs[0].1.is_active(), "one tab at a time");

        let panel_shown = || dock.panel().is_visible();
        let none_active = || tabs.iter().all(|(_, button)| !button.is_active());
        tabs[1].1.emit_clicked();
        assert!(!panel_shown(), "a click on the active tab collapses it");
        assert!(none_active());
        assert!(dock.strip().is_visible(), "the tabs stay");
        tabs[1].1.emit_clicked();
        assert!(panel_shown(), "and a second click brings it back");
        assert_eq!(dock.current(), DockPage::Macros);
        assert!(tabs[1].1.is_active());
        tabs[1].1.emit_clicked();
        tabs[0].1.emit_clicked();
        assert!(panel_shown(), "any tab brings it back");
        assert_eq!(dock.current(), DockPage::Sessions);
        tabs[1].1.emit_clicked();
        assert!(panel_shown(), "a switching click never collapses");
        assert_eq!(dock.current(), DockPage::Macros);

        dock.toggle_sftp();
        assert_eq!(visible().as_deref(), Some("sftp"));
        assert!(tabs[2].1.is_active());
        dock.toggle_sftp();
        assert_eq!(dock.current(), DockPage::Macros, "back where it came from");

        // `toggle_session_tree`: to Sessions, then hide, then back.
        assert!(dock.toggle_sessions());
        assert_eq!(dock.current(), DockPage::Sessions);
        assert!(!dock.toggle_sessions());
        assert!(!panel_shown(), "a second toggle hides the panel");
        assert!(none_active());
        tabs[0].1.emit_clicked();
        assert!(panel_shown(), "a click on the tab brings it back");
        assert!(tabs[0].1.is_active());
        dock.inner.hide_panel();
        dock.toggle_sftp();
        assert!(panel_shown());
        assert_eq!(
            dock.current(),
            DockPage::Sftp,
            "a hidden panel returns on SFTP"
        );
        dock.inner.hide_panel();
        assert!(dock.toggle_sessions(), "a hidden panel returns on Sessions");
        assert!(panel_shown());
    }

    /// The SFTP panel hosts the current tab's sidebar and a placeholder
    /// otherwise; a sidebar is shown only once the panel has it in view; a
    /// replaced sidebar is hosted anew, and a closed tab's is let go.
    pub(in crate::ui) fn sftp_panel_follows_the_current_tab() {
        let notebook = gtk4::Notebook::new();
        let dock = SideDock::new(&gtk4::Label::new(None), &gtk4::Label::new(None), &notebook);
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.append(dock.strip());
        root.append(dock.panel());
        root.append(&notebook);
        let window = gtk4::Window::new();
        window.set_child(Some(&root));

        let local = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let ssh = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let first = SftpSidebar::hidden();
        SessionTab::set_sidebar_for_page(ssh.upcast_ref(), first.clone());
        notebook.append_page(&local, None::<&gtk4::Widget>);
        notebook.append_page(&ssh, None::<&gtk4::Widget>);
        window.present();
        spin();

        let face = || {
            dock.inner
                .sftp
                .stack
                .visible_child_name()
                .map(|n| n.to_string())
        };
        let hosted = |sidebar: &SftpSidebar| sidebar.widget().is_ancestor(dock.panel());
        assert_eq!(face().as_deref(), Some(SFTP_PLACEHOLDER), "a local tab");

        notebook.set_current_page(Some(1));
        assert!(hosted(&first));
        assert_eq!(face().as_deref(), Some(SFTP_SIDEBAR));
        assert!(!first.is_shown(), "out of view: no SFTP channel yet");
        dock.select(DockPage::Sftp);
        assert!(first.is_shown(), "in view");

        notebook.set_current_page(Some(0));
        assert_eq!(face().as_deref(), Some(SFTP_PLACEHOLDER));
        assert!(first.widget().parent().is_none(), "let go with its tab");

        // A redial replaces the sidebar; the panel, in view, shows the new one.
        notebook.set_current_page(Some(1));
        let second = SftpSidebar::hidden();
        SessionTab::set_sidebar_for_page(ssh.upcast_ref(), second.clone());
        sidebar_replaced(&notebook);
        assert!(hosted(&second) && !hosted(&first));
        assert!(second.is_shown());

        // `sftp_on_connect` brings the panel forward for the current tab
        // only, and never un-hides a panel the user hid.
        dock.select(DockPage::Sessions);
        reveal_sftp(&notebook, local.upcast_ref());
        assert_eq!(dock.current(), DockPage::Sessions, "a background tab");
        dock.inner.hide_panel();
        reveal_sftp(&notebook, ssh.upcast_ref());
        assert_eq!(dock.current(), DockPage::Sessions, "the panel stays hidden");
        assert!(!dock.panel().is_visible());
        dock.select(DockPage::Sessions);
        reveal_sftp(&notebook, ssh.upcast_ref());
        assert_eq!(dock.current(), DockPage::Sftp);

        // Closing the tab lets its sidebar go.
        notebook.remove_page(Some(1));
        assert!(second.widget().parent().is_none());
        assert_eq!(face().as_deref(), Some(SFTP_PLACEHOLDER));
        window.destroy();
    }
}
