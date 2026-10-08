//! Tab label widget — session name + close button, with in-place rename
//! (doc 10 Phase 4 "Tab rename (double-click tab label)").
//!
//! Split out of `tab.rs` so the rename/state-machine work stays additive to
//! the connect-flow rewrite happening there (file ownership, P4-PAR-2).
//!
//! The label keeps a per-tab [`TabTitle`] state machine (placeholder → set →
//! renamed) reachable through qdata on the returned widget:
//!
//! * **Placeholder** — initial state, value = the session name from the store.
//! * **Set** — a programmatic title landed (doc 02's `TitleChanged` event
//!   from the terminal, wired later via [`apply_terminal_title`]).
//! * **Renamed** — the user double-clicked and renamed the tab; later
//!   programmatic titles are ignored so the remote's OSC title churn cannot
//!   clobber an explicit user choice.
//!
//! The state machine itself is display-free and unit-tested in
//! `tests/terminal_polish.rs`; only the widget glue needs a display.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

/// Qdata key under which the tab label widget carries its
/// [`Rc<TabLabelState>`].
const TAB_TITLE_KEY: &str = "wayterm-tab-title";

/// Tab title lifecycle states (doc 02 `TitleChanged` seam).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabTitleState {
    /// Initial: only the session name from the store is known.
    Placeholder,
    /// A programmatic title has been applied (terminal `TitleChanged`).
    Set,
    /// The user renamed the tab; programmatic titles no longer win.
    Renamed,
}

/// Display-free tab title state machine.
#[derive(Debug)]
pub struct TabTitle {
    state: TabTitleState,
    value: String,
}

impl TabTitle {
    /// A new title in the [`TabTitleState::Placeholder`] state.
    pub fn new(placeholder: &str) -> Self {
        Self {
            state: TabTitleState::Placeholder,
            value: placeholder.to_string(),
        }
    }

    /// Apply a programmatic title (terminal `TitleChanged`, session load).
    /// Ignored once the user renamed the tab; returns whether it took effect.
    pub fn set(&mut self, title: &str) -> bool {
        if self.state == TabTitleState::Renamed || self.value == title {
            return false;
        }
        self.value = title.to_string();
        self.state = TabTitleState::Set;
        true
    }

    /// Apply a user rename (double-click). Empty names are rejected —
    /// a tab must keep a label. Always transitions to
    /// [`TabTitleState::Renamed`]; returns whether it took effect.
    pub fn rename(&mut self, title: &str) -> bool {
        if title.is_empty() || self.value == title {
            return false;
        }
        self.value = title.to_string();
        self.state = TabTitleState::Renamed;
        true
    }

    /// Current lifecycle state.
    // Read by the state-machine tests; consumers of a live tab go through
    // `value()` (the rendered label).
    #[allow(dead_code)]
    pub fn state(&self) -> &TabTitleState {
        &self.state
    }

    /// The title as shown in the tab label.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Widget-side holder: the [`TabTitle`] model plus the [`gtk4::Label`] it
/// renders into. Stored as `Rc<TabLabelState>` in qdata on the tab label.
pub struct TabLabelState {
    title: RefCell<TabTitle>,
    label: gtk4::Label,
}

impl TabLabelState {
    /// Programmatic title update (doc 02 `TitleChanged` seam): refreshes the
    /// visible label only when the model accepted the value.
    pub fn set(&self, title: &str) -> bool {
        let changed = self.title.borrow_mut().set(title);
        if changed {
            self.label.set_text(self.title.borrow().value());
        }
        changed
    }

    /// User rename: refreshes the label and locks the title as `Renamed`.
    pub fn rename(&self, title: &str) -> bool {
        let changed = self.title.borrow_mut().rename(title);
        if changed {
            self.label.set_text(self.title.borrow().value());
        }
        changed
    }

    /// Current title text.
    pub fn value(&self) -> String {
        self.title.borrow().value().to_string()
    }
}

/// The [`TabLabelState`] attached to `label_widget` (the widget returned by
/// [`build_tab_label`]), if any.
pub fn title_state_of(label_widget: &gtk4::Widget) -> Option<Rc<TabLabelState>> {
    // SAFETY: the key is only ever written by `build_tab_label` with this
    // exact type.
    unsafe { label_widget.data::<Rc<TabLabelState>>(TAB_TITLE_KEY) }
        .map(|p| unsafe { (*p.as_ptr()).clone() })
}

/// Doc 02 `TitleChanged` seam: push a terminal-provided title into the tab
/// label's state machine. Returns `false` when the label carries no title
/// state or the title was ignored (tab already user-renamed).
pub fn apply_terminal_title(label_widget: &gtk4::Widget, title: &str) -> bool {
    match title_state_of(label_widget) {
        Some(state) => state.set(title),
        None => false,
    }
}

/// Build the tab label widget: session name (double-click to rename) plus
/// the close button. `on_close` runs when the close button is clicked —
/// the caller owns page bookkeeping.
pub fn build_tab_label(
    session_name: &str,
    on_close: impl Fn() + 'static,
) -> (gtk4::Widget, gtk4::Label) {
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);

