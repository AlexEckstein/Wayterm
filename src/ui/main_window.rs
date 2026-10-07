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
use crate::ui::{SessionTree, StatusBar};

/// gio action id for doc 09's `toggle_sftp`.
const TOGGLE_SFTP_ACTION: &str = "toggle_sftp";

/// Doc 07's default key for `toggle_sftp` (`<Control><Shift>B`).
const TOGGLE_SFTP_ACCEL: &str = "<Control><Shift>b";

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

        install_toggle_sftp(app, &window, &notebook);

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

/// Register doc 09's `toggle_sftp` action on the window with doc 07's default
/// key, Ctrl+Shift+B.
///
/// The action acts on the **active** tab (doc 09: the sidebar is per tab), and
/// is disabled whenever the active page has no sidebar — that is how
/// doc 09 §Availability's "RDP/VNC/serial tabs hide the toggle" is enforced:
/// page switches (and tab close, which always switches) re-evaluate the
/// enabled state.
fn install_toggle_sftp(
    app: &gtk4::Application,
    window: &gtk4::ApplicationWindow,
    notebook: &gtk4::Notebook,
) {
    let action = gio::SimpleAction::new(TOGGLE_SFTP_ACTION, None);

    let for_activate = notebook.clone();
    action.connect_activate(move |_, _| {
        if let Some(sidebar) = SessionTab::active_sidebar(&for_activate) {
            sidebar.toggle_visible();
        }
    });

    let for_state = action.clone();
    notebook.connect_switch_page(move |_notebook, page, _page_num| {
        for_state.set_enabled(SessionTab::sidebar_for_page(page).is_some());
    });

    // No SSH tab open yet: the Home page has no sidebar.
    action.set_enabled(false);
    window.add_action(&action);
    app.set_accels_for_action(&format!("win.{TOGGLE_SFTP_ACTION}"), &[TOGGLE_SFTP_ACCEL]);
}
