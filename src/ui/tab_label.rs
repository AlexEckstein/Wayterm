//! The tab label: session name plus close button; double-click renames
//! (doc 03 §Tab labels).
//!
//! Each label carries a [`TabTitle`]: placeholder (the session name) →
//! set (a title from the terminal) → renamed (by the user). Once renamed,
//! terminal titles no longer change it.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use crate::ui::widgets::{data, prompt_text, set_data, window_of};

/// Label data key: the [`TabTitleLabel`].
const TAB_TITLE_KEY: &str = "wayterm-tab-title";

/// Where a tab's title came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabTitleState {
    /// The session name, until the terminal sets a title.
    Placeholder,
    /// The terminal's window title.
    Set,
    /// The user's rename; terminal titles no longer apply.
    Renamed,
}

/// The display-free title state machine.
#[derive(Debug)]
pub struct TabTitle {
    /// Where the title came from.
    state: TabTitleState,
    /// The text shown.
    value: String,
}

impl TabTitle {
    /// A title showing `placeholder`.
    pub fn new(placeholder: &str) -> Self {
        Self {
            state: TabTitleState::Placeholder,
            value: placeholder.to_string(),
        }
    }

    /// A title from the terminal; ignored once renamed. Returns whether it
    /// took effect.
    pub fn set(&mut self, title: &str) -> bool {
        if self.state == TabTitleState::Renamed || self.value == title {
            return false;
        }
        self.value = title.to_string();
        self.state = TabTitleState::Set;
        true
    }

    /// A user rename; empty names are rejected so a tab keeps a label.
    pub fn rename(&mut self, title: &str) -> bool {
        if title.is_empty() || self.value == title {
            return false;
        }
        self.value = title.to_string();
        self.state = TabTitleState::Renamed;
        true
    }

    /// Where the title came from.
    pub fn state(&self) -> &TabTitleState {
        &self.state
    }

    /// The text shown.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A [`TabTitle`] bound to the label that shows it.
pub struct TabLabelState {
    /// The title state machine.
    title: RefCell<TabTitle>,
    /// The label it drives.
    label: gtk4::Label,
}

impl TabLabelState {
    /// Show a terminal title, unless renamed; `true` if it changed.
    pub fn set(&self, title: &str) -> bool {
        self.update(|t| t.set(title))
    }

    /// Rename by the user; `true` if it changed.
    pub fn rename(&self, title: &str) -> bool {
        self.update(|t| t.rename(title))
    }

    /// Apply `change`; show the new title if it changed.
    fn update(&self, change: impl FnOnce(&mut TabTitle) -> bool) -> bool {
        let changed = change(&mut self.title.borrow_mut());
        if changed {
            self.label.set_text(self.title.borrow().value());
        }
        changed
    }

    /// The text shown.
    pub fn value(&self) -> String {
        self.title.borrow().value().to_string()
    }
}

/// The title state of a label built by [`build_tab_label`].
pub fn title_state_of(label_widget: &gtk4::Widget) -> Option<Rc<TabLabelState>> {
    data(label_widget, TAB_TITLE_KEY)
}

/// Feed a terminal title to a tab label; `false` if it has no title state
/// or was renamed by the user.
pub fn apply_terminal_title(label_widget: &gtk4::Widget, title: &str) -> bool {
    title_state_of(label_widget).is_some_and(|state| state.set(title))
}

/// Name (double-click to rename) plus close button; `on_close` runs on
/// the close button. Returns the label widget and its name label.
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

    let state = Rc::new(TabLabelState {
        title: RefCell::new(TabTitle::new(session_name)),
        label: name_label.clone(),
    });
    set_data(&box_, TAB_TITLE_KEY, state);

    let click = gtk4::GestureClick::new();
    click.set_button(gtk4::gdk::BUTTON_PRIMARY);
    let label_widget: gtk4::Widget = box_.upcast();
    // Weak: the gesture lives on a child of the label widget.
    let for_rename = label_widget.downgrade();
    click.connect_pressed(move |gesture, n_press, _x, _y| {
        if n_press != 2 {
            return;
        }
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        if let Some(label_widget) = for_rename.upgrade() {
            prompt_rename(&label_widget);
        }
    });
    name_label.add_controller(click);
    (label_widget, name_label)
}

/// Ask for a new name for the tab whose label is `label_widget` (one
/// built by [`build_tab_label`]): a double-click on the name, or the
/// `rename_tab` action. No-op for any other label.
pub fn prompt_rename(label_widget: &gtk4::Widget) {
    let Some(state) = title_state_of(label_widget) else {
        return;
    };
    let parent = window_of(label_widget);
    if let Some(new_name) = prompt_text(parent.as_ref(), "Rename Tab", &state.value()) {
        state.rename(&new_name);
    }
}
