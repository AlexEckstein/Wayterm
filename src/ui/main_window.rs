//! The main window (doc 01): the side dock — its icon strip and panel —
//! beside the tab notebook, the status bar below. It also owns the
//! window-wide connection supervisor and the router that decides which
//! tab drives the status bar.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::gio;
use gtk4::prelude::*;

use crate::hotkeys::engine::{focus_within, ContextHost};
use crate::hotkeys::{ActionContext, HotkeyEngine};
use crate::store::SessionStore;
use crate::transport::connection::SshConnection;
use crate::transport::supervisor::ConnectionSupervisor;
use crate::transport::types::TransportState;
use crate::ui::macros_panel::MacrosPanel;
use crate::ui::session_tree::SessionTree;
use crate::ui::side_dock::SideDock;
use crate::ui::status_bar::StatusBar;
use crate::ui::tab::SessionTab;
use crate::ui::tab_restore::TabPersistence;
use crate::ui::widgets::{data, set_data};
use crate::ui::{settings, tab_resilience, terminal_actions, tunnels_dialog};

/// Window data keys.
pub(crate) const SUPERVISOR_KEY: &str = "wayterm-connection-supervisor";
const STATUS_ROUTER_KEY: &str = "wayterm-status-router";

const TOGGLE_SFTP_ACTION: &str = "toggle_sftp";
/// The dock panel's starting width: room for the SFTP sidebar's columns.
const DOCK_PANEL_WIDTH: i32 = 320;

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

        settings::general::apply(&store);

        let session_tree = {
            let (store, notebook) = (store.clone(), notebook.clone());
            SessionTree::new(store.clone(), move |id| match store.get_session(id) {
                Ok(config) => {
                    SessionTab::open(&notebook, &config, &store);
                }
                Err(err) => eprintln!("Wayterm: failed to load session {id}: {err}"),
            })
        };
        session_tree.refresh();
        let macros = MacrosPanel::new(store.clone(), &notebook);
        let dock = SideDock::new(&session_tree.widget(), macros.widget(), &notebook);

        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(DOCK_PANEL_WIDTH);
        paned.set_start_child(Some(dock.panel()));
        paned.set_end_child(Some(&notebook));
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        // The strip stays outside the paned, so dragging the panel shut
        // never hides the tabs.
        let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        body.append(dock.strip());
        body.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
        body.append(&paned);

        let status_bar = Rc::new(StatusBar::new());
        let router = Rc::new(RefCell::new(StatusRouter {
            notebook: notebook.clone(),
            status_bar: status_bar.clone(),
            last: None,
        }));
        let for_switch = router.clone();
        notebook.connect_switch_page(move |_, page, _| {
            let status = SessionTab::page_status(page).unwrap_or_default();
            for_switch
                .borrow_mut()
                .push(page, status.state, &status.host, status.port);
        });

        let tunnels_button = gtk4::Button::with_label("Tunnels…");
        tunnels_button.set_action_name(Some("win.tunnels"));
        tunnels_button.set_tooltip_text(Some(
            "View and edit this session's port forwards (live status when connected)",
        ));
        let settings_button = gtk4::Button::with_label("Settings");
        settings_button.set_action_name(Some("win.settings"));
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        header.set_halign(gtk4::Align::End);
        header.set_margin_top(4);
        header.set_margin_bottom(4);
        header.set_margin_end(8);
        header.append(&tunnels_button);
        header.append(&settings_button);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&body);
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

        install_toggle_sftp(&window, &notebook, &dock);
        install_tunnels_action(&window, &notebook, &store);
        terminal_actions::install(&window, &notebook);
        let for_actions = notebook.clone();
        tab_resilience::install_connection_actions(&window, move || {
            crate::ui::tab::current_tab_id(&for_actions)
        });
        // Every chord, `toggle_sftp`'s included, comes from the engine
        // (doc 07). Terminal shortcuts sit on the notebook but fire only
        // while the current tab's terminal has focus.
        let for_focus = notebook.clone();
        hotkeys.borrow_mut().install(
            app,
            vec![
                ContextHost::new(ActionContext::Terminal, &notebook).with_focus_check(move || {
                    SessionTab::active_terminal(&for_focus)
                        .is_some_and(|t| focus_within(&t.widget()))
                }),
                ContextHost::new(ActionContext::SessionTree, &session_tree.widget()),
            ],
        );

        // Settings on one of its pages; an import there refreshes the tree.
        let show_settings = {
            let (store, parent) = (store.clone(), window.clone());
            let tree = session_tree.clone();
            let on_import: Rc<dyn Fn()> = Rc::new(move || tree.refresh());
            move |page: &str| {
                settings::show_settings(
                    hotkeys.clone(),
                    store.clone(),
                    on_import.clone(),
                    page,
                    Some(parent.upcast_ref()),
                );
            }
        };
        let show_settings = Rc::new(show_settings);
        let open_settings = gio::SimpleAction::new("settings", None);
        let for_action = show_settings.clone();
        open_settings.connect_activate(move |_, _| for_action(settings::GENERAL_PAGE));
        window.add_action(&open_settings);

        // Restore before watching, so the restored tabs are not re-saved as
        // a burst of user actions. The close handler keeps the watcher alive
        // and writes the final state.
        let tabs = TabPersistence::new(notebook.clone(), home.upcast(), store.clone());
        tabs.restore();
        tabs.install();
        window.connect_close_request(move |_| {
            tabs.flush();
            gtk4::glib::Propagation::Proceed
        });

        window.present();
        // After presenting, so the offer opens over a visible window.
        if settings::backup::take_first_run_offer(&store) {
            show_settings(settings::BACKUP_PAGE);
        }
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

