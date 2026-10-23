//! The main window (doc 01): the side dock — its icon strip and panel —
//! beside the tab notebook, the status bar below. It also owns the
//! window-wide connection supervisor and the router that decides which
//! tab drives the status bar.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::gio;
use gtk4::prelude::*;

use crate::hotkeys::engine::{focus_owns_keys, ContextHost};
use crate::hotkeys::{ActionContext, HotkeyEngine};
use crate::store::SessionStore;
use crate::transport::connection::SshConnection;
use crate::transport::supervisor::ConnectionSupervisor;
use crate::transport::types::TransportState;
use crate::ui::dialogs::tunnels;
use crate::ui::panels::macros_panel::MacrosPanel;
use crate::ui::panels::session_tree::SessionTree;
use crate::ui::panels::sftp_sidebar::SftpButton;
use crate::ui::panels::side_dock::SideDock;
use crate::ui::status_bar::StatusBar;
use crate::ui::tabs::restore::TabPersistence;
use crate::ui::tabs::{self, terminal_actions};
use crate::ui::tabs::{current_page, SessionTab};
use crate::ui::widgets::{data, set_data};
use crate::ui::{appearance, settings};

/// Window data keys.
pub(crate) const SUPERVISOR_KEY: &str = "wayterm-connection-supervisor";
const STATUS_ROUTER_KEY: &str = "wayterm-status-router";

const TOGGLE_SFTP_ACTION: &str = "toggle_sftp";
/// The `sftp` context's actions and the toolbar button each presses.
const SFTP_ACTIONS: [(&str, SftpButton); 4] = [
    ("sftp_upload", SftpButton::Upload),
    ("sftp_download", SftpButton::Download),
    ("sftp_rename", SftpButton::Rename),
    ("sftp_delete", SftpButton::Delete),
];
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
        if current.as_ref() == Some(page) {
            self.show(page, state, host, port);
        }
    }

    /// Show `page`'s state as the current page's. `switch-page` calls this
    /// directly: GTK emits it before the notebook makes `page` current.
    fn show(&mut self, page: &gtk4::Widget, state: TransportState, host: &str, port: u16) {
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
        tabs::bar::install(&notebook, &store);

        appearance::track(&notebook);
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
                .show(page, status.state, &status.host, status.port);
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
        tabs::actions::install(&window, &notebook, home.upcast_ref(), &store);
        install_view_actions(&window, &notebook, &dock, &session_tree);
        let for_actions = notebook.clone();
        tabs::resilience::install_connection_actions(&window, move || {
            crate::ui::tabs::current_tab_id(&for_actions)
        });
        // Every chord comes from the engine (doc 11 §Dispatch), checked
        // against its context at each key press. Terminal shortcuts fire
        // whenever the current tab has a terminal, unless the user is
        // typing elsewhere.
        let for_terminal = notebook.clone();
        let unhandled = hotkeys.borrow_mut().install(
            &window,
            vec![
                ContextHost::new(ActionContext::Terminal, move || {
                    SessionTab::active_terminal(&for_terminal).is_some()
                        && !focus_owns_keys(&for_terminal)
                }),
                ContextHost::focus_within(ActionContext::SessionTree, &session_tree.widget()),
                ContextHost::focus_within(ActionContext::Sftp, dock.sftp_panel()),
            ],
        );
        if !unhandled.is_empty() {
            eprintln!(
                "Wayterm: shortcuts with no action behind them: {}",
                unhandled.join(", ")
            );
        }

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

/// `win.tunnels`: the current session tab's Tunnels dialog, live when the
/// tab's forward engine is armed, offline otherwise (no tab: no-op). OK
/// saves the rules to the session row.
fn install_tunnels_action(
    window: &gtk4::ApplicationWindow,
    notebook: &gtk4::Notebook,
    store: &Rc<SessionStore>,
) {
    let action = gio::SimpleAction::new(tunnels::TUNNELS_ACTION, None);
    let (notebook, store, parent) = (notebook.clone(), store.clone(), window.clone());
    action.connect_activate(move |_, _| {
        let Some(id) = crate::ui::tabs::current_session_id(&notebook) else {
            return;
        };
        let Ok(mut config) = store.get_session(id) else {
            return;
        };
        let source = SessionTab::current_forward_source(&notebook);
        let dialog = tunnels::TunnelsDialog::new(parent.upcast_ref(), &config, source);
        if let Some(rules) = dialog.run() {
            config.forward_rules = if rules.is_empty() { None } else { Some(rules) };
            if let Err(err) = store.update_session(&config) {
                eprintln!("Wayterm: failed to save tunnel rules: {err}");
            }
        }
    });
    window.add_action(&action);
}

/// The window actions for the view, the session tree and the SFTP sidebar
/// (doc 11 §Handlers). Session-tree and SFTP actions act on what their
/// panel shows; the engine fires them only while focus is in that panel.
fn install_view_actions(
    window: &gtk4::ApplicationWindow,
    notebook: &gtk4::Notebook,
    dock: &SideDock,
    tree: &SessionTree,
) {
    let add = |id: &str, run: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(id, None);
        action.connect_activate(move |_, _| run());
        window.add_action(&action);
    };

    let weak = window.downgrade();
    add(
        "toggle_fullscreen",
        Box::new(move || {
            if let Some(window) = weak.upgrade() {
                window.set_fullscreened(!window.is_fullscreen());
            }
        }),
    );
    let (for_toggle, for_focus, notebook_for_toggle) =
        (dock.clone(), tree.clone(), notebook.clone());
    add(
        "toggle_session_tree",
        Box::new(move || {
            if for_toggle.toggle_sessions() {
                for_focus.focus();
            } else if let Some(terminal) = SessionTab::active_terminal(&notebook_for_toggle) {
                // The hidden panel took focus with it.
                terminal.widget().grab_focus();
            }
        }),
    );

    tree.install_actions(window);

    for (id, button) in SFTP_ACTIONS {
        let notebook = notebook.clone();
        add(
            id,
            Box::new(move || {
                let sidebar = current_page(&notebook)
                    .as_ref()
                    .and_then(SessionTab::sidebar_for_page);
                if let Some(sidebar) = sidebar {
                    sidebar.press(button);
                }
            }),
        );
    }
}

