//! Placeholder session tab — Phase 2 seam.
//!
//! Each tab represents an open saved session.  For Phase 1 the content is
//! just a centered label; Phase 2 will replace it with a real terminal
//! backend (doc 04).  The tab label is a small box containing the session
//! name plus a close button; opening the same session twice focuses the
//! existing tab instead of duplicating it.

use gtk4::prelude::*;

use crate::store::model::SessionConfig;

/// Key used to stash the session id on the tab content widget for dedup.
const SESSION_ID_KEY: &str = "wayterm-session-tab-id";

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
}

/// Build the tab label widget: session name + close button.
fn build_tab_label(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);

    let name_label = gtk4::Label::new(Some(&session.name));
    box_.append(&name_label);

    let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
    close_button.set_has_frame(false);
    close_button.set_tooltip_text(Some("Close tab"));
    close_button.set_focus_on_click(false);

    let notebook_for_close = notebook.clone();
    let session_id = session.id;
    close_button.connect_clicked(move |_| {
        // Doc 04 §Lifecycle: "Confirm-before-close if processes running
        // (setting `confirm_close`)". The decision and the dialog live in
        // `tab_resilience` (this file is owned by the connect-flow lane);
        // the only hook here is routing the click through it.
        let parent = gtk_window_for(&notebook_for_close.clone().upcast());
        let notebook_for_close_button = notebook_for_close.clone();
        crate::ui::tab_resilience::request_close(session_id, parent.as_ref(), move || {
            if let Some((page_num, _)) =
                find_tab_by_session_id(&notebook_for_close_button, session_id)
            {
                notebook_for_close_button.remove_page(Some(page_num));
            }
        });
    });

    box_.append(&close_button);
    box_.upcast()
}

/// The session id of the notebook's current page, if it is a session tab.
///
/// The reconnect / disconnect actions (doc 07) resolve their target tab
/// through this; a non-session page (the Home tab) yields `None` and the
/// actions no-op.
pub(crate) fn current_session_id(notebook: &gtk4::Notebook) -> Option<i64> {
    let page_num = notebook.current_page()?;
    let child = notebook.nth_page(Some(page_num))?;
    #[allow(deprecated)]
    let id: Option<i64> = unsafe { child.data::<i64>(SESSION_ID_KEY).map(|d| *d.as_ptr()) };
    id
}

/// Walk up from a widget to the [`gtk4::Window`] that owns it, so a
/// dialog opened from a tab's close button gets the right transient
/// parent.
#[allow(deprecated)]
fn gtk_window_for(widget: &gtk4::Widget) -> Option<gtk4::Window> {
    let mut current = widget.clone();
    loop {
        if let Ok(window) = current.clone().downcast::<gtk4::Window>() {
            return Some(window);
        }
        current = current.parent()?;
    }
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
