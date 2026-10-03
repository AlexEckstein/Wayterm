//! Main application window — layout only (doc 01 / doc 10 Phase 1).
//!
//! The high-level structure is a horizontal [`gtk4::Paned`] with a
//! session-tree slot on the left and a [`gtk4::Notebook`] on the
//! right. The left side is intentionally a placeholder: the real
//! session tree (consuming [`crate::store::SessionStore`]) lands in
//! Task 7.

use gtk4::prelude::*;

/// The primary Wayterm application window.
#[allow(dead_code)]
pub struct MainWindow {
    window: gtk4::ApplicationWindow,
}

impl MainWindow {
    /// Build and present the main window.
    ///
    /// Creates the paned layout described in the module docs and wires
    /// the minimum placeholder widgets needed to render the split.
    pub fn new(app: &gtk4::Application) -> Self {
        let notebook = gtk4::Notebook::new();
        let home_label = gtk4::Label::new(Some("Welcome to Wayterm"));
        notebook.append_page(&home_label, Some(&gtk4::Label::new(Some("Home"))));

        // Session-tree seam (doc 01): placeholder until Task 7 swaps
        // this for a real Gtk.TreeView backed by SessionStore.
        let session_tree_slot = gtk4::Label::new(Some("Sessions"));
        session_tree_slot.set_margin_top(12);
        session_tree_slot.set_margin_bottom(12);
        session_tree_slot.set_margin_start(12);
        session_tree_slot.set_margin_end(12);

        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(280);
        paned.set_start_child(Some(&session_tree_slot));
        paned.set_end_child(Some(&notebook));
        paned.set_vexpand(true);
        paned.set_hexpand(true);

        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Wayterm")
            .default_width(1200)
            .default_height(800)
            .child(&paned)
            .build();
        window.present();

        Self { window }
    }

    /// Return the underlying GTK window.
    ///
    /// Useful for tests that need to inspect or close the window.
    #[allow(dead_code)]
    pub fn window(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }
}
