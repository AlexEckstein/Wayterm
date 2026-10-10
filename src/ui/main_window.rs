//! The main window (doc 01): the session tree beside the tab notebook, the
//! status bar below. It also owns the window-wide connection supervisor
//! and the router that decides which tab drives the status bar.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::gio;
use gtk4::prelude::*;

use crate::hotkeys::HotkeyEngine;
use crate::store::SessionStore;
use crate::transport::connection::SshConnection;
use crate::transport::supervisor::ConnectionSupervisor;
use crate::transport::types::TransportState;
use crate::ui::session_tree::SessionTree;
use crate::ui::status_bar::StatusBar;
use crate::ui::tab::SessionTab;
use crate::ui::tab_restore::TabPersistence;
use crate::ui::widgets::{data, set_data};
use crate::ui::{settings, tab_resilience, terminal_actions};

/// Window data keys.
pub(crate) const SUPERVISOR_KEY: &str = "wayterm-connection-supervisor";
const STATUS_ROUTER_KEY: &str = "wayterm-status-router";

const TOGGLE_SFTP_ACTION: &str = "toggle_sftp";
/// Doc 07's default for `toggle_sftp`.
const TOGGLE_SFTP_ACCEL: &str = "<Control><Shift>b";

/// Lets only the notebook's current page drive the status bar, and never
/// pushes the same (page, state) twice, so the elapsed timer is not reset
/// for nothing.
struct StatusRouter {
    notebook: gtk4::Notebook,
    status_bar: Rc<StatusBar>,
    last: Option<(gtk4::Widget, TransportState)>,
}

impl StatusRouter {
    fn push(&mut self, page: &gtk4::Widget, state: TransportState, host: &str, port: u16) {
        let current = self
            .notebook
            .current_page()
            .and_then(|num| self.notebook.nth_page(Some(num)));
        if current.as_ref() != Some(page) {
            return;
        }
        if self.last.as_ref() == Some(&(page.clone(), state.clone())) {
            return;
        }
        self.status_bar.set_state(&state, host, port);
        self.last = Some((page.clone(), state));
    }
}

type StatusRouterCell = Rc<RefCell<StatusRouter>>;

/// Show `page`'s new state if it is the current page (no-op without a
/// window).
pub(crate) fn route_status(
    window: &gtk4::ApplicationWindow,
    page: &gtk4::Widget,
    state: TransportState,
    host: &str,
    port: u16,
) {
    if let Some(router) = data::<StatusRouterCell>(window, STATUS_ROUTER_KEY) {
        router.borrow_mut().push(page, state, host, port);
    }
}

pub struct MainWindow {
    window: gtk4::ApplicationWindow,
}

