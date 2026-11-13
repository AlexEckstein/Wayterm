//! The Customize page's rows: each shows a setting, saves it and applies
//! it. Numeric controls save once they settle ([`SETTLE`]): each save
//! re-applies to every tab, and a font size change resizes every remote's
//! grid.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::store::SessionStore;
use crate::terminal::options::{color_row as stored_color, RowChoice};
use crate::ui::settings::general::apply;
use crate::ui::settings::{described, row};
use crate::ui::widgets::release_wheel;

/// A row's title and subtitle.
pub(super) type Text<'a> = (&'a str, &'a str);

/// A spin button's `(lowest, highest, step)`.
pub(super) type Span = (f64, f64, f64);

/// What a color button shows for a row that is not set, where the setting
/// has no color of its own.
const UNSET_COLOR: [u8; 3] = [0x80, 0x80, 0x80];

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
        tracing::warn!(key = %key, error = %err, "failed to save setting");
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
    // A lowered scrollback scrolled into by accident drops lines from open
    // tabs for good.
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

/// title over subtitle | a drop-down of `T`'s names.
pub(super) fn enum_row<T: RowChoice>(
    store: &Rc<SessionStore>,
    text: Text,
    current: T,
    key: &'static str,
) -> gtk4::ListBoxRow {
    let labels: Vec<&str> = T::ALL.iter().map(|(_, name)| *name).collect();
    choice_row(
        store,
        text,
        &labels,
        current.position(),
        key,
        move |store, position| store.set_setting(key, T::ALL[position].1),
    )
}

/// title over subtitle | switch, stored as `1` or `0`.
pub(super) fn switch_row(
    store: &Rc<SessionStore>,
    (title, subtitle): Text,
    on: bool,
    key: &'static str,
) -> gtk4::ListBoxRow {
    let switch = gtk4::Switch::builder()
        .active(on)
        .valign(gtk4::Align::Center)
        .build();
    let text = described(title, Some(subtitle), &switch);
    let store = Rc::clone(store);
    switch.connect_active_notify(move |switch| {
        let value = if switch.is_active() { "1" } else { "0" };
        save_and_apply(&store, key, |store| store.set_setting(key, value));
    });
    row(&[text.upcast_ref(), switch.upcast_ref()])
}

fn spin(store_value: f64, (lowest, highest, step): Span, digits: u32) -> gtk4::SpinButton {
    let spin = gtk4::SpinButton::with_range(lowest, highest, step);
    spin.set_digits(digits);
    spin.set_value(store_value);
    spin
}

/// title over subtitle | a number to one decimal place.
pub(super) fn decimal_row(
    store: &Rc<SessionStore>,
    text: Text,
    span: Span,
    current: f64,
    key: &'static str,
) -> gtk4::ListBoxRow {
    let spin = spin(current, span, 1);
    settled_row(
        store,
        text,
        &spin,
        &spin.adjustment(),
        key,
        move |store, value| store.set_setting(key, &format!("{value:.1}")),
    )
}

/// title over subtitle | a whole number.
pub(super) fn whole_row(
    store: &Rc<SessionStore>,
    text: Text,
    span: Span,
    current: f64,
    key: &'static str,
) -> gtk4::ListBoxRow {
    let spin = spin(current, span, 0);
    settled_row(
        store,
        text,
        &spin,
        &spin.adjustment(),
        key,
        move |store, value| store.set_setting(key, &format!("{value:.0}")),
    )
}

fn rgba([r, g, b]: [u8; 3]) -> gdk::RGBA {
    let channel = |c: u8| f32::from(c) / 255.0;
    gdk::RGBA::new(channel(r), channel(g), channel(b), 1.0)
}

fn bytes(color: &gdk::RGBA) -> [u8; 3] {
    let channel = |c: f32| (c * 255.0).round() as u8;
    [
        channel(color.red()),
        channel(color.green()),
        channel(color.blue()),
    ]
}

/// title over subtitle | color button, Reset. Reset blanks the row, which
/// reads as not set, and is off while it is. `default` is the color that
/// brings back, for a setting with one of its own.
pub(super) fn color_row(
    store: &Rc<SessionStore>,
    (title, subtitle): Text,
    current: Option<[u8; 3]>,
    default: Option<[u8; 3]>,
    key: &'static str,
) -> gtk4::ListBoxRow {
    let fallback = default.unwrap_or(UNSET_COLOR);
    let dialog = gtk4::ColorDialog::builder().with_alpha(false).build();
    let button = gtk4::ColorDialogButton::new(Some(dialog));
    button.set_valign(gtk4::Align::Center);
    button.set_rgba(&rgba(current.unwrap_or(fallback)));
    let stored = store.setting(key);
    let reset = gtk4::Button::builder()
        .label("Reset")
        .valign(gtk4::Align::Center)
        .sensitive(stored.is_some_and(|row| !row.trim().is_empty()))
        .build();
    let text = described(title, Some(subtitle), &button);

    let resetting = Rc::new(Cell::new(false));
    let (saving, weak_reset) = (Rc::clone(store), reset.downgrade());
    let quiet = Rc::clone(&resetting);
    button.connect_rgba_notify(move |button| {
        if quiet.get() {
            return;
        }
        let color = stored_color(bytes(&button.rgba()));
        save_and_apply(&saving, key, |store| store.set_setting(key, &color));
        if let Some(reset) = weak_reset.upgrade() {
            reset.set_sensitive(true);
        }
    });
    let (saving, weak_button) = (Rc::clone(store), button.downgrade());
    reset.connect_clicked(move |reset| {
        if let Some(button) = weak_button.upgrade() {
            resetting.set(true);
            button.set_rgba(&rgba(fallback));
            resetting.set(false);
        }
        save_and_apply(&saving, key, |store| store.set_setting(key, ""));
        reset.set_sensitive(false);
    });
    row(&[text.upcast_ref(), button.upcast_ref(), reset.upcast_ref()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A save that fails is logged with its key, and nothing is applied:
    /// the function returns before it would need a display.
    #[test]
    fn a_failed_save_is_logged_with_its_key() {
        let store = SessionStore::in_memory().unwrap();

        let (buffer, guard) = crate::logging::tests::capture(None);
        save_and_apply(&store, "default_font_size", |_| {
            Err(rusqlite::Error::InvalidQuery)
        });
        drop(guard);
        let output = buffer.contents();
        let failed = output
            .lines()
            .find(|line| line.contains("failed to save setting"))
            .unwrap_or_else(|| panic!("no save failure logged: {output}"));
        assert!(failed.contains("WARN"), "{failed}");
        assert!(failed.contains("key=default_font_size"), "{failed}");
    }
}
