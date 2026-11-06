//! The options every VTE terminal shares (doc 02 §View options): installed
//! once, they reach open terminals and each new one.

use std::cell::RefCell;

use gtk4::{gdk, glib};
use vte4::prelude::*;
use vte4::{CursorBlinkMode, CursorShape, Terminal as VteTerminal};

use crate::terminal::options::{SelectionColors, VteBlink, VteCursor, VteOptions};

#[derive(Default)]
struct Installed {
    options: VteOptions,
    terminals: Vec<glib::WeakRef<VteTerminal>>,
}

thread_local! {
    static INSTALLED: RefCell<Installed> = RefCell::default();
}

/// Make `options` every terminal's. Main thread, as terminals are.
pub fn install_options(options: &VteOptions) {
    let terminals: Vec<VteTerminal> = INSTALLED.with(|installed| {
        let mut installed = installed.borrow_mut();
        installed.options = options.clone();
        let terminals = installed.terminals.iter();
        terminals.filter_map(glib::WeakRef::upgrade).collect()
    });
    for terminal in &terminals {
        apply(terminal, options);
    }
}

/// Take the installed options, now and whenever they change.
pub(super) fn follow_options(terminal: &VteTerminal) {
    let options = INSTALLED.with(|installed| {
        let mut installed = installed.borrow_mut();
        installed.terminals.retain(|weak| weak.upgrade().is_some());
        installed.terminals.push(terminal.downgrade());
        installed.options.clone()
    });
    apply(terminal, &options);
}

/// Put the installed selection colors back: setting a palette clears them.
pub(super) fn restore_selection(terminal: &VteTerminal) {
    let selection = INSTALLED.with(|installed| installed.borrow().options.selection);
    apply_selection(terminal, &selection);
}

fn apply(terminal: &VteTerminal, options: &VteOptions) {
    terminal.set_cursor_shape(match options.cursor_shape {
        VteCursor::Block => CursorShape::Block,
        VteCursor::IBeam => CursorShape::Ibeam,
        VteCursor::Underline => CursorShape::Underline,
    });
    terminal.set_cursor_blink_mode(match options.cursor_blink {
        VteBlink::System => CursorBlinkMode::System,
        VteBlink::On => CursorBlinkMode::On,
        VteBlink::Off => CursorBlinkMode::Off,
    });
    terminal.set_audible_bell(options.audible_bell);
    terminal.set_cell_height_scale(options.line_height);
    terminal.set_bold_is_bright(options.bold_is_bright);
    terminal.set_mouse_autohide(options.hide_mouse);
    apply_selection(terminal, &options.selection);
}

fn apply_selection(terminal: &VteTerminal, selection: &SelectionColors) {
    let rgba = |[r, g, b]: [u8; 3]| {
        let channel = |c: u8| f32::from(c) / 255.0;
        gdk::RGBA::new(channel(r), channel(g), channel(b), 1.0)
    };
    terminal.set_color_highlight(selection.bg.map(rgba).as_ref());
    terminal.set_color_highlight_foreground(selection.fg.map(rgba).as_ref());
}
