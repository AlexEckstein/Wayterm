//! Small GTK helpers shared by the dialogs, tabs and sidebars.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};

/// The window a widget lives in — the transient parent for its dialogs.
pub fn window_of(widget: &impl IsA<gtk4::Widget>) -> Option<gtk4::Window> {
    widget.root()?.downcast::<gtk4::Window>().ok()
}

/// A clone of the value stored on `widget` under `key`, if any. Each key
/// must only ever be stored with one type.
pub fn data<T: Clone + 'static>(widget: &impl IsA<glib::Object>, key: &str) -> Option<T> {
    // SAFETY: every key in the crate is written by `set_data` with a single
    // type, which is the type it is read back with.
    unsafe { widget.data::<T>(key).map(|value| value.as_ref().clone()) }
}

/// Store `value` on `widget` under `key`; it is dropped with the widget.
pub fn set_data<T: 'static>(widget: &impl IsA<glib::Object>, key: &str, value: T) {
    // SAFETY: see `data`.
    unsafe { widget.set_data(key, value) }
}
/// The value under `key`, first storing `T::default()` if there is none.
/// Meant for shared handles (`Rc<Cell<_>>`, `Rc<RefCell<_>>`), where the
/// returned clone and the stored value are the same slot.
pub fn data_or_default<T: Clone + Default + 'static>(
    widget: &impl IsA<glib::Object>,
    key: &str,
) -> T {
    data(widget, key).unwrap_or_else(|| {
        let value = T::default();
        set_data(widget, key, value.clone());
        value
    })
}

/// Set the same margin on all four sides.
pub fn set_margins(widget: &impl IsA<gtk4::Widget>, margin: i32) {
    widget.set_margin_top(margin);
    widget.set_margin_bottom(margin);
    widget.set_margin_start(margin);
    widget.set_margin_end(margin);
}

/// A label centered in all the space it is given.
pub fn centered_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_vexpand(true);
    label.set_hexpand(true);
    label.set_valign(gtk4::Align::Center);
    label.set_halign(gtk4::Align::Center);
    label.set_justify(gtk4::Justification::Center);
    label
}

/// A left-aligned label that wraps at word or character boundaries.
pub fn wrapped_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .xalign(0.0)
        .build()
}

/// A start-aligned, wrapping note, dimmed.
pub fn dim_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    label.add_css_class("dim-label");
    label
}

/// An empty start-aligned, wrapping label styled as an error, for inline
/// validation messages.
pub fn error_label() -> gtk4::Label {
    let label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    label.add_css_class("error");
    label
}

/// `buttons` in a right-aligned row, in the given order.
pub fn button_row(buttons: &[&gtk4::Button]) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.set_halign(gtk4::Align::End);
    for button in buttons {
        row.append(*button);
    }
    row
}

/// A modal window built by the caller: buttons write their answer into
/// `result` and close the window.
pub struct Modal<T> {
    /// The modal window.
    pub window: gtk4::ApplicationWindow,
    /// The answer, once a button wrote it.
    pub result: Rc<RefCell<Option<T>>>,
}

impl<T: 'static> Modal<T> {
    /// A modal window titled `title`, transient for `parent`.
    pub fn new(title: &str, parent: Option<&gtk4::Window>) -> Self {
        let window = gtk4::ApplicationWindow::builder()
            .title(title)
            .modal(true)
            .build();
        if let Some(parent) = parent {
            window.set_transient_for(Some(parent));
            window.set_destroy_with_parent(true);
        }
        Self {
            window,
            result: Rc::default(),
        }
    }

    /// A button that stores `answer()` and closes the window.
    pub fn button(&self, label: &str, answer: impl Fn() -> Option<T> + 'static) -> gtk4::Button {
        let button = gtk4::Button::with_label(label);
        let (window, result) = (self.window.clone(), self.result.clone());
        button.connect_clicked(move |_| {
            *result.borrow_mut() = answer();
            window.close();
        });
        button
    }

    /// Present the window and run a nested main loop until it closes, so
    /// the caller stays synchronous. `None` = closed without an answer.
    pub fn run(self) -> Option<T> {
        let done = Rc::new(std::cell::Cell::new(false));
        let closed = done.clone();
        self.window.connect_close_request(move |window| {
            closed.set(true);
            window.set_visible(false);
            glib::Propagation::Proceed
        });
        self.window.present();
        while !done.get() {
            glib::MainContext::default().iteration(true);
        }
        self.result.take()
    }
}

