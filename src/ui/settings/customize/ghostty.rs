//! The Customize page's Ghostty section (doc 13 §Customize).

use std::rc::Rc;

use super::{rows, scheme_row, terminal_defaults};
use crate::store::SessionStore;
use crate::terminal::options::{
    GhosttyOptions, BLINK_PERIOD, GHOSTTY_CURSOR_BLINKS_SETTING, GHOSTTY_CURSOR_BLINK_SETTING,
    GHOSTTY_CURSOR_STYLE_SETTING, GHOSTTY_IMAGE_LIMIT_SETTING, GHOSTTY_SEARCH_BG_SETTING,
    GHOSTTY_SEARCH_CURRENT_BG_SETTING, GHOSTTY_SEARCH_FG_SETTING, GHOSTTY_SELECTION_BG_SETTING,
    GHOSTTY_SELECTION_FG_SETTING, GHOSTTY_TEXT_BLINK_SETTING, GHOSTTY_WHEEL_ROWS_SETTING,
    IMAGE_LIMIT, WHEEL_ROWS,
};
use crate::terminal::GHOSTTY_COLOR_SCHEME_SETTING;

pub(super) fn section(store: &Rc<SessionStore>) -> gtk4::ListBox {
    let current = terminal_defaults(store).ghostty;
    let seed = GhosttyOptions::default();
    let period = (
        *BLINK_PERIOD.start() as f64,
        *BLINK_PERIOD.end() as f64,
        50.0,
    );
    let list = crate::ui::settings::boxed_list();
    list.append(&scheme_row(store, GHOSTTY_COLOR_SCHEME_SETTING));
    list.append(&rows::enum_row(
        store,
        ("Cursor style", "Until a program sets its own."),
        current.cursor_style,
        GHOSTTY_CURSOR_STYLE_SETTING,
    ));
    list.append(&rows::switch_row(
        store,
        ("Blinking cursor", "Until a program sets its own."),
        current.cursor_blinks,
        GHOSTTY_CURSOR_BLINKS_SETTING,
    ));
    list.append(&rows::whole_row(
        store,
        ("Cursor blink period", "Milliseconds on, and off."),
        period,
        current.cursor_blink.as_millis() as f64,
        GHOSTTY_CURSOR_BLINK_SETTING,
    ));
    list.append(&rows::whole_row(
        store,
        ("Text blink period", "Milliseconds on, and off."),
        period,
        current.text_blink.as_millis() as f64,
        GHOSTTY_TEXT_BLINK_SETTING,
    ));
    list.append(&rows::decimal_row(
        store,
        ("Mouse wheel speed", "Rows per notch."),
        (*WHEEL_ROWS.start(), *WHEEL_ROWS.end(), 0.5),
        current.wheel_rows,
        GHOSTTY_WHEEL_ROWS_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Selection color", "Reset follows the color scheme."),
        current.selection.bg,
        None,
        GHOSTTY_SELECTION_BG_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Selection text color", "Reset follows the color scheme."),
        current.selection.fg,
        None,
        GHOSTTY_SELECTION_FG_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Search match color", "Behind every match."),
        Some(current.search.bg),
        Some(seed.search.bg),
        GHOSTTY_SEARCH_BG_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Current match color", "Behind the match search stopped on."),
        Some(current.search.current_bg),
        Some(seed.search.current_bg),
        GHOSTTY_SEARCH_CURRENT_BG_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Search text color", "The text of every match."),
        Some(current.search.fg),
        Some(seed.search.fg),
        GHOSTTY_SEARCH_FG_SETTING,
    ));
    list.append(&rows::whole_row(
        store,
        (
            "Kitty image storage",
            "Megabytes of images each screen holds; 0 turns Kitty images off.",
        ),
        (
            f64::from(*IMAGE_LIMIT.start()),
            f64::from(*IMAGE_LIMIT.end()),
            10.0,
        ),
        f64::from(current.image_limit_mb),
        GHOSTTY_IMAGE_LIMIT_SETTING,
    ));
    list
}
