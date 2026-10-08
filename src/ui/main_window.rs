//! Main application window — layout only (doc 01 / doc 10 Phase 1).
//!
//! The high-level structure is a vertical [`gtk4::Box`] holding a
//! horizontal [`gtk4::Paned`] (session-tree sidebar on the left,
//! [`gtk4::Notebook`] on the right) with a [`StatusBar`] pinned to the
//! bottom.
//!
//! It also owns the two window-wide singletons the connect flow needs
//! (task P3-3): the [`ConnectionSupervisor`] and the [`StatusRouter`] that
//! decides which tab's transport state the status bar shows.
//!
//! P4-PAR-2 (additive): registers the six terminal actions from doc 07
//! §Default Actions on the window and routes them to the active tab's
//! terminal — see [`install_terminal_actions`] and the display-free
//! routing table below.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Weak};

use gio::prelude::*;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::terminal::TerminalBackend;
use crate::transport::supervisor::ConnectionSupervisor;
use crate::transport::types::TransportState;
use crate::ui::session_tree::SessionTree;
use crate::ui::status_bar::StatusBar;
use crate::ui::tab::SessionTab;
use crate::ui::tab_restore::TabPersistence;

/// gio action id for doc 09's `toggle_sftp`.
const TOGGLE_SFTP_ACTION: &str = "toggle_sftp";

/// Doc 07's default key for `toggle_sftp` (`<Control><Shift>B`).
const TOGGLE_SFTP_ACCEL: &str = "<Control><Shift>b";

/// Key under which the status bar is stashed on the window, following the
/// same pattern as `tab.rs`'s session-id key.
const STATUS_BAR_KEY: &str = "wayterm-status-bar";

/// Key under which the app-wide connection supervisor is stashed on the
/// window (doc 04 §Connection Supervisor; deliverable P3-3 1a).
pub(crate) const SUPERVISOR_KEY: &str = "wayterm-connection-supervisor";

/// Key under which the status router is stashed on the window.
pub(crate) const STATUS_ROUTER_KEY: &str = "wayterm-status-router";

/// Shared handle to the window's [`StatusRouter`] — main-thread only, so a
/// plain `Rc<RefCell<_>>` is the right owner (same pattern as the SFTP
/// sidebar stash in `tab.rs`).
pub(crate) type StatusRouterCell = Rc<RefCell<StatusRouter>>;

/// Routes transport-state updates to the status bar: only the notebook's
/// **current** page may drive it (doc 10 / P3-3 deliverable 1g), and an
/// identical (page, state) pair is never pushed twice in a row so the
/// elapsed-time timer does not restart for nothing.
///
/// Held on the window; every tab's state-drain future and the notebook's
/// `switch_page` handler go through it.
pub(crate) struct StatusRouter {
    notebook: gtk4::Notebook,
    status_bar: Rc<StatusBar>,
    /// Last (page, state) actually pushed — the dedup key.
    last: Option<(gtk4::Widget, TransportState)>,
}

impl StatusRouter {
    pub(crate) fn new(notebook: gtk4::Notebook, status_bar: Rc<StatusBar>) -> Self {
        Self {
            notebook,
            status_bar,
            last: None,
        }
    }

    /// Push `state` for `page` if the page is current and the pair changed.
    ///
    /// Background pages are silently ignored here — their state is already
    /// stored in their [`crate::ui::tab::PageStatus`] stash, which the
    /// `switch_page` handler replays when the page becomes current.
    pub(crate) fn push(
        &mut self,
        page: &gtk4::Widget,
        state: TransportState,
        host: &str,
        port: u16,
    ) {
        let current = self
            .notebook
            .current_page()
            .and_then(|num| self.notebook.nth_page(Some(num)));
        if current.as_ref() != Some(page) {
            return;
        }
        if let Some((ref last_page, ref last_state)) = self.last {
            if last_page == page && *last_state == state {
                return;
            }
        }
        self.status_bar.set_state(&state, host, port);
        self.last = Some((page.clone(), state));
    }
}

/// Store `state` for `page` and route it to the status bar (no-op when the
/// window has no router — display-free contexts).
///
/// `state` is already stashed in the page's `PageStatus` by the caller;
/// this only handles the routing half.
pub(crate) fn route_status(
    window: &gtk4::ApplicationWindow,
    page: &gtk4::Widget,
    state: TransportState,
    host: &str,
    port: u16,
) {
    let Some(router) = (unsafe {
        window
            .data::<StatusRouterCell>(STATUS_ROUTER_KEY)
            .map(|data| data.as_ref().clone())
    }) else {
        return;
    };
    router.borrow_mut().push(page, state, host, port);
}