/// `[Cancel] [OK]`, right-aligned, OK as the default widget.
pub fn ok_cancel_row<T: 'static>(
    modal: &Modal<T>,
    ok: impl Fn() -> Option<T> + 'static,
) -> gtk4::Box {
    let ok = modal.button("OK", ok);
    let cancel = modal.button("Cancel", || None);
    modal.window.set_default_widget(Some(&ok));
    button_row(&[&cancel, &ok])
}

/// Ask for one line of text. Enter confirms, Escape cancels.
pub fn prompt_text(parent: Option<&gtk4::Window>, title: &str, initial: &str) -> Option<String> {
    let modal = Modal::new(title, parent);
    modal.window.set_default_width(320);
    modal.window.set_resizable(false);

    let entry = gtk4::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    entry.set_hexpand(true);
    // Plain GTK4 windows do not close on Escape.
    let escape = gtk4::EventControllerKey::new();
    let window = modal.window.clone();
    escape.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            window.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    entry.add_controller(escape);

    let text = entry.clone();
    let buttons = ok_cancel_row(&modal, move || Some(text.text().to_string()));
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&vbox, 12);
    vbox.append(&entry);
    vbox.append(&buttons);
    modal.window.set_child(Some(&vbox));
    modal.run()
}

/// `[Cancel] [accept]` alert; `on_accept` runs only if the user accepts.
pub fn confirm(
    parent: Option<&gtk4::Window>,
    message: &str,
    detail: &str,
    accept: &str,
    on_accept: impl FnOnce() + 'static,
) {
    let dialog = gtk4::AlertDialog::builder()
        .message(message)
        .detail(detail)
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", accept]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(1);
    dialog.choose(parent, gio::Cancellable::NONE, move |answer| {
        if matches!(answer, Ok(1)) {
            on_accept();
        }
    });
}

/// [`confirm`] with a "Do not show me again" check box under the detail;
/// `on_accept` runs only if the user accepts, and gets whether the box was
/// ticked. Cancel, Escape or closing the window answers no.
pub fn confirm_with_opt_out(
    parent: Option<&gtk4::Window>,
    message: &str,
    detail: &str,
    accept: &str,
    on_accept: impl FnOnce(bool) + 'static,
) {
    let window = gtk4::Window::builder()
        .title(message)
        .modal(true)
        .resizable(false)
        .default_width(420)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
        window.set_destroy_with_parent(true);
    }

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&content, 18);
    content.append(&wrapped_label(detail));
    let opt_out = gtk4::CheckButton::with_label("Do not show me again");
    content.append(&opt_out);
    let cancel = gtk4::Button::with_label("Cancel");
    let accept = gtk4::Button::with_label(accept);
    content.append(&button_row(&[&cancel, &accept]));
    window.set_child(Some(&content));
    window.set_default_widget(Some(&accept));

    let for_cancel = window.clone();
    cancel.connect_clicked(move |_| for_cancel.close());
    let for_accept = window.clone();
    let on_accept = std::cell::Cell::new(Some(on_accept));
    accept.connect_clicked(move |_| {
        for_accept.close();
        if let Some(on_accept) = on_accept.take() {
            on_accept(opt_out.is_active());
        }
    });
    // Plain GTK4 windows do not close on Escape.
    let escape = gtk4::EventControllerKey::new();
    let for_escape = window.clone();
    escape.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(escape);
    window.present();
}
