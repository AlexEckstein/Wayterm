//! The Customize settings page (doc 13 §Customize): how terminals look and
//! how much they keep, for sessions without their own.
//!
//! "Default color scheme" writes `default_color_scheme` (`System`, `Light`,
//! `Dark`) and re-colors open terminals at once; `System` follows
//! the desktop theme ([`appearance`](crate::ui::appearance)).
//!
//! Font, font size, opacity and scrollback write their rows and reach open
//! terminals the same way. The numeric controls save once they settle
//! ([`SETTLE`]): each save re-applies to every tab, and a font size change
//! resizes every remote's grid.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{glib, pango};

use super::general::apply;
use crate::store::SessionStore;
use crate::terminal::defaults::{
    DEFAULT_FONT_FAMILY_SETTING, DEFAULT_FONT_SIZE_SETTING, DEFAULT_OPACITY_SETTING,
    SCROLLBACK_LIMIT_SETTING,
};
use crate::terminal::{SchemeChoice, TerminalDefaults, DEFAULT_COLOR_SCHEME_SETTING};

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// The row's text.
pub const COLOR_SCHEME_TITLE: &str = "Default color scheme";
pub const COLOR_SCHEME_SUBTITLE: &str = "Terminal colors for sessions without their own. \
                                         System follows the desktop's light or dark theme.";

/// The stored `default_color_scheme`; missing or unknown is `System`, as
/// tabs resolve it.
pub fn color_scheme(store: &SessionStore) -> SchemeChoice {
    store
        .setting(DEFAULT_COLOR_SCHEME_SETTING)
        .as_deref()
        .and_then(SchemeChoice::from_setting)
        .unwrap_or(SchemeChoice::System)
}

/// Store the global scheme; [`apply`] makes it take effect.
pub fn save_color_scheme(store: &SessionStore, choice: SchemeChoice) -> rusqlite::Result<()> {
    store.set_setting(DEFAULT_COLOR_SCHEME_SETTING, choice.as_setting())
}

pub const FONT_TITLE: &str = "Font";
pub const FONT_SUBTITLE: &str = "Terminal font for sessions without their own.";
pub const FONT_SIZE_TITLE: &str = "Font size";
pub const FONT_SIZE_SUBTITLE: &str = "In points, for sessions without their own.";
pub const OPACITY_TITLE: &str = "Background opacity";
pub const OPACITY_SUBTITLE: &str = "Below 100%, the desktop shows through the terminal's \
                                    default background. For sessions without their own.";
pub const SCROLLBACK_TITLE: &str = "Scrollback lines";
pub const SCROLLBACK_SUBTITLE: &str = "Lines each terminal keeps; 0 keeps none. \
                                       Lowering it drops older lines from open tabs.";

/// The stored font, opacity and scrollback rows, as tabs resolve them.
pub fn terminal_defaults(store: &SessionStore) -> TerminalDefaults {
    TerminalDefaults::from_rows(|key| store.setting(key))
}

pub fn save_font_family(store: &SessionStore, family: &str) -> rusqlite::Result<()> {
    store.set_setting(DEFAULT_FONT_FAMILY_SETTING, family)
}

pub fn save_font_size(store: &SessionStore, points: f64) -> rusqlite::Result<()> {
    store.set_setting(DEFAULT_FONT_SIZE_SETTING, &points.to_string())
}

/// The control counts percent; the row holds 0–1.
pub fn save_opacity_percent(store: &SessionStore, percent: f64) -> rusqlite::Result<()> {
    store.set_setting(DEFAULT_OPACITY_SETTING, &(percent / 100.0).to_string())
}

