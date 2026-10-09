//! The terminal-context window actions (doc 07 §Default Actions): copy,
//! paste, select all, clear, and the two searches, applied to the active
//! tab's terminal. The routing is display-free; the hotkey engine owns the
//! chords, so the actions are registered without accels.

use std::rc::Rc;
use std::sync::{Arc, Weak};

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::terminal::TerminalBackend;
use crate::ui::tab::SessionTab;
use crate::ui::widgets::{data, set_data, set_margins};

/// The action ids, all doc 07 terminal-context defaults.
pub const TERMINAL_ACTIONS: [&str; 6] = [
    "copy",
    "paste",
    "select_all",
    "clear",
    "search",
    "search_scrollback",
];

/// Payloads some actions need: clipboard text for `paste`, the query and
/// direction for the searches.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalActionInputs<'a> {
    pub clipboard: Option<&'a str>,
    pub search: Option<&'a str>,
    pub search_reverse: bool,
}

/// An action id resolved to the backend call it makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAction {
    Copy,
    Paste(String),
    SelectAll,
    Clear,
    Search { text: String, reverse: bool },
}

impl TerminalAction {
    pub fn apply(&self, backend: &dyn TerminalBackend) {
        match self {
            Self::Copy => backend.copy_selection(),
            Self::Paste(text) => backend.paste(text),
            Self::SelectAll => backend.select_all(),
            Self::Clear => backend.clear(),
            Self::Search { text, reverse } => backend.search(text, *reverse),
        }
    }
}

/// Resolve an action id. `None` — nothing to do — for unknown ids and for
/// an empty clipboard or query.
pub fn route_terminal_action(
    id: &str,
    inputs: &TerminalActionInputs<'_>,
) -> Option<TerminalAction> {
    let non_empty = |text: Option<&str>| text.filter(|t| !t.is_empty()).map(str::to_string);
    match id {
        "copy" => Some(TerminalAction::Copy),
        "paste" => non_empty(inputs.clipboard).map(TerminalAction::Paste),
        "select_all" => Some(TerminalAction::SelectAll),
        "clear" => Some(TerminalAction::Clear),
        "search" | "search_scrollback" => {
            non_empty(inputs.search).map(|text| TerminalAction::Search {
                text,
                reverse: inputs.search_reverse,
            })
        }
        _ => None,
    }
}

/// The two searches share one popover and differ only here: reopening
/// with a query resumes backwards for `search_scrollback`.
pub fn initial_search_reverse(id: &str) -> bool {
    id == "search_scrollback"
}

fn search(backend: &dyn TerminalBackend, text: &str, reverse: bool) {
    let inputs = TerminalActionInputs {
        search: Some(text),
        search_reverse: reverse,
        ..Default::default()
    };
    if let Some(action) = route_terminal_action("search", &inputs) {
        action.apply(backend);
    }
}

/// Register the six actions on `window`; each is a no-op when the active
/// tab has no terminal.
pub fn install(window: &gtk4::ApplicationWindow, notebook: &gtk4::Notebook) {
    for id in TERMINAL_ACTIONS {
        let action = gio::SimpleAction::new(id, None);
        let notebook = notebook.clone();
        let window_for_action = window.clone();
        action.connect_activate(move |_, _| {
            let Some(backend) = SessionTab::active_terminal(&notebook) else {
                return;
            };
            match id {
                "paste" => paste_from_clipboard(&window_for_action, backend),
                "search" | "search_scrollback" => open_search(backend, id),
                id => {
                    if let Some(action) = route_terminal_action(id, &Default::default()) {
                        action.apply(&*backend);
                    }
                }
            }
        });
        window.add_action(&action);
    }
}

/// Paste the clipboard into the terminal (never into a focused entry).
fn paste_from_clipboard(window: &gtk4::ApplicationWindow, backend: Arc<dyn TerminalBackend>) {
    let clipboard = window.clipboard();
    glib::MainContext::default().spawn_local(async move {
        if let Ok(Some(text)) = clipboard.read_text_future().await {
            let inputs = TerminalActionInputs {
                clipboard: Some(&text),
                ..Default::default()
            };
            if let Some(action) = route_terminal_action("paste", &inputs) {
                action.apply(&*backend);
            }
        }
    });
}

const SEARCH_POPOVER_KEY: &str = "wayterm-search-popover";

/// One search popover per terminal widget: Enter searches forward,
/// Shift+Enter backwards, Escape closes.
struct SearchPopover {
    popover: gtk4::Popover,
    entry: gtk4::Entry,
    /// Weak: the popover lives in the widget's data, so a strong reference
    /// to the backend (which owns the widget) would leak every closed tab.
    backend: Weak<dyn TerminalBackend>,
}

fn open_search(backend: Arc<dyn TerminalBackend>, id: &str) {
    let anchor = backend.widget();
    let popover = data::<Rc<SearchPopover>>(&anchor, SEARCH_POPOVER_KEY)
        .unwrap_or_else(|| build_search_popover(&anchor, &backend));
    popover.popover.popup();
    popover.entry.grab_focus();
    let text = popover.entry.text();
    if !text.is_empty() {
        search(&*backend, &text, initial_search_reverse(id));
    }
}

fn build_search_popover(
    anchor: &gtk4::Widget,
    backend: &Arc<dyn TerminalBackend>,
) -> Rc<SearchPopover> {
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Search terminal"));
    set_margins(&entry, 6);
    let popover = gtk4::Popover::new();
    popover.set_child(Some(&entry));
    popover.set_autohide(true);
    popover.set_parent(anchor);
    let search_popover = Rc::new(SearchPopover {
        popover,
        entry: entry.clone(),
        backend: Arc::downgrade(backend),
    });

    // Plain Enter is the entry's `activate` (it runs before bubble-phase
    // controllers); the key controller sees Shift+Enter and Escape.
    let for_activate = search_popover.clone();
    entry.connect_activate(move |entry| {
        if let Some(backend) = for_activate.backend.upgrade() {
            search(&*backend, &entry.text(), false);
        }
    });
    let keys = gtk4::EventControllerKey::new();
    let for_keys = search_popover.clone();
    keys.connect_key_pressed(move |_, key, _, state| {
        let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
        if enter && state.contains(gdk::ModifierType::SHIFT_MASK) {
            if let Some(backend) = for_keys.backend.upgrade() {
                search(&*backend, &for_keys.entry.text(), true);
            }
            glib::Propagation::Stop
        } else if key == gdk::Key::Escape {
            for_keys.popover.popdown();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    entry.add_controller(keys);

    set_data(anchor, SEARCH_POPOVER_KEY, search_popover.clone());
    search_popover
}
