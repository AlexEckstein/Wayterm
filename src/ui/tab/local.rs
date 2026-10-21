//! Local shell and serial tabs: nothing to dial or authenticate — no
//! supervisor, host key or credential machinery (doc 04 §Local and serial
//! transports). The transport starts when the terminal is wired.

use std::rc::Rc;

use gtk4::glib;

use super::current_gen;
use super::terminal::finish_connect;
use crate::store::model::SessionConfig;

/// Connect `session` for attempt `gen`: straight to the terminal.
pub(super) fn start(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    gen: u64,
) {
    let (notebook, page, session) = (notebook.clone(), page.clone(), session.clone());
    glib::MainContext::default().spawn_local(async move {
        if current_gen(&page) == gen {
            finish_connect(&notebook, &session, &page, None).await;
        }
    });
}
