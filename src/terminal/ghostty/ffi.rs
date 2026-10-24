//! Raw bindings to `shim.c`, the narrow C layer over libghostty-vt; use
//! [`Engine`](super::engine::Engine) instead of calling these directly.
//! The structs mirror `WtCell`/`WtFrame` there; the unit tests check the
//! sizes against the C compiler's.

use std::ffi::{c_char, c_int, c_void};

/// Opaque `WtGhostty`: a terminal plus its render state and its key and
/// mouse encoders.
#[repr(C)]
pub struct WtGhostty {
    _private: [u8; 0],
}

pub const SUCCESS: c_int = 0;
pub const OUT_OF_SPACE: c_int = -3;

pub const EFFECT_PTY: c_int = 1;
pub const EFFECT_BELL: c_int = 2;
pub const EFFECT_TITLE: c_int = 3;

pub const CELL_BOLD: u8 = 1 << 0;
pub const CELL_ITALIC: u8 = 1 << 1;
pub const CELL_UNDERLINE: u8 = 1 << 2;
pub const CELL_STRIKE: u8 = 1 << 3;
pub const CELL_INVERSE: u8 = 1 << 4;
pub const CELL_SELECTED: u8 = 1 << 5;
pub const CELL_INVISIBLE: u8 = 1 << 6;
pub const CELL_FAINT: u8 = 1 << 7;

pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 1;
pub const MOD_ALT: u32 = 1 << 2;
pub const MOD_SUPER: u32 = 1 << 3;

pub const MOUSE_PRESS: u32 = 0;
pub const MOUSE_RELEASE: u32 = 1;
pub const MOUSE_MOTION: u32 = 2;

pub const BUTTON_NONE: u32 = 0;
pub const BUTTON_LEFT: u32 = 1;
pub const BUTTON_WHEEL_UP: u32 = 4;
pub const BUTTON_WHEEL_DOWN: u32 = 5;

/// One grid cell, colors resolved to RGB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtCell {
    /// 0 = empty.
    pub codepoint: u32,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub flags: u8,
    /// 0 = the default background.
    pub has_bg: u8,
}

/// Frame-wide render state.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtFrame {
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: u8,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor_rgb: [u8; 3],
    pub cursor_has_rgb: u8,
}

/// One key press for the encoder.
#[repr(C)]
pub struct WtKey {
    /// The GDK keyval (an X11 keysym).
    pub keysym: u32,
    /// The keysym's codepoint without Shift; 0 if none.
    pub unshifted: u32,
    /// `MOD_*` bits.
    pub mods: u32,
    /// The text the key produces, UTF-8, not NUL-terminated.
    pub utf8: *const c_char,
    pub utf8_len: usize,
}

/// One mouse event for the encoder.
#[repr(C)]
pub struct WtMouse {
    /// `MOUSE_*`.
    pub action: u32,
    /// `BUTTON_*`.
    pub button: u32,
    /// `MOD_*` bits.
    pub mods: u32,
    /// Position in widget pixels.
    pub x: f32,
    pub y: f32,
    /// The widget's size in pixels.
    pub width: u32,
    pub height: u32,
    /// One cell in pixels.
    pub cell_w: u32,
    pub cell_h: u32,
    /// Non-zero while a button is held, this event's included.
    pub any_pressed: u32,
}

pub type EffectFn =
    unsafe extern "C" fn(userdata: *mut c_void, kind: c_int, data: *const u8, len: usize);

extern "C" {
    pub fn wt_ghostty_new(cols: u16, rows: u16) -> *mut WtGhostty;
    pub fn wt_ghostty_free(g: *mut WtGhostty);
    pub fn wt_ghostty_set_effect(
        g: *mut WtGhostty,
        effect: Option<EffectFn>,
        userdata: *mut c_void,
    );
    pub fn wt_ghostty_write(g: *mut WtGhostty, data: *const u8, len: usize);
    pub fn wt_ghostty_resize(
        g: *mut WtGhostty,
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    ) -> c_int;
    pub fn wt_ghostty_size(g: *mut WtGhostty, cols: *mut u16, rows: *mut u16);
    pub fn wt_ghostty_reset(g: *mut WtGhostty);
    pub fn wt_ghostty_set_scrollback_lines(g: *mut WtGhostty, lines: usize) -> c_int;
    pub fn wt_ghostty_scroll(g: *mut WtGhostty, place: c_int, delta: isize);
    pub fn wt_ghostty_free_bytes(ptr: *mut u8, len: usize);
    pub fn wt_ghostty_screen_text(g: *mut WtGhostty, out: *mut *mut u8, len: *mut usize) -> c_int;
    pub fn wt_ghostty_selection_text(
        g: *mut WtGhostty,
        out: *mut *mut u8,
        len: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_select_all(g: *mut WtGhostty) -> c_int;
    pub fn wt_ghostty_select_viewport(
        g: *mut WtGhostty,
        x0: u16,
        y0: u16,
        x1: u16,
        y1: u16,
    ) -> c_int;
    pub fn wt_ghostty_clear_selection(g: *mut WtGhostty);
    pub fn wt_ghostty_encode_paste(
        g: *mut WtGhostty,
        text: *mut c_char,
        len: usize,
        out: *mut c_char,
        cap: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_encode_key(
        g: *mut WtGhostty,
        key: *const WtKey,
        out: *mut c_char,
        cap: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_dec_mode(g: *mut WtGhostty, value: u16) -> bool;
    pub fn wt_ghostty_encode_focus(
        g: *mut WtGhostty,
        gained: bool,
        out: *mut c_char,
        cap: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_encode_mouse(
        g: *mut WtGhostty,
        mouse: *const WtMouse,
        out: *mut c_char,
        cap: usize,
        written: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_snapshot(
        g: *mut WtGhostty,
        frame: *mut WtFrame,
        cells: *mut WtCell,
        cap: usize,
    ) -> c_int;
}

// ABI checks for the mirrors above (engine tests).
#[cfg(test)]
extern "C" {
    pub fn wt_ghostty_cell_size() -> usize;
    pub fn wt_ghostty_frame_size() -> usize;
}
