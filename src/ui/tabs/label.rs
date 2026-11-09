//! The tab label: session name plus close button; double-click renames
//! (doc 03 §Tab labels).

use gtk4::prelude::*;

use crate::ui::widgets::{data, prompt_text, set_data, window_of};

const NAME_LABEL_KEY: &str = "wayterm-tab-name-label";

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

    set_data(&box_, NAME_LABEL_KEY, name_label.clone());

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
    let Some(name) = data::<gtk4::Label>(label_widget, NAME_LABEL_KEY) else {
        return;
    };
    let parent = window_of(label_widget);
    if let Some(new_name) = prompt_text(parent.as_ref(), "Rename Tab", &name.text()) {
        // An empty name is ignored so the tab keeps a label.
        if !new_name.is_empty() {
            name.set_text(&new_name);
        }
    }
}
