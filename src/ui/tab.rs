//! Session tab — the notebook page for one saved session.
//!
//! Each tab represents an open saved session. The tab label is a small box
//! containing the session name plus a close button; opening the same session
//! twice focuses the existing tab instead of duplicating it.
//!
//! # Page layout (doc 09)
//!
//! An SSH session's page is a [`gtk4::Paned`]: the [`SftpSidebar`] on the
//! left, the terminal on the right, user-resizable — doc 09 §Layout's
//! "sidebar left, terminal right". The paned belongs to the **tab**, not to
//! the window, because doc 09 §Connection Model gives each sidebar the
//! session's own shared `SshConnection`: two SSH tabs browse two servers
//! independently, so a single window-wide sidebar could not represent either.
//! Non-SSH session types get the bare page content instead — doc 09
//! §Availability restricts the sidebar to SSH sessions.
//!
//! The terminal side is still the Phase-2 placeholder label; task P3-2 only
//! adds the sidebar around it.

use gtk4::prelude::*;

use crate::store::model::SessionConfig;
use crate::ui::sftp_sidebar::{supports_sftp, SftpSidebar};

/// Key used to stash the session id on the tab content widget for dedup.
const SESSION_ID_KEY: &str = "wayterm-session-tab-id";

/// Key used to stash the sidebar on an SSH tab's page widget so the
/// `toggle_sftp` action can find it — the same widget-data pattern
/// `session_tree.rs` uses to recover its own widget from a `TreeView`.
const SIDEBAR_KEY: &str = "wayterm-sftp-sidebar";

/// Width of the sidebar when the tab is created, in pixels.
const SIDEBAR_WIDTH: i32 = 320;

/// A placeholder terminal tab for a saved session.
#[allow(dead_code)]
pub struct SessionTab {
    session_id: i64,
    widget: gtk4::Widget,
}

impl SessionTab {
    /// Open (or focus if already open) a tab for `session`.
    ///
    /// The tab label shows the session name and a close button. The page
    /// content is a centered label reading "terminal lands in Phase 2 — doc
    /// 04"; for SSH sessions it is wrapped in doc 09's paned with an SFTP
    /// sidebar on the left. Returns the tab content widget.
    pub fn open(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
        // Dedup: if a tab for this session already exists, focus it.
        if let Some((page_num, widget)) = find_tab_by_session_id(notebook, session.id) {
            notebook.set_current_page(Some(page_num));
            return widget;
        }

        let page = page_content(session);

        let tab_label = build_tab_label(notebook, session);
        let page_num = notebook.append_page(&page, Some(&tab_label));
        notebook.set_current_page(Some(page_num));

        page
    }

    /// The SFTP sidebar of the notebook's current page, if any.
    ///
    /// Doc 09's `toggle_sftp` acts on the **active** SSH tab, so this is what
    /// the window's toggle action resolves on every activation and on every
    /// page switch (which is also how the action knows to disable itself on
    /// RDP/VNC/serial/local pages — doc 09 §Availability).
    pub fn active_sidebar(notebook: &gtk4::Notebook) -> Option<SftpSidebar> {
        let page_num = notebook.current_page()?;
        let page = notebook.nth_page(Some(page_num))?;
        Self::sidebar_for_page(&page)
    }

    /// The SFTP sidebar of the page widget `page` belongs to, if it has one.
    pub fn sidebar_for_page(page: &gtk4::Widget) -> Option<SftpSidebar> {
        unsafe {
            page.data::<SftpSidebar>(SIDEBAR_KEY)
                .map(|data| data.as_ref().clone())
        }
    }
}

/// Build a tab's page: the terminal side, plus the SFTP sidebar and its paned
/// for SSH sessions.
fn page_content(session: &SessionConfig) -> gtk4::Widget {
    let placeholder = gtk4::Label::new(Some(&format!(
        "{}\nterminal lands in Phase 2 — doc 04",
        session.name
    )));
    placeholder.set_vexpand(true);
    placeholder.set_hexpand(true);
    placeholder.set_valign(gtk4::Align::Center);
    placeholder.set_halign(gtk4::Align::Center);
    placeholder.set_justify(gtk4::Justification::Center);

    let page: gtk4::Widget = if supports_sftp(&session.session_type) {
        let sidebar = SftpSidebar::new();
        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(SIDEBAR_WIDTH);
        paned.set_start_child(Some(sidebar.widget()));
        paned.set_end_child(Some(&placeholder));
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        // Stash the sidebar on the page so `toggle_sftp` (and the connect
        // flow, once it has a connection) can reach it later.
        unsafe {
            paned
                .upcast_ref::<gtk4::Widget>()
                .set_data(SIDEBAR_KEY, sidebar);
        }
        paned.upcast()
    } else {
        placeholder.upcast()
    };

    // Tag the content widget with the session id so we can find it again for
    // dedup and close bookkeeping. This must be the page widget — for SSH
    // tabs that is the paned, not the placeholder inside it.
    unsafe {
        page.set_data(SESSION_ID_KEY, session.id);
    }
    page
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
        if let Some((page_num, _)) = find_tab_by_session_id(&notebook_for_close, session_id) {
            notebook_for_close.remove_page(Some(page_num));
        }
    });

    box_.append(&close_button);
    box_.upcast()
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
