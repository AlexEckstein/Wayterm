//! The Customize page's rows: each shows a setting, saves it and applies
//! it. Numeric controls save once they settle ([`SETTLE`]): each save
//! re-applies to every tab, and a font size change resizes every remote's
//! grid.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::settings::general::apply;
use crate::ui::settings::{described, row};

/// A row's title and subtitle.
pub(super) type Text<'a> = (&'a str, &'a str);

/// How long a numeric control stays still before it saves.
const SETTLE: Duration = Duration::from_millis(150);

/// A numeric control's waiting save, re-armed by every change.
type PendingSave = Rc<RefCell<Option<glib::SourceId>>>;

pub(super) fn save_and_apply(
    store: &SessionStore,
    key: &str,
    save: impl FnOnce(&SessionStore) -> rusqlite::Result<()>,
) {
    if let Err(err) = save(store) {
        eprintln!("Wayterm: failed to save setting '{key}': {err}");
        return;
    }
    apply(store);
}

/// Replace `pending` with `save`, due after [`SETTLE`]. It holds the value,
/// not the control, so closing the window does not lose it.
fn save_when_settled(
    pending: &PendingSave,
    store: &Rc<SessionStore>,
    key: &'static str,
    save: impl FnOnce(&SessionStore) -> rusqlite::Result<()> + 'static,
) {
    if let Some(id) = pending.take() {
        id.remove();
    }
    let slot = Rc::clone(pending);
    let store = Rc::clone(store);
    let id = glib::timeout_add_local_once(SETTLE, move || {
        // A fired source is gone; removing it again would be a GLib error.
        slot.take();
        save_and_apply(&store, key, save);
    });
    pending.replace(Some(id));
}

/// Leave the wheel to the scrolling page. A page scrolled under the pointer
/// would otherwise change the value, and a lowered scrollback drops lines
/// from open tabs for good.
fn release_wheel(control: &impl IsA<gtk4::Widget>) {
    let wheel: Vec<gtk4::EventControllerScroll> = control
        .observe_controllers()
        .iter::<glib::Object>()
        .flatten()
        .filter_map(|controller| controller.downcast().ok())
        .collect();
    for controller in &wheel {
        control.remove_controller(controller);
    }
}

/// title over subtitle | `control`, a numeric control over `adjustment`.
pub(super) fn settled_row<S>(
    store: &Rc<SessionStore>,
    (title, subtitle): Text,
    control: &impl IsA<gtk4::Widget>,
    adjustment: &gtk4::Adjustment,
    key: &'static str,
    save: S,
) -> gtk4::ListBoxRow
where
    S: Fn(&SessionStore, f64) -> rusqlite::Result<()> + Clone + 'static,
{
    control.set_valign(gtk4::Align::Center);
    release_wheel(control);
    let text = described(title, Some(subtitle), control);
    let store = Rc::clone(store);
    let pending = PendingSave::default();
    adjustment.connect_value_changed(move |adjustment| {
        let (value, save) = (adjustment.value(), save.clone());
        save_when_settled(&pending, &store, key, move |store| save(store, value));
    });
    row(&[text.upcast_ref(), control.upcast_ref()])
}

/// title over subtitle | a drop-down of `labels`; `save` gets the position
/// picked.
pub(super) fn choice_row(
    store: &Rc<SessionStore>,
    (title, subtitle): Text,
    labels: &[&str],
    selected: usize,
    key: &'static str,
    save: impl Fn(&SessionStore, usize) -> rusqlite::Result<()> + 'static,
) -> gtk4::ListBoxRow {
    let dropdown = gtk4::DropDown::from_strings(labels);
    dropdown.set_valign(gtk4::Align::Center);
    dropdown.set_selected(selected as u32);
    let text = described(title, Some(subtitle), &dropdown);
    let store = Rc::clone(store);
    let count = labels.len();
    dropdown.connect_selected_notify(move |dropdown| {
        let position = dropdown.selected() as usize;
        if position < count {
            save_and_apply(&store, key, |store| save(store, position));
        }
    });
    row(&[text.upcast_ref(), dropdown.upcast_ref()])
}