pub fn save_scrollback(store: &SessionStore, lines: u32) -> rusqlite::Result<()> {
    store.set_setting(SCROLLBACK_LIMIT_SETTING, &lines.to_string())
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// How long a numeric control stays still before it saves.
const SETTLE: Duration = Duration::from_millis(150);

/// A numeric control's waiting save, re-armed by every change.
type PendingSave = Rc<RefCell<Option<glib::SourceId>>>;

/// The page widget: one list of the terminal's rows.
pub fn page(store: Rc<SessionStore>) -> gtk4::Widget {
    let terminal = super::boxed_list();
    terminal.append(&color_scheme_row(&store));
    terminal.append(&font_row(&store));
    terminal.append(&font_size_row(&store));
    terminal.append(&opacity_row(&store));
    terminal.append(&scrollback_row(&store));
    super::scrolling_page(&[
        super::heading("Terminal").upcast_ref(),
        terminal.upcast_ref(),
    ])
}

/// title over subtitle | System / Light / Dark.
fn color_scheme_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let labels: Vec<&str> = SchemeChoice::ALL.iter().map(|c| c.as_setting()).collect();
    let current = color_scheme(store);
    let dropdown = gtk4::DropDown::from_strings(&labels);
    dropdown.set_valign(gtk4::Align::Center);
    let selected = SchemeChoice::ALL.iter().position(|c| *c == current);
    dropdown.set_selected(selected.unwrap_or_default() as u32);
    let text = super::described(COLOR_SCHEME_TITLE, Some(COLOR_SCHEME_SUBTITLE), &dropdown);
    let store = Rc::clone(store);
    dropdown.connect_selected_notify(move |dropdown| {
        let Some(choice) = SchemeChoice::ALL.get(dropdown.selected() as usize) else {
            return;
        };
        if let Err(err) = save_color_scheme(&store, *choice) {
            eprintln!("Wayterm: failed to save setting '{DEFAULT_COLOR_SCHEME_SETTING}': {err}");
            return;
        }
        apply(&store);
    });
    super::row(&[text.upcast_ref(), dropdown.upcast_ref()])
}

fn save_and_apply(
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

/// The dialog hands over families or faces, depending on its level.
fn is_monospace(item: &glib::Object) -> bool {
    if let Some(family) = item.downcast_ref::<pango::FontFamily>() {
        return family.is_monospace();
    }
    item.downcast_ref::<pango::FontFace>()
        .is_some_and(|face| face.family().is_monospace())
}

/// title over subtitle | font button. Family only, as the row stores: a
/// face picked here could not be kept.
fn font_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let dialog = gtk4::FontDialog::builder()
        .title("Terminal Font")
        .filter(&gtk4::CustomFilter::new(is_monospace))
        .build();
    let button = gtk4::FontDialogButton::builder()
        .dialog(&dialog)
        .level(gtk4::FontLevel::Family)
        .valign(gtk4::Align::Center)
        .build();
    button.set_font_desc(&terminal_defaults(store).font(None, None));
    let text = super::described(FONT_TITLE, Some(FONT_SUBTITLE), &button);
    let store = Rc::clone(store);
    button.connect_font_desc_notify(move |button| {
        let Some(family) = button.font_desc().and_then(|font| font.family()) else {
            return;
        };
        save_and_apply(&store, DEFAULT_FONT_FAMILY_SETTING, |store| {
            save_font_family(store, &family)
        });
    });
    super::row(&[text.upcast_ref(), button.upcast_ref()])
}

/// title over subtitle | points.
fn font_size_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let spin = gtk4::SpinButton::with_range(1.0, 512.0, 1.0);
    spin.set_digits(1);
    spin.set_valign(gtk4::Align::Center);
    spin.set_value(terminal_defaults(store).font_size);
    release_wheel(&spin);
    let text = super::described(FONT_SIZE_TITLE, Some(FONT_SIZE_SUBTITLE), &spin);
    let store = Rc::clone(store);
    let pending = PendingSave::default();
    spin.connect_value_changed(move |spin| {
        let points = spin.value();
        save_when_settled(&pending, &store, DEFAULT_FONT_SIZE_SETTING, move |store| {
            save_font_size(store, points)
        });
    });
    super::row(&[text.upcast_ref(), spin.upcast_ref()])
}

