//! Local shell and serial tabs: nothing to dial or authenticate — no
//! supervisor, host key or credential machinery (doc 06). The transport
//! starts when the terminal is wired. A local shell's sidebar then browses
//! this machine's files (doc 07 §Local tabs).

use std::rc::Rc;

use gtk4::glib;

use super::terminal::finish_connect;
use super::{current_gen, ssh};
use crate::store::model::SessionConfig;

/// Connect `session` for attempt `generation`: straight to the terminal.
pub(super) fn start(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    generation: u64,
) {
    let (notebook, page, session) = (notebook.clone(), page.clone(), session.clone());
    glib::MainContext::default().spawn_local(async move {
        if current_gen(&page) == generation {
            finish_connect(&notebook, &session, &page, None).await;
        }
    });
}

/// The terminal is wired: the page's sidebar, if it has one (a serial page
/// has none), browses from the directory the shell starts in and follows
/// the shell from there.
pub(super) fn on_connect(page: &gtk4::Widget, session: &SessionConfig, terminal: &gtk4::Widget) {
    let Some(sidebar) = ssh::sidebar_of(page).filter(|sidebar| sidebar.is_local()) else {
        return;
    };
    sidebar.attach_local_on_connect(&shell_start_dir(session));
    ssh::mark_sidebar_attached(page);
    ssh::follow_terminal_directory(page, terminal);
}

/// Where `LocalTransport` starts the shell: the session's working
/// directory, else Wayterm's own (doc 06 §Local shell).
fn shell_start_dir(session: &SessionConfig) -> String {
    let own = std::env::current_dir().unwrap_or_else(|_| "/".into());
    // Not trimmed: the transport takes the directory as it is written.
    let configured = session.local_working_dir.as_deref();
    let dir = match configured.filter(|dir| !dir.is_empty()) {
        Some(dir) => own.join(dir),
        None => own,
    };
    dir.to_string_lossy().into_owned()
}
