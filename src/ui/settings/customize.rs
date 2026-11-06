//! The Customize settings page (doc 13 §Customize): how terminals look and
//! how much they keep, for sessions without their own.
//!
//! "Default color scheme" writes `default_color_scheme` (`System`, `Light`,
//! `Dark`) and re-colors open terminals at once; `System` follows
//! the desktop theme ([`appearance`](crate::ui::appearance)).
//!
//! Font, font size, opacity and scrollback write their rows and reach open
//! terminals the same way. [`rows`] builds the controls.

mod rows;

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{glib, pango};

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

/// Store the global scheme; [`apply`](super::general::apply) makes it take
/// effect.
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
    let selected = SchemeChoice::ALL.iter().position(|c| *c == current);
    rows::choice_row(
        store,
        (COLOR_SCHEME_TITLE, COLOR_SCHEME_SUBTITLE),
        &labels,
        selected.unwrap_or_default(),
        DEFAULT_COLOR_SCHEME_SETTING,
        |store, position| save_color_scheme(store, SchemeChoice::ALL[position]),
    )
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
        rows::save_and_apply(&store, DEFAULT_FONT_FAMILY_SETTING, |store| {
            save_font_family(store, &family)
        });
    });
    super::row(&[text.upcast_ref(), button.upcast_ref()])
}

/// title over subtitle | points.
fn font_size_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let spin = gtk4::SpinButton::with_range(1.0, 512.0, 1.0);
    spin.set_digits(1);
    spin.set_value(terminal_defaults(store).font_size);
    rows::settled_row(
        store,
        (FONT_SIZE_TITLE, FONT_SIZE_SUBTITLE),
        &spin,
        &spin.adjustment(),
        DEFAULT_FONT_SIZE_SETTING,
        save_font_size,
    )
}

/// title over subtitle | 0–100% slider.
fn opacity_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let scale = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 0.0, 100.0, 1.0);
    scale.set_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk4::PositionType::Right);
    scale.set_format_value_func(|_, percent| format!("{percent:.0}%"));
    scale.set_width_request(220);
    scale.set_value((terminal_defaults(store).opacity * 100.0).round());
    rows::settled_row(
        store,
        (OPACITY_TITLE, OPACITY_SUBTITLE),
        &scale,
        &scale.adjustment(),
        DEFAULT_OPACITY_SETTING,
        save_opacity_percent,
    )
}

/// title over subtitle | lines.
fn scrollback_row(store: &Rc<SessionStore>) -> gtk4::ListBoxRow {
    let spin = gtk4::SpinButton::with_range(0.0, f64::from(u32::MAX), 1000.0);
    spin.set_value(f64::from(terminal_defaults(store).scrollback_lines));
    rows::settled_row(
        store,
        (SCROLLBACK_TITLE, SCROLLBACK_SUBTITLE),
        &spin,
        &spin.adjustment(),
        SCROLLBACK_LIMIT_SETTING,
        // `value_as_int` is an i32, and the row goes up to u32::MAX.
        |store, lines| save_scrollback(store, lines as u32),
    )
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
