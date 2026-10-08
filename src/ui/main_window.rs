//! Main application window — layout only (doc 01 / doc 10 Phase 1).
//!
//! The high-level structure is a vertical [`gtk4::Box`] holding a
//! horizontal [`gtk4::Paned`] (session-tree sidebar on the left,
//! [`gtk4::Notebook`] on the right) with a [`StatusBar`] pinned to the
//! bottom.

use gio;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::transport::types::TransportState;
use crate::ui::tab::SessionTab;
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

// --- Hotkey settings entry (P4-6, additive) -------------------------------
// P3-3 owns the connect-flow hunks above; this block only appends a
// header-bar "Settings" button that opens the hotkey settings window
// (doc 07 §"Settings UI"), so the two lanes never touch the same lines.

impl MainWindow {
    /// Append a header-bar "Settings" button opening the hotkey settings
    /// window (doc 07 §"Settings UI").
    ///
    /// Additive by design: `new` is untouched, and the button lives in
    /// the window's titlebar, leaving the paned layout exactly as built.
    /// Registers the `win.hotkey-settings` open action (close is the
    /// settings window's own Close button plus the window-manager
    /// close); the header button simply activates that action.
    pub fn attach_hotkey_settings(
        &self,
        engine: std::rc::Rc<std::cell::RefCell<crate::hotkeys::HotkeyEngine>>,
        store: std::rc::Rc<crate::store::SessionStore>,
    ) {
        let open_settings = gio::SimpleAction::new("hotkey-settings", None);
        {
            let engine = std::rc::Rc::clone(&engine);
            let store = std::rc::Rc::clone(&store);
            let parent = self.window.clone();
            open_settings.connect_activate(move |_, _| {
                crate::ui::settings::show_hotkey_settings(
                    std::rc::Rc::clone(&engine),
                    std::rc::Rc::clone(&store),
                    Some(parent.upcast_ref::<gtk4::Window>()),
                );
            });
        }
        self.window.add_action(&open_settings);

        // A slim top strip carrying the Settings button. This runs after
        // `new` already presented the window, and `set_titlebar` on a
        // realized window only warns without applying — so the button
        // lives in the layout (prepended above the paned area) instead
        // of the native header bar.
        let settings_button = gtk4::Button::with_label("Settings");
        settings_button.set_action_name(Some("win.hotkey-settings"));
        settings_button.set_halign(gtk4::Align::End);
        settings_button.set_margin_top(4);
        settings_button.set_margin_bottom(4);
        settings_button.set_margin_start(8);
        settings_button.set_margin_end(8);
        if let Some(root) = self
            .window
            .child()
            .and_then(|child| child.downcast::<gtk4::Box>().ok())
        {
            root.prepend(&settings_button);
        }
    }
}