/// Qdata key under which a terminal widget carries its search popover (an
/// `Rc<SearchPopover>`), so repeated `search` activations reuse it.
const SEARCH_POPOVER_KEY: &str = "wayterm-search-popover";

// ─── Terminal actions (doc 07 §Default Actions, context = terminal) ────────

/// The six terminal-context actions: `(action id, default chord)`, exactly
/// as doc 07's table lists them.
///
/// **No accels are registered here.** The chords are recorded so the
/// hotkey-core lane can bind `Action::current_key` from one source of
/// truth; doc 07's conflict engine owns the actual binding (Tier-2,
/// context=terminal), and registering window accels now would double-fire
/// with the engine later. `tests/terminal_polish.rs` locks this table.
pub const TERMINAL_ACTION_TABLE: [(&str, &str); 6] = [
    ("copy", "Ctrl+Shift+C"),
    ("paste", "Ctrl+Shift+V"),
    ("select_all", "Ctrl+Shift+A"),
    ("clear", "Ctrl+Shift+L"),
    ("search", "Ctrl+Shift+F"),
    ("search_scrollback", "Ctrl+Shift+K"),
];

/// Payloads the GTK glue must gather before a doc-07 action id can run:
/// the clipboard text for `paste`, the popover query for `search*`.
/// `search_reverse` is the direction of the step (`Enter` → false,
/// `Shift+Enter` → true).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalActionInputs<'a> {
    pub clipboard: Option<&'a str>,
    pub search: Option<&'a str>,
    pub search_reverse: bool,
}

/// A doc-07 action id resolved to the backend call it performs.
/// Display-free: built by [`route_terminal_action`], applied by
/// [`TerminalAction::apply`] — shared by the window glue and the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAction {
    Copy,
    Paste(String),
    SelectAll,
    Clear,
    Search { text: String, reverse: bool },
}

impl TerminalAction {
    /// Perform the action on `backend` (doc 02 interaction/scrollback
    /// methods).
    pub fn apply(&self, backend: &dyn TerminalBackend) {
        match self {
            TerminalAction::Copy => backend.copy_selection(),
            TerminalAction::Paste(text) => backend.paste(text),
            TerminalAction::SelectAll => backend.select_all(),
            TerminalAction::Clear => backend.clear(),
            TerminalAction::Search { text, reverse } => backend.search(text, *reverse),
        }
    }
}

/// The action-to-terminal routing table: a doc-07 action id plus its
/// payload resolves to the [`TerminalAction`] it should perform.
///
/// Returns `None` for unknown ids or missing/empty payloads (no clipboard
/// content → nothing to paste; empty query → nothing to search), so callers
/// can treat `None` as "nothing to do" and never panic.
pub fn route_terminal_action(
    id: &str,
    inputs: &TerminalActionInputs<'_>,
) -> Option<TerminalAction> {
    match id {
        "copy" => Some(TerminalAction::Copy),
        "paste" => inputs
            .clipboard
            .filter(|text| !text.is_empty())
            .map(|text| TerminalAction::Paste(text.to_string())),
        "select_all" => Some(TerminalAction::SelectAll),
        "clear" => Some(TerminalAction::Clear),
        "search" | "search_scrollback" => {
            inputs
                .search
                .filter(|text| !text.is_empty())
                .map(|text| TerminalAction::Search {
                    text: text.to_string(),
                    reverse: inputs.search_reverse,
                })
        }
        _ => None,
    }
}

/// Doc 07 / deliverable 5: `search` and `search_scrollback` share one
/// popover; the actions differ only in the direction of the initial query
/// when the popover is (re)opened with text already in the entry —
/// `search_scrollback` resumes backwards into the scrollback.
pub fn initial_search_reverse(id: &str) -> bool {
    matches!(id, "search_scrollback")
}

/// Resolve and perform `id` on `backend`; a `None` route is a silent no-op.
fn apply_route(id: &str, inputs: &TerminalActionInputs<'_>, backend: &dyn TerminalBackend) {
    if let Some(action) = route_terminal_action(id, inputs) {
        action.apply(backend);
    }
}

/// The search popover attached to one terminal widget (deliverable 5):
/// an entry where Enter searches forward, Shift+Enter backwards, Escape
/// closes. One instance per terminal — cached in qdata on the widget.
struct SearchPopover {
    popover: gtk4::Popover,
    entry: gtk4::Entry,
    /// Direction used when the popover is opened with a query already in
    /// the entry (`initial_search_reverse` of the action that opened it).
    initial_reverse: Cell<bool>,
    /// Deliberately **weak**: the popover lives in the widget's qdata, so a
    /// strong `Arc` back to the backend (which owns that widget) would form
    /// a refcount cycle and leak the terminal on every closed tab. While
    /// the tab is open, the content page's qdata holds the strong `Arc`.
    backend: Weak<dyn TerminalBackend>,
}