impl MainWindow {
    /// Build and present the window.
    pub fn new(
        app: &gtk4::Application,
        store: Rc<SessionStore>,
        hotkeys: Rc<RefCell<HotkeyEngine>>,
    ) -> Self {
        let notebook = gtk4::Notebook::new();
        let home = gtk4::Label::new(Some("Welcome to Wayterm"));
        notebook.append_page(&home, Some(&gtk4::Label::new(Some("Home"))));
        auto_hide_home(&notebook, &home);

        tab_resilience::install_settings(
            store.flag(tab_resilience::CONFIRM_CLOSE_SETTING),
            store.flag(tab_resilience::RECONNECT_ON_DROP_SETTING),
        );

        let session_tree = {
            let (store, notebook) = (store.clone(), notebook.clone());
            SessionTree::new(store.clone(), move |id| match store.get_session(id) {
                Ok(config) => {
                    SessionTab::open(&notebook, &config);
                }
                Err(err) => eprintln!("Wayterm: failed to load session {id}: {err}"),
            })
        };
        session_tree.refresh();

        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(280);
        paned.set_start_child(Some(&session_tree.widget()));
        paned.set_end_child(Some(&notebook));
        paned.set_vexpand(true);
        paned.set_hexpand(true);

        let status_bar = Rc::new(StatusBar::new());
        let router = Rc::new(RefCell::new(StatusRouter {
            notebook: notebook.clone(),
            status_bar: status_bar.clone(),
            last: None,
        }));
        let for_switch = router.clone();
        notebook.connect_switch_page(move |_, page, _| {
            let (state, host, port) = match SessionTab::page_status(page) {
                Some(status) => {
                    let status = status.borrow();
                    (status.state.clone(), status.host.clone(), status.port)
                }
                None => (TransportState::Disconnected, String::new(), 0),
            };
            for_switch.borrow_mut().push(page, state, &host, port);
        });

        let settings_button = gtk4::Button::with_label("Settings");
        settings_button.set_action_name(Some("win.hotkey-settings"));
        settings_button.set_halign(gtk4::Align::End);
        settings_button.set_margin_top(4);
        settings_button.set_margin_bottom(4);
        settings_button.set_margin_end(8);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&settings_button);
        root.append(&paned);
        root.append(status_bar.widget());

        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Wayterm")
            .default_width(1200)
            .default_height(800)
            .child(&root)
            .build();
        set_data(&window, STATUS_ROUTER_KEY, router);
        set_data(
            &window,
            SUPERVISOR_KEY,
            Arc::new(ConnectionSupervisor::<SshConnection>::new()),
        );

        install_toggle_sftp(app, &window, &notebook);
        terminal_actions::install(&window, &notebook);
        let for_actions = notebook.clone();
        tab_resilience::install_connection_actions(&window, move || {
            crate::ui::tab::current_session_id(&for_actions)
        });

        let open_settings = gio::SimpleAction::new("hotkey-settings", None);
        let (for_settings, parent) = (store.clone(), window.clone());
        open_settings.connect_activate(move |_, _| {
            settings::show_hotkey_settings(
                hotkeys.clone(),
                for_settings.clone(),
                Some(parent.upcast_ref()),
            );
        });
        window.add_action(&open_settings);

        // Restore before watching, so the restored tabs are not re-saved as
        // a burst of user actions. The close handler keeps the watcher alive
        // and writes the final state.
        let tabs = TabPersistence::new(notebook.clone(), home.upcast(), store);
        tabs.restore();
        tabs.install();
        window.connect_close_request(move |_| {
            tabs.flush();
            gtk4::glib::Propagation::Proceed
        });

        window.present();
        Self { window }
    }

    pub fn window(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }
}

/// Show the Home tab only while it is the notebook's sole page, so it
/// takes no tab-bar space next to open sessions. The page stays in the
/// notebook (hidden, not removed), keeping page indices stable for tab
/// restore.
fn auto_hide_home(notebook: &gtk4::Notebook, home: &gtk4::Label) {
    let sync = {
        let home = home.clone();
        move |notebook: &gtk4::Notebook| {
            let alone = notebook.n_pages() <= 1;
            home.set_visible(alone);
            if alone {
                notebook.set_current_page(Some(0));
            }
        }
    };
    let for_added = sync.clone();
    notebook.connect_page_added(move |notebook, _, _| {
        // Only hides here: the new tab is about to be focused by its opener.
        if notebook.n_pages() > 1 {
            for_added(notebook);
        }
    });
    notebook.connect_page_removed(move |notebook, _, _| sync(notebook));
}

/// `toggle_sftp` (doc 09) acts on the active tab's sidebar and is disabled
/// on pages without one — which is how non-SSH tabs hide the toggle.
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
    notebook.connect_switch_page(move |_, page, _| {
        for_state.set_enabled(SessionTab::sidebar_for_page(page).is_some());
    });
    action.set_enabled(false);
    window.add_action(&action);
    app.set_accels_for_action(&format!("win.{TOGGLE_SFTP_ACTION}"), &[TOGGLE_SFTP_ACCEL]);
}