/// title over subtitle | 0–100% slider.
fn opacity_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let scale = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 0.0, 100.0, 1.0);
    scale.set_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk4::PositionType::Right);
    scale.set_format_value_func(|_, percent| format!("{percent:.0}%"));
    scale.set_width_request(220);
    scale.set_valign(gtk4::Align::Center);
    scale.set_value((terminal_defaults(store).opacity * 100.0).round());
    release_wheel(&scale);
    let text = super::described(OPACITY_TITLE, Some(OPACITY_SUBTITLE), &scale);
    let store = Rc::clone(store);
    let pending = PendingSave::default();
    scale.connect_value_changed(move |scale| {
        let percent = scale.value();
        save_when_settled(&pending, &store, DEFAULT_OPACITY_SETTING, move |store| {
            save_opacity_percent(store, percent)
        });
    });
    super::row(&[text.upcast_ref(), scale.upcast_ref()])
}

/// title over subtitle | lines.
fn scrollback_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let spin = gtk4::SpinButton::with_range(0.0, f64::from(u32::MAX), 1000.0);
    spin.set_valign(gtk4::Align::Center);
    spin.set_value(f64::from(terminal_defaults(store).scrollback_lines));
    release_wheel(&spin);
    let text = super::described(SCROLLBACK_TITLE, Some(SCROLLBACK_SUBTITLE), &spin);
    let store = Rc::clone(store);
    let pending = PendingSave::default();
    spin.connect_value_changed(move |spin| {
        // `value_as_int` is an i32, and the row goes up to u32::MAX.
        let lines = spin.value() as u32;
        save_when_settled(&pending, &store, SCROLLBACK_LIMIT_SETTING, move |store| {
            save_scrollback(store, lines)
        });
    });
    super::row(&[text.upcast_ref(), spin.upcast_ref()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new store follows the system; a saved choice round-trips, and an
    /// unknown row reads as System.
    #[test]
    fn color_scheme_choice_round_trips() {
        let store = SessionStore::in_memory().unwrap();
        assert_eq!(
            store
                .get_setting(DEFAULT_COLOR_SCHEME_SETTING)
                .unwrap()
                .as_deref(),
            Some("System"),
            "the seed"
        );
        assert_eq!(color_scheme(&store), SchemeChoice::System);
        for choice in [
            SchemeChoice::Light,
            SchemeChoice::Dark,
            SchemeChoice::System,
        ] {
            save_color_scheme(&store, choice).unwrap();
            assert_eq!(color_scheme(&store), choice);
        }
        store
            .set_setting(DEFAULT_COLOR_SCHEME_SETTING, "Solarized")
            .unwrap();
        assert_eq!(color_scheme(&store), SchemeChoice::System);
    }

    /// Each saved value reads back as tabs resolve it, the opacity from
    /// percent to the row's 0–1.
    #[test]
    fn terminal_rows_round_trip() {
        let store = SessionStore::in_memory().unwrap();
        assert_eq!(terminal_defaults(&store), TerminalDefaults::default());

        save_font_family(&store, "Fira Code").unwrap();
        save_font_size(&store, 14.5).unwrap();
        save_opacity_percent(&store, 57.0).unwrap();
        save_scrollback(&store, u32::MAX).unwrap();
        assert_eq!(
            terminal_defaults(&store),
            TerminalDefaults {
                font_family: "Fira Code".to_string(),
                font_size: 14.5,
                opacity: 0.57,
                scrollback_lines: u32::MAX,
                ..Default::default()
            }
        );

        save_opacity_percent(&store, 0.0).unwrap();
        save_scrollback(&store, 0).unwrap();
        let defaults = terminal_defaults(&store);
        assert_eq!((defaults.opacity, defaults.scrollback_lines), (0.0, 0));
    }
}