/// Register the six doc-07 terminal actions on the window as plain gio
/// actions — no accelerators (the hotkey engine binds the chords later).
///
/// context=terminal semantics: every activation resolves the *active tab's*
/// terminal first and is a silent no-op when no tab carries a wired
/// terminal (the Home page, or placeholder tabs before the P3 connect flow
/// calls [`SessionTab::attach_terminal`]).
fn install_terminal_actions(window: &gtk4::ApplicationWindow, notebook: &gtk4::Notebook) {
    for (action_id, _default_chord) in TERMINAL_ACTION_TABLE {
        let action = gio::SimpleAction::new(action_id, None);
        let notebook = notebook.clone();
        let window_for_action = window.clone();
        action.connect_activate(move |_, _| {
            let Some(backend) = SessionTab::active_terminal(&notebook) else {
                return;
            };
            match action_id {
                "paste" => paste_via_clipboard(&window_for_action, backend),
                "search" | "search_scrollback" => open_search_popover(backend, action_id),
                id => apply_route(id, &TerminalActionInputs::default(), &*backend),
            }
        });
        // `win.<id>` — the window's ActionMap namespace; the hotkey engine
        // later points doc-07 chords at these paths.
        window.add_action(&action);
    }
}

/// `paste`: read the clipboard (async, never blocking the main thread) and
/// route it to the terminal backend — not to any focused GtkEntry.
fn paste_via_clipboard(window: &gtk4::ApplicationWindow, backend: Arc<dyn TerminalBackend>) {
    let clipboard = window.clipboard();
    let backend_for_paste = backend.clone();
    gtk4::glib::MainContext::default().spawn_local(async move {
        if let Ok(Some(text)) = clipboard.read_text_future().await {
            apply_route(
                "paste",
                &TerminalActionInputs {
                    clipboard: Some(&text),
                    ..TerminalActionInputs::default()
                },
                &*backend_for_paste,
            );
        }
    });
}

/// `search` / `search_scrollback`: show the popover on the active terminal.
/// Both actions share one popover (deliverable 5); they differ only in the
/// direction of an initial query already sitting in the entry.
fn open_search_popover(backend: Arc<dyn TerminalBackend>, action_id: &str) {
    let anchor = backend.widget();
    let search = match unsafe { anchor.data::<Rc<SearchPopover>>(SEARCH_POPOVER_KEY) } {
        Some(ptr) => unsafe { (*ptr.as_ptr()).clone() },
        None => build_search_popover(&anchor, backend.clone()),
    };
    // Strong only for this activation; the cached handle stays weak.
    let Some(backend) = search.backend.upgrade() else {
        return;
    };

    search
        .initial_reverse
        .set(initial_search_reverse(action_id));
    search.popover.popup();
    search.entry.grab_focus();

    // Re-opening with a query still in the entry resumes the search in the
    // action's initial direction (this is the whole `search` vs
    // `search_scrollback` distinction).
    let text = search.entry.text();
    if !text.is_empty() {
        apply_route(
            action_id,
            &TerminalActionInputs {
                search: Some(&text),
                search_reverse: search.initial_reverse.get(),
                ..TerminalActionInputs::default()
            },
            &*backend,
        );
    }
}

