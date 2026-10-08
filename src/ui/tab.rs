//! Placeholder session tab — Phase 2 seam.
//!
//! Each tab represents an open saved session.  For Phase 1 the content is
//! just a centered label; Phase 2 will replace it with a real terminal
//! backend (doc 04).  The tab label is a small box containing the session
//! name plus a close button; opening the same session twice focuses the
//! existing tab instead of duplicating it.
//!
//! P4-PAR-2 additions (additive only, P3-3 owns the connect flow):
//! [`SessionTab::attach_terminal`] / [`SessionTab::active_terminal`] are the
//! seam the connect flow uses to publish a tab's terminal backend and the
//! window-level terminal actions use to find it;
//! [`SessionTab::apply_terminal_title`] is the doc-02 `TitleChanged` seam.

use std::sync::Arc;

use gtk4::prelude::*;

use crate::store::model::SessionConfig;
use crate::terminal::TerminalBackend;
use crate::ui::tab_label;

/// Key used to stash the session id on the tab content widget for dedup.
const SESSION_ID_KEY: &str = "wayterm-session-tab-id";

/// Key used to stash the tab's wired terminal backend (an
/// `Arc<dyn TerminalBackend>`) on the tab content widget.
const TERMINAL_BACKEND_KEY: &str = "wayterm-terminal-backend";

/// A placeholder terminal tab for a saved session.
#[allow(dead_code)]
pub struct SessionTab {
    session_id: i64,
    widget: gtk4::Widget,
}

impl SessionTab {
    /// Open (or focus if already open) a placeholder tab for `session`.
    ///
    /// The tab label shows the session name and a close button.  The tab
    /// content is a centered label reading "terminal lands in Phase 2 —
    /// doc 04".  Returns the tab content widget.
    pub fn open(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
        // Dedup: if a tab for this session already exists, focus it.
        if let Some((page_num, widget)) = find_tab_by_session_id(notebook, session.id) {
            notebook.set_current_page(Some(page_num));
            return widget;
        }

        let placeholder = gtk4::Label::new(Some(&format!(
            "{}\nterminal lands in Phase 2 — doc 04",
            session.name
        )));
        placeholder.set_vexpand(true);
        placeholder.set_hexpand(true);
        placeholder.set_valign(gtk4::Align::Center);
        placeholder.set_halign(gtk4::Align::Center);
        placeholder.set_justify(gtk4::Justification::Center);

        // Tag the content widget with the session id so we can find it
        // again for dedup and close bookkeeping.
        unsafe {
            placeholder.set_data(SESSION_ID_KEY, session.id);
        }

        let tab_label = build_tab_label(notebook, session);
        let page_num = notebook.append_page(&placeholder, Some(&tab_label));
        notebook.set_current_page(Some(page_num));

        placeholder.upcast()
    }

    /// Publish `backend` as the terminal of the tab whose content widget is
    /// `content` (the connect flow's side of the seam; P3 calls this once a
    /// session is wired). The window-level terminal actions read it back
    /// through [`SessionTab::active_terminal`].
    // The connect flow (P3) is the caller; it lands in that lane.
    #[allow(dead_code)]
    pub fn attach_terminal(content: &gtk4::Widget, backend: Arc<dyn TerminalBackend>) {
        // SAFETY: the key and value type have a 1:1 relationship; the qdata
        // (and its Arc) is dropped with the widget.
        unsafe {
            content.set_data(TERMINAL_BACKEND_KEY, backend);
        }
    }

    /// The terminal backend wired to the notebook's current tab, if the
    /// connect flow has attached one yet. `None` for the Home page and for
    /// placeholder tabs — window-level terminal actions must treat that as
    /// "nothing to act on" (doc 07 context=terminal).
    pub fn active_terminal(notebook: &gtk4::Notebook) -> Option<Arc<dyn TerminalBackend>> {
        let page = notebook.current_page()?;
        let content = notebook.nth_page(Some(page))?;
        // SAFETY: only `attach_terminal` writes this key, with this type.
        unsafe { content.data::<Arc<dyn TerminalBackend>>(TERMINAL_BACKEND_KEY) }
            .map(|ptr| unsafe { (*ptr.as_ptr()).clone() })
    }

    /// Doc 02 `TitleChanged` seam: feed a terminal-provided window title
    /// into the active tab's title state machine. Ignored when the user has
    /// already renamed the tab (see [`tab_label::TabTitle`]). Returns
    /// whether the title took effect.
    ///
    /// Not called yet — the pump drops `TerminalEvent::TitleChanged` today;
    /// the consumer wiring lands with the connect flow.
    #[allow(dead_code)]
    pub fn apply_terminal_title(notebook: &gtk4::Notebook, title: &str) -> bool {
        let Some(page) = notebook.current_page() else {
            return false;
        };
        let Some(content) = notebook.nth_page(Some(page)) else {
            return false;
        };
        let Some(label) = notebook.tab_label(&content) else {
            return false;
        };
        tab_label::apply_terminal_title(&label, title)
    }
}

/// Build the tab label widget: session name + close button.
fn build_tab_label(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
    let notebook_for_close = notebook.clone();
    let session_id = session.id;
    tab_label::build_tab_label(&session.name, move || {
        if let Some((page_num, _)) = find_tab_by_session_id(&notebook_for_close, session_id) {
            notebook_for_close.remove_page(Some(page_num));
        }
    })
}

/// Find the notebook page whose content widget carries `session_id`.
fn find_tab_by_session_id(
    notebook: &gtk4::Notebook,
    session_id: i64,
) -> Option<(u32, gtk4::Widget)> {
    let n_pages = notebook.n_pages();
    for page_num in 0..n_pages {
        let child = notebook.nth_page(Some(page_num))?;
        let id: Option<i64> = unsafe { child.data::<i64>(SESSION_ID_KEY).map(|d| *d.as_ptr()) };
        if id == Some(session_id) {
            return Some((page_num, child));
        }
    }
    None
}