    let name_label = gtk4::Label::new(Some(session_name));
    box_.append(&name_label);

    let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
    close_button.set_has_frame(false);
    close_button.set_tooltip_text(Some("Close tab"));
    close_button.set_focus_on_click(false);
    close_button.connect_clicked(move |_| on_close());
    box_.append(&close_button);

    // Per-tab title state, reachable through the notebook's tab label.
    let state = Rc::new(TabLabelState {
        title: RefCell::new(TabTitle::new(session_name)),
        label: name_label.clone(),
    });
    // SAFETY: value and key have a 1:1 relationship; the qdata is dropped
    // with the widget.
    unsafe {
        box_.set_data(TAB_TITLE_KEY, state.clone());
    }

    // Double-click on the NAME part starts the rename prompt; the close
    // button is a sibling and unaffected.
    let click = gtk4::GestureClick::new();
    click.set_button(1); // primary button only
    let label_for_rename = name_label.clone();
    let state_for_rename = state.clone();
    click.connect_pressed(move |gesture, n_press, _x, _y| {
        if n_press != 2 {
            return;
        }
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        let parent = label_for_rename
            .root()
            .and_then(|root| root.downcast::<gtk4::Window>().ok());
        let current = state_for_rename.value();
        if let Some(new_name) = prompt_text(parent.as_ref(), "Rename Tab", &current) {
            state_for_rename.rename(&new_name);
        }
    });
    name_label.add_controller(click);

    // The name label is returned too: the terminal-title wiring (doc 02's
    // TitleChanged seam) must reach the label that displays it, and the
    // tab.rs caller carries both through `build_tab_label`'s contract.
    (box_.upcast(), name_label)
}

/// Ask the user for a short text string in a small modal window.
///
/// Returns `Some(value)` if the user confirmed, `None` if cancelled —
/// Enter commits (the OK button is the default widget), Escape cancels.
/// Nested-iteration pattern copied from `session_tree::prompt_text`: the
/// dialog runs its own main-context loop so the caller stays synchronous.
fn prompt_text(parent: Option<&gtk4::Window>, title: &str, initial: &str) -> Option<String> {
    let result = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let done = std::rc::Rc::new(std::cell::RefCell::new(false));

    let dialog = gtk4::ApplicationWindow::builder()
        .title(title)
        .modal(true)
        .default_width(320)
        .default_height(120)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        dialog.set_transient_for(Some(p));
        dialog.set_destroy_with_parent(true);
    }

    let entry = gtk4::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    entry.set_margin_top(12);
    entry.set_margin_bottom(12);
    entry.set_margin_start(12);
    entry.set_margin_end(12);
    entry.set_hexpand(true);

    // Escape cancels: plain GTK4 windows do not close on Escape (only
    // GtkDialog did in GTK3), so bind it explicitly.
    let escape = gtk4::EventControllerKey::new();
    let dialog_for_escape = dialog.clone();
    escape.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            dialog_for_escape.close();
            return gtk4::glib::Propagation::Stop;
        }
        gtk4::glib::Propagation::Proceed
    });
    entry.add_controller(escape);

    let ok_button = gtk4::Button::with_label("OK");
    let cancel_button = gtk4::Button::with_label("Cancel");
    cancel_button.set_margin_end(12);

    let button_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    button_box.set_halign(gtk4::Align::End);
    button_box.set_margin_start(12);
    button_box.set_margin_end(12);
    button_box.set_margin_bottom(12);
    button_box.append(&cancel_button);
    button_box.append(&ok_button);

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    vbox.append(&entry);
    vbox.append(&button_box);
    dialog.set_child(Some(&vbox));

    let result_ok = result.clone();
    let done_ok = done.clone();
    ok_button.connect_clicked({
        let dialog = dialog.clone();
        let entry = entry.clone();
        move |_| {
            *result_ok.borrow_mut() = Some(entry.text().to_string());
            *done_ok.borrow_mut() = true;
            dialog.close();
        }
    });

    let done_cancel = done.clone();
    cancel_button.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            *done_cancel.borrow_mut() = true;
            dialog.close();
        }
    });

    let done_close = done.clone();
    dialog.connect_close_request(move |window| {
        *done_close.borrow_mut() = true;
        window.set_visible(false);
        gtk4::glib::Propagation::Proceed
    });

    dialog.set_default_widget(Some(&ok_button));
    dialog.present();

    while !*done.borrow() {
        gtk4::glib::MainContext::default().iteration(true);
    }

    result.take()
}
