//! The page's status views, shared by every tab type: connecting,
//! disconnected and the error view with Retry and Close Tab.

use std::rc::Rc;

use gtk4::prelude::*;

use super::terminal::close_terminal;
use super::{bump_gen, close_tab, publish_status, settle_redial, start_connect};
use crate::store::model::SessionConfig;
use crate::transport::types::{TransportError, TransportState};
use crate::ui::widgets::{centered_label, set_margins};

/// Make `child` the page's only child, filling it.
// used by ui::tabs::vnc and ui::tabs::rdp
pub(crate) fn set_terminal_area(page: &gtk4::Widget, child: &impl IsA<gtk4::Widget>) {
    let Some(page) = page.downcast_ref::<gtk4::Box>() else {
        return;
    };
    while let Some(old) = page.first_child() {
        page.remove(&old);
    }
    child.set_hexpand(true);
    child.set_vexpand(true);
    page.append(child);
}

/// A centered column: the layout of every status view.
fn status_column() -> gtk4::Box {
    let view = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    view.set_vexpand(true);
    view.set_hexpand(true);
    view.set_valign(gtk4::Align::Center);
    view.set_halign(gtk4::Align::Center);
    view
}

/// `text` over a Cancel button that runs `on_cancel`: a dial in progress.
/// The button lives inside the page, so `on_cancel` must hold the page
/// weakly, or a tab closed while connecting is never freed.
// used by ui::tabs::rdp
pub(crate) fn connecting_view(text: &str, on_cancel: impl Fn() + 'static) -> gtk4::Widget {
    let view = status_column();
    view.append(&centered_label(text));
    let cancel = gtk4::Button::with_label("Cancel");
    cancel.set_halign(gtk4::Align::Center);
    cancel.connect_clicked(move |_| on_cancel());
    view.append(&cancel);
    view.upcast()
}

/// A heading over a wrapped, selectable `detail`, then `buttons` in a
/// centered row. Handlers on the buttons must hold the page weakly (see
/// [`connecting_view`]).
// used by ui::tabs::rdp
pub(crate) fn notice_view(heading: &str, detail: &str, buttons: &[gtk4::Button]) -> gtk4::Widget {
    let view = status_column();
    set_margins(&view, 24);
    let title = gtk4::Label::new(Some(heading));
    title.add_css_class("heading");
    let text = gtk4::Label::new(Some(detail));
    text.set_wrap(true);
    text.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    text.set_selectable(true);
    text.set_justify(gtk4::Justification::Center);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.set_halign(gtk4::Align::Center);
    for button in buttons {
        row.append(button);
    }
    view.append(&title);
    view.append(&text);
    view.append(&row);
    view.upcast()
}

/// "Connecting to host:port…" with a Cancel that drops the attempt.
// used by ui::tabs::vnc
pub(crate) fn show_connecting_view(page: &gtk4::Widget, host: &str, port: u16) {
    let (weak, host_for_cancel) = (page.downgrade(), host.to_string());
    let view = connecting_view(&format!("Connecting to {host}:{port}…"), move || {
        if let Some(page) = weak.upgrade() {
            bump_gen(&page);
            close_terminal(&page);
            show_disconnected(&page, &host_for_cancel, port);
        }
    });
    set_terminal_area(page, &view);
}

/// The attempt ended cleanly (a cancelled prompt, Cancel): "Disconnected".
// used by ui::tabs::vnc and ui::tabs::rdp
pub(crate) fn show_disconnected(page: &gtk4::Widget, host: &str, port: u16) {
    set_terminal_area(page, &centered_label("Disconnected"));
    publish_status(page, TransportState::Disconnected, host, port);
}

/// The attempt failed: the error view, and the error on the status bar.
// used by ui::tabs::vnc and ui::tabs::rdp
pub(crate) fn show_failure(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    message: &str,
    host: &str,
    port: u16,
) {
    show_error_view(notebook, page, session, message);
    publish_status(page, TransportState::Error(message.to_string()), host, port);
}

/// The error message with Retry and Close Tab.
// used by ui::tabs::vnc and ui::tabs::rdp
pub(crate) fn show_error_view(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    message: &str,
) {
    settle_redial(
        page,
        Err(TransportError::ConnectionFailed(message.to_string())),
    );
    // An error can arrive after the terminal was built.
    close_terminal(page);

    let retry = gtk4::Button::with_label("Retry");
    retry.add_css_class("suggested-action");
    {
        let (notebook, page, session) = (notebook.clone(), page.downgrade(), session.clone());
        retry.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                start_connect(&notebook, &page, &session);
            }
        });
    }
    let close = gtk4::Button::with_label("Close Tab");
    {
        let (notebook, page) = (notebook.clone(), page.downgrade());
        close.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                close_tab(&notebook, &page);
            }
        });
    }
    let view = notice_view("Connection failed", message, &[retry, close]);
    set_terminal_area(page, &view);
}
