//! The Customize page's VTE section (doc 13 §Customize).

use std::rc::Rc;

use super::{rows, scheme_row, terminal_defaults};
use crate::store::SessionStore;
use crate::terminal::options::{
    VTE_AUDIBLE_BELL_SETTING, VTE_BOLD_IS_BRIGHT_SETTING, VTE_CURSOR_BLINK_SETTING,
    VTE_CURSOR_SHAPE_SETTING, VTE_HIDE_MOUSE_SETTING, VTE_LINE_HEIGHT_SETTING,
    VTE_SELECTION_BG_SETTING, VTE_SELECTION_FG_SETTING,
};
use crate::terminal::{options, VTE_COLOR_SCHEME_SETTING};

pub(super) fn section(store: &Rc<SessionStore>) -> gtk4::ListBox {
    let current = terminal_defaults(store).vte;
    let list = crate::ui::settings::boxed_list();
    list.append(&scheme_row(store, VTE_COLOR_SCHEME_SETTING));
    list.append(&rows::enum_row(
        store,
        ("Cursor shape", "Until a program sets its own."),
        current.cursor_shape,
        VTE_CURSOR_SHAPE_SETTING,
    ));
    list.append(&rows::enum_row(
        store,
        ("Cursor blink", "System follows the desktop's setting."),
        current.cursor_blink,
        VTE_CURSOR_BLINK_SETTING,
    ));
    list.append(&rows::switch_row(
        store,
        ("Audible bell", "Sound the bell when a program rings it."),
        current.audible_bell,
        VTE_AUDIBLE_BELL_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Selection color", "Reset follows the color scheme."),
        current.selection.bg,
        None,
        VTE_SELECTION_BG_SETTING,
    ));
    list.append(&rows::color_row(
        store,
        ("Selection text color", "Reset follows the color scheme."),
        current.selection.fg,
        None,
        VTE_SELECTION_FG_SETTING,
    ));
    list.append(&rows::decimal_row(
        store,
        ("Line height", "As a multiple of the font's height."),
        (
            *options::LINE_HEIGHT.start(),
            *options::LINE_HEIGHT.end(),
            0.1,
        ),
        current.line_height,
        VTE_LINE_HEIGHT_SETTING,
    ));
    list.append(&rows::switch_row(
        store,
        (
            "Bold text is bright",
            "Draw bold text in the palette's bright colors.",
        ),
        current.bold_is_bright,
        VTE_BOLD_IS_BRIGHT_SETTING,
    ));
    list.append(&rows::switch_row(
        store,
        (
            "Hide mouse while typing",
            "The pointer comes back when it moves.",
        ),
        current.hide_mouse,
        VTE_HIDE_MOUSE_SETTING,
    ));
    list
}