/// Build (once per terminal widget) the search popover: an entry where
/// Enter → forward, Shift+Enter → reverse, Escape → close.
fn build_search_popover(
    anchor: &gtk4::Widget,
    backend: Arc<dyn TerminalBackend>,
) -> Rc<SearchPopover> {
    let popover = gtk4::Popover::new();
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Search terminal"));
    entry.set_margin_top(6);
    entry.set_margin_bottom(6);
    entry.set_margin_start(6);
    entry.set_margin_end(6);
    popover.set_child(Some(&entry));
    popover.set_autohide(true);
    // Popover is a GtkNative (GTK4): it draws in its own surface and only
    // needs the anchor for positioning, so the VTE widget works as parent.
    popover.set_parent(anchor);

    let search = Rc::new(SearchPopover {
        popover: popover.clone(),
        entry: entry.clone(),
        initial_reverse: Cell::new(false),
        backend: Arc::downgrade(&backend),
    });

    let keys = gtk4::EventControllerKey::new();
    let search_for_keys = search.clone();
    keys.connect_key_pressed(move |_, key, _, state| {
        // Plain Return never reaches this controller: GTK activates
        // `Entry::activate` (a key binding) in the *target* phase, before
        // bubble controllers — so plain forward search is handled there.
        // This controller only sees what the Entry does not bind: Shift+Enter
        // (reverse step) and Escape (close).
        if key == gtk4::gdk::Key::Return || key == gtk4::gdk::Key::KP_Enter {
            if state.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
                let Some(backend) = search_for_keys.backend.upgrade() else {
                    return gtk4::glib::Propagation::Proceed;
                };
                let text = search_for_keys.entry.text();
                apply_route(
                    "search",
                    &TerminalActionInputs {
                        search: Some(&text),
                        search_reverse: true,
                        ..TerminalActionInputs::default()
                    },
                    &*backend,
                );
                return gtk4::glib::Propagation::Stop;
            }
            return gtk4::glib::Propagation::Proceed;
        }
        if key == gtk4::gdk::Key::Escape {
            search_for_keys.popover.popdown();
            return gtk4::glib::Propagation::Stop;
        }
        gtk4::glib::Propagation::Proceed
    });
    entry.add_controller(keys);

    // Plain Enter: the Entry's `activate` signal (target phase — fires even
    // though `activates_default` is unset; that property only gates the
    // default widget).
    let search_for_activate = search.clone();
    entry.connect_activate(move |e| {
        let Some(backend) = search_for_activate.backend.upgrade() else {
            return;
        };
        let text = e.text();
        apply_route(
            "search",
            &TerminalActionInputs {
                search: Some(&text),
                search_reverse: false,
                ..TerminalActionInputs::default()
            },
            &*backend,
        );
    });

    // SAFETY: key/value have a 1:1 relationship; dropped with the anchor.
    unsafe {
        anchor.set_data(SEARCH_POPOVER_KEY, search.clone());
    }
    search
}

/// The primary Wayterm application window.
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

        // Doc 04 §Lifecycle reads `confirm_close` / `reconnect_on_drop`
        // from the settings table (doc 06 §Default Data). Resolve them once
        // here, before the store moves into the session tree — this is the
        // only place the window holds it. A missing or unreadable row keeps
        // the documented default (ON).
        crate::ui::tab_resilience::install_settings(
            store.flag(crate::ui::tab_resilience::CONFIRM_CLOSE_SETTING),
            store.flag(crate::ui::tab_resilience::RECONNECT_ON_DROP_SETTING),
        );

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
        // the router below drives it from the active tab's transport.
        let status_bar = Rc::new(StatusBar::new());
        status_bar.set_state(&TransportState::Disconnected, "", 0);

        // Window-wide status routing: page switches replay each page's
        // stashed state; live updates only pass through when their page
        // is the current one (P3-3 deliverable 1g).
        let router = Rc::new(RefCell::new(StatusRouter::new(
            notebook.clone(),
            status_bar.clone(),
        )));
        {
            let router_for_switch = router.clone();
            notebook.connect_switch_page(move |_notebook, page, _page_num| {
                let (state, host, port) = match SessionTab::page_status(page) {
                    Some(cell) => {
                        let status = cell.borrow();
                        (status.state.clone(), status.host.clone(), status.port)
                    }
                    // The Home page (and any other status-less page) shows
                    // the neutral startup state.
                    None => (TransportState::Disconnected, String::new(), 0),
                };
                router_for_switch
                    .borrow_mut()
                    .push(page, state, &host, port);
            });
        }

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
        // Keep the status bar alive for the window's lifetime; retrieve it
        // with `window.data::<Rc<StatusBar>>("wayterm-status-bar")` to drive
        // `set_state` directly when routing is not involved.
        unsafe {
            window.set_data(STATUS_BAR_KEY, status_bar.clone());
        }
        // App-wide connection supervisor (doc 04 §Connection Supervisor):
        // one authenticated session per (host, port, username), shared by
        // every tab and SFTP sidebar, released when the last consumer drops
        // it (P3-3 deliverable 1a).
        let supervisor: Arc<ConnectionSupervisor> = Arc::new(ConnectionSupervisor::new());
        unsafe {
            window.set_data(SUPERVISOR_KEY, supervisor);
            window.set_data(STATUS_ROUTER_KEY, router);
        }

        install_toggle_sftp(app, &window, &notebook);

        // Terminal actions (doc 07): registered with no accels; they act on
        // the active tab's terminal once the connect flow attaches one.
        install_terminal_actions(&window, &notebook);

        // Doc 07 §Default Actions: `reconnect` (Ctrl+Shift+R) and
        // `disconnect` (Ctrl+Shift+X) resolve against the active tab.
        // Registered unbound — the hotkey lane (doc 07's
        // ShortcutController) owns the accelerators, so the two lanes do
        // not both claim the chords.
        let notebook_for_actions = notebook.clone();
        crate::ui::tab_resilience::install_connection_actions(&window, move || {
            crate::ui::tab::current_session_id(&notebook_for_actions)
        });

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
