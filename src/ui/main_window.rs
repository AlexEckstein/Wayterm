//! Main application window — layout only (doc 01 / doc 10 Phase 1).
//!
//! The high-level structure is a vertical [`gtk4::Box`] holding a
//! horizontal [`gtk4::Paned`] (session-tree sidebar on the left,
//! [`gtk4::Notebook`] on the right) with a [`StatusBar`] pinned to the
//! bottom.
//!
//! P4-PAR-2 (additive): registers the six terminal actions from doc 07
//! §Default Actions on the window and routes them to the active tab's
//! terminal — see [`install_terminal_actions`] and the display-free
//! routing table below.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use gio::prelude::*;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::terminal::TerminalBackend;
use crate::transport::types::TransportState;
use crate::ui::tab::SessionTab;
use crate::ui::{SessionTree, StatusBar};

/// Key under which the status bar is stashed on the window, following the
/// same pattern as `tab.rs`'s session-id key.
const STATUS_BAR_KEY: &str = "wayterm-status-bar";

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

        // Terminal actions (doc 07): registered with no accels; they act on
        // the active tab's terminal once the connect flow attaches one.
        install_terminal_actions(&window, &notebook);

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
