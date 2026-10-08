//! Main application window — layout only (doc 01 / doc 10 Phase 1).
//!
//! The high-level structure is a vertical [`gtk4::Box`] holding a
//! horizontal [`gtk4::Paned`] (session-tree sidebar on the left,
//! [`gtk4::Notebook`] on the right) with a [`StatusBar`] pinned to the
//! bottom.

use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::transport::types::TransportState;
use crate::ui::tab::SessionTab;
use crate::ui::tab_restore::TabPersistence;
use crate::ui::{SessionTree, StatusBar};

/// Key under which the status bar is stashed on the window, following the
/// same pattern as `tab.rs`'s session-id key.
const STATUS_BAR_KEY: &str = "wayterm-status-bar";

/// The primary Wayterm application window.
#[allow(dead_code)]
pub struct MainWindow {
    window: gtk4::ApplicationWindow,
}

impl MainWindow {
    /// Build and present the main window, wiring in the session tree.
    pub fn new(app: &gtk4::Application, store: SessionStore) -> Self {
        let notebook = gtk4::Notebook::new();
        let home_label = gtk4::Label::new(Some("Welcome to Wayterm"));
        notebook.append_page(&home_label, Some(&gtk4::Label::new(Some("Home"))));
        // Keep the welcome page's widget: tab persistence identifies
        // the home tab by identity, so a reordered notebook still never
        // saves it (doc 10 Phase 4 — it is rebuilt on every launch).
        let home_widget: gtk4::Widget = home_label.upcast();

        // Tab persistence needs its own connection: `store` moves into
        // the session tree below.
        let store_for_tabs = store.duplicate().unwrap_or_else(|e| {
            eprintln!("Wayterm: failed to duplicate store for tab persistence: {e}");
            // Tabs then simply do not survive the run; the app is still
            // usable, which beats refusing to start.
            SessionStore::in_memory().expect("failed to create fallback store")
        });

        let notebook_for_tree = notebook.clone();
        let store_for_tree = store.duplicate().unwrap_or_else(|e| {
            eprintln!("Wayterm: failed to duplicate store for session tree: {e}");
            // Fall back to an in-memory store so the tree still renders;
            // opening tabs from it will fail silently when sessions are not
            // found.
            SessionStore::in_memory().expect("failed to create fallback store")
        });
        let session_tree = SessionTree::new(store, move |session_id| {
            let store = match store_for_tree.duplicate() {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("Wayterm: failed to duplicate store for open tab: {err}");
                    return;
                }
            };
            match store.get_session(session_id) {
                Ok(config) => {
                    let _ = SessionTab::open(&notebook_for_tree, &config);
                }
                Err(err) => {
                    eprintln!("Wayterm: failed to load session {session_id}: {err}");
                }
            }
        });
        session_tree.refresh();

        // The tree needs to scroll when the folder list grows.
        let scrolled = gtk4::ScrolledWindow::builder()
            .child(&session_tree.widget())
            .vexpand(true)
            .hexpand(true)
            .build();

        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(280);
        paned.set_start_child(Some(&scrolled));
        paned.set_end_child(Some(&notebook));
        paned.set_vexpand(true);
        paned.set_hexpand(true);

        // Status bar pinned to the bottom of the window. No tab is active
        // yet, so it shows the initial Disconnected state with no host;
        // task P2-7b drives it from the active tab's transport.
        let status_bar = StatusBar::new();
        status_bar.set_state(&TransportState::Disconnected, "", 0);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&paned);
        root.append(status_bar.widget());

        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Wayterm")
            .default_width(1200)
            .default_height(800)
            .child(&root)
            .build();
        // Keep the status bar alive for the window's lifetime; task P2-7b
        // retrieves it with `window.data::<StatusBar>("wayterm-status-bar")`
        // to drive `set_state` from the active tab.
        unsafe {
            window.set_data(STATUS_BAR_KEY, status_bar);
        }

        // Tab session persistence (doc 10 Phase 4: "Remember open tabs
        // on exit, restore on launch"). Restore first, then start
        // watching: the restored tabs are the starting state, not a
        // burst of user actions to be re-saved.
        let tabs = TabPersistence::new(notebook.clone(), home_widget, store_for_tabs);
        tabs.restore();
        tabs.install();

        // Exit save. The close-request handler is also what keeps the
        // watcher alive for the window's lifetime — the notebook's own
        // handlers only hold a weak reference back.
        window.connect_close_request(move |_| {
            tabs.flush();
            gtk4::glib::Propagation::Proceed
        });

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