/// `toggle_sftp` (doc 07) switches the dock to the SFTP panel, or back
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

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use std::time::{Duration, Instant};

    use gtk4::glib;

    use super::*;
    use crate::hotkeys::DEFAULT_ACTIONS;
    use crate::ui::dialogs::host_picker::LOCAL_SHELL_ID;
    use crate::ui::tabs::current_session_id;
    use crate::ui::tabs::state::TabState;

    /// The first `T` at or below `widget`, depth first.
    fn find<T: IsA<gtk4::Widget>>(widget: &gtk4::Widget) -> Option<T> {
        if let Some(found) = widget.downcast_ref::<T>() {
            return Some(found.clone());
        }
        let mut child = widget.first_child();
        while let Some(node) = child {
            if let Some(found) = find(&node) {
                return Some(found);
            }
            child = node.next_sibling();
        }
        None
    }

    fn toplevel(title: &str) -> Option<gtk4::Window> {
        gtk4::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<gtk4::Window>().ok())
            .find(|w| w.is_visible() && w.title().as_deref() == Some(title))
    }

    /// The status bar's text: the last label in the window's bottom row.
    fn status_text(window: &gtk4::ApplicationWindow) -> String {
        window
            .child()
            .and_then(|root| root.last_child())
            .and_then(|bar| bar.last_child())
            .and_downcast::<gtk4::Label>()
            .expect("the status label")
            .text()
            .to_string()
    }

    fn spin_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Every action in doc 11's table has a window action behind it and a
    /// shortcut in the window's dispatcher; the tab actions move between
    /// tabs; terminal shortcuts keep firing with focus off the terminal;
    /// the tab bar's "+" opens a local shell; and a local-shell tab
    /// restores as one.
    pub(in crate::ui) fn every_shortcut_has_an_action() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let _entered = runtime.enter();
        let app = gtk4::Application::new(
            Some("com.alex-eckstein.wayterm.window-test"),
            gio::ApplicationFlags::NON_UNIQUE,
        );
        app.register(None::<&gio::Cancellable>).expect("register");
        let store = Rc::new(SessionStore::in_memory().expect("in-memory store"));
        store
            .set_setting(tabs::resilience::CONFIRM_CLOSE_SETTING, "0")
            .unwrap();
        store
            .set_setting(settings::backup::IMPORT_OFFERED_KEY, "1")
            .unwrap();
        store
            .save_open_tabs(&[TabState::session(LOCAL_SHELL_ID, "Local shell", 0, 0)])
            .unwrap();
        store
            .insert_session(&crate::store::model::SessionConfig {
                name: "box".into(),
                session_type: crate::store::model::SessionType::LocalShell,
                local_shell: Some("/bin/sh".into()),
                ..Default::default()
            })
            .unwrap();

        let main = MainWindow::new(&app, store, Rc::new(RefCell::new(HotkeyEngine::new())));
        let window = main.window().clone();
        let act = |id: &str| gio::prelude::ActionGroupExt::activate_action(&window, id, None);
        for default in DEFAULT_ACTIONS {
            assert!(
                window.has_action(default.id),
                "{} has no action",
                default.id
            );
        }
        let dispatcher = (0..window.observe_controllers().n_items())
            .filter_map(|i| window.observe_controllers().item(i))
            .filter_map(|c| c.downcast::<gtk4::ShortcutController>().ok())
            .find(|c| c.name().as_deref() == Some(crate::hotkeys::engine::DISPATCHER_NAME))
            .expect("the dispatcher");
        assert_eq!(dispatcher.n_items() as usize, DEFAULT_ACTIONS.len());
        let shortcut_for = |accel: &str| {
            let (key, mods) = gtk4::accelerator_parse(accel).unwrap();
            let accel = gtk4::accelerator_name(key, mods).to_string();
            (0..dispatcher.n_items())
                .filter_map(|i| dispatcher.item(i).and_downcast::<gtk4::Shortcut>())
                .find(|s| s.trigger().is_some_and(|t| t.to_str() == accel))
                .expect("a shortcut for the chord")
        };
        let fires = |accel: &str| {
            shortcut_for(accel).action().unwrap().activate(
                gtk4::ShortcutActionFlags::empty(),
                &window,
                None::<&glib::Variant>,
            )
        };

        // The saved local shell came back as a local shell, not detached.
        let notebook = find::<gtk4::Notebook>(window.upcast_ref()).expect("the notebook");
        assert_eq!(notebook.n_pages(), 2, "Home and the restored tab");
        assert_eq!(current_session_id(&notebook), Some(LOCAL_SHELL_ID));

        // New Tab: the picker, and its first row opens another local shell.
        act("new_tab");
        let picker = toplevel("New Tab").expect("the host picker");
        let list = find::<gtk4::ListBox>(picker.upcast_ref()).expect("the host list");
        list.row_at_index(0).expect("local shell").emit_activate();
        assert_eq!(notebook.n_pages(), 3);
        assert_eq!(notebook.current_page(), Some(2));

        // Next/previous wrap round the two tabs; Home is never one of them.
        act("goto_tab_1");
        assert_eq!(notebook.current_page(), Some(1));
        act("next_tab");
        assert_eq!(notebook.current_page(), Some(2));
        act("next_tab");
        assert_eq!(notebook.current_page(), Some(1), "wraps");
        act("prev_tab");
        assert_eq!(notebook.current_page(), Some(2), "wraps back");
        act("goto_tab_3");
        assert_eq!(notebook.current_page(), Some(2), "no third tab");

        // Terminal shortcuts fire with focus anywhere but a text field:
        // the bug was copy/paste dying once a click moved focus off VTE.
        spin_until("the local shell connects", || {
            SessionTab::active_terminal(&notebook).is_some()
        });
        let terminal = SessionTab::active_terminal(&notebook).unwrap().widget();
        terminal.grab_focus();
        assert!(fires("<Control><Shift>c"), "copy, terminal focused");
        assert!(!fires("Delete"), "a session-tree chord, focus elsewhere");
        notebook.grab_focus();
        assert!(
            fires("<Control><Shift>c"),
            "copy after clicking the tab bar"
        );
        let tree = find::<gtk4::ListView>(window.upcast_ref()).expect("the tree");
        tree.grab_focus();
        assert!(fires("<Control><Shift>c"), "copy with the tree focused");
        // With no row selected the row actions are off, so their keys reach
        // the list (Return activates its focused row); selected, Return is
        // connect_session.
        assert!(!fires("<Control>e"), "edit with nothing selected");
        assert!(!fires("Return"), "Return with nothing selected");
        tree.model()
            .and_downcast::<gtk4::SingleSelection>()
            .expect("the tree's selection")
            .set_selected(0);
        assert!(fires("Return"), "connect_session on the selected row");
        assert_eq!(notebook.n_pages(), 4, "the session opened a tab");

        // Rename prompts for the current tab (closed by a timer: the prompt
        // runs its own loop).
        glib::timeout_add_local_once(Duration::from_millis(50), || {
            if let Some(prompt) = toplevel("Rename Tab") {
                prompt.close();
            }
        });
        act("rename_tab");

        // Toggle Session Tree hides the dock's panel, then brings it back.
        let panel = find::<gtk4::Paned>(window.upcast_ref())
            .and_then(|paned| paned.start_child())
            .expect("the dock panel");
        assert!(panel.is_visible());
        act("toggle_session_tree");
        assert!(!panel.is_visible());
        act("toggle_session_tree");
        assert!(panel.is_visible());

        // The tab bar's "+" opens a local shell straight away, no picker.
        notebook
            .action_widget(gtk4::PackType::End)
            .and_then(|end| find::<gtk4::Button>(&end))
            .expect("the + button")
            .emit_clicked();
        assert_eq!(notebook.n_pages(), 5);
        assert_eq!(notebook.current_page(), Some(4));
        assert_eq!(current_session_id(&notebook), Some(LOCAL_SHELL_ID));
        assert!(toplevel("New Tab").is_none(), "the picker stays shut");

        // The status bar follows the current tab. Close Tab closes tabs,
        // never Home, and with no tabs left the status bar reads
        // Disconnected again.
        spin_until("the new shell shows as connected", || {
            status_text(&window).starts_with("Connected")
        });
        act("goto_tab_3");
        spin_until("the box session's shell shows", || {
            status_text(&window).starts_with("Connected to /bin/sh")
        });
        for _ in 0..4 {
            act("close_tab");
        }
        assert_eq!(notebook.n_pages(), 1);
        assert_eq!(status_text(&window), "Disconnected");
        act("close_tab");
        assert_eq!(notebook.n_pages(), 1, "Home stays");

        for _ in 0..20 {
            glib::MainContext::default().iteration(false);
        }
        window.destroy();
        // Leave the process-wide settings as the other widget tests expect.
        tabs::resilience::install_settings(true, true);
    }
}