/// `win.tunnels` (Phase 11): the current tab's Tunnels dialog. It always
/// opens — with no engine (not connected, non-SSH, or no tab at all it is
/// a no-op) the dialog is the offline fallback: rules editable, live cells
/// reading "no active connection". A connected SSH tab passes its page's
/// live status source instead of `None`; the dialog already speaks it.
fn install_tunnels_action(
    window: &gtk4::ApplicationWindow,
    notebook: &gtk4::Notebook,
    store: &Rc<SessionStore>,
) {
    let action = gio::SimpleAction::new(tunnels_dialog::TUNNELS_ACTION, None);
    let (notebook, store, parent) = (notebook.clone(), store.clone(), window.clone());
    action.connect_activate(move |_, _| {
        let Some(id) = crate::ui::tab::current_session_id(&notebook) else {
            return;
        };
        let Ok(mut config) = store.get_session(id) else {
            return;
        };
        let source = SessionTab::current_forward_source(&notebook);
        let dialog = tunnels_dialog::TunnelsDialog::new(parent.upcast_ref(), &config, source);
        if let Some(rules) = dialog.run() {
            config.forward_rules = if rules.is_empty() { None } else { Some(rules) };
            if let Err(err) = store.update_session(&config) {
                eprintln!("Wayterm: failed to save tunnel rules: {err}");
            }
        }
    });
    window.add_action(&action);
}

/// `toggle_sftp` (doc 09) switches the dock to the SFTP panel, or back
/// from it, and is disabled on pages without a sidebar — which is how
/// non-SSH tabs hide the toggle. Its chord is the hotkey engine's.
fn install_toggle_sftp(
    window: &gtk4::ApplicationWindow,
    notebook: &gtk4::Notebook,
    dock: &SideDock,
) {
    let action = gio::SimpleAction::new(TOGGLE_SFTP_ACTION, None);
    let dock = dock.clone();
    action.connect_activate(move |_, _| dock.toggle_sftp());
    let for_state = action.clone();
    notebook.connect_switch_page(move |_, page, _| {
        for_state.set_enabled(SessionTab::sidebar_for_page(page).is_some());
    });
    action.set_enabled(false);
    window.add_action(&action);
}
