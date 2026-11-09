//! Raw bindings to `shim.c`, the narrow C layer over libghostty-vt; use
//! [`Engine`](super::engine::Engine) instead of calling these directly.
//! The structs mirror `WtCell`/`WtFrame`/`WtSpan`/`WtPlacement`/`WtPixels`
//! there; the unit tests check the sizes, and where the cell's grapheme,
//! link and underline fields, and the fields after any padding, sit, against
//! the C compiler's.

use std::ffi::{c_char, c_int, c_void};

/// Opaque `WtGhostty`: a terminal plus its render state, its key and
/// mouse encoders, and its search.
#[repr(C)]
pub struct WtGhostty {
    _private: [u8; 0],
}

pub const SUCCESS: c_int = 0;
pub const OUT_OF_SPACE: c_int = -3;
pub const NO_VALUE: c_int = -4;

pub const EFFECT_PTY: c_int = 1;
pub const EFFECT_BELL: c_int = 2;
pub const EFFECT_PWD: c_int = 4;
pub const EFFECT_CLIPBOARD_SYSTEM: c_int = 5;
pub const EFFECT_CLIPBOARD_PRIMARY: c_int = 6;

pub const CELL_BOLD: u16 = 1 << 0;
pub const CELL_ITALIC: u16 = 1 << 1;
pub const CELL_UNDERLINE: u16 = 1 << 2;
pub const CELL_STRIKE: u16 = 1 << 3;
pub const CELL_INVERSE: u16 = 1 << 4;
pub const CELL_SELECTED: u16 = 1 << 5;
pub const CELL_INVISIBLE: u16 = 1 << 6;
pub const CELL_FAINT: u16 = 1 << 7;
/// SGR 5 or 6: libghostty-vt keeps one flag for both.
pub const CELL_BLINK: u16 = 1 << 8;

/// `GhosttyRenderStateCursorVisualStyle`, as `WtFrame` carries it.
pub const CURSOR_BAR: u8 = 0;
pub const CURSOR_BLOCK: u8 = 1;
pub const CURSOR_UNDERLINE: u8 = 2;
pub const CURSOR_BLOCK_HOLLOW: u8 = 3;

/// `WtCell::underline`: libghostty-vt's `GHOSTTY_SGR_UNDERLINE_*` values.
pub const UNDERLINE_NONE: u8 = 0;
pub const UNDERLINE_SINGLE: u8 = 1;
pub const UNDERLINE_DOUBLE: u8 = 2;
pub const UNDERLINE_CURLY: u8 = 3;
pub const UNDERLINE_DOTTED: u8 = 4;
pub const UNDERLINE_DASHED: u8 = 5;

pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 1;
pub const MOD_ALT: u32 = 1 << 2;
pub const MOD_SUPER: u32 = 1 << 3;

pub const MOUSE_PRESS: u32 = 0;
pub const MOUSE_RELEASE: u32 = 1;
pub const MOUSE_MOTION: u32 = 2;

pub const BUTTON_NONE: u32 = 0;
pub const BUTTON_LEFT: u32 = 1;
pub const BUTTON_RIGHT: u32 = 2;
pub const BUTTON_MIDDLE: u32 = 3;
pub const BUTTON_WHEEL_UP: u32 = 4;
pub const BUTTON_WHEEL_DOWN: u32 = 5;
pub const BUTTON_WHEEL_LEFT: u32 = 6;
pub const BUTTON_WHEEL_RIGHT: u32 = 7;

pub const LAYER_BELOW_BG: u8 = 0;
pub const LAYER_BELOW_TEXT: u8 = 1;
pub const LAYER_ABOVE_TEXT: u8 = 2;

/// Bytes of images one screen holds; also bounds a PNG's decoded size.
pub const KITTY_STORAGE_LIMIT: u64 = 320_000_000;

pub const FORMAT_RGB: u8 = 0;
pub const FORMAT_RGBA: u8 = 1;
pub const FORMAT_GRAY_ALPHA: u8 = 3;
pub const FORMAT_GRAY: u8 = 4;

/// One grid cell, colors resolved to RGB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtCell {
    /// The grapheme's base codepoint; 0 = empty.
    pub codepoint: u32,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub underline_rgb: [u8; 3],
    pub flags: u16,
    /// 0 = the default background.
    pub has_bg: u8,
    /// 0 = the line takes the text color.
    pub underline_has_rgb: u8,
    /// `UNDERLINE_*`.
    pub underline: u8,
    /// Codepoints in the cluster, base included: 0 = empty, 1 = just
    /// `codepoint`.
    pub grapheme_len: u16,
    /// 0 = no OSC 8 hyperlink; else its entry, from 1, in the snapshot's
    /// links ([`wt_ghostty_links`]).
    pub link: u16,
    /// Non-zero on a row's last cell when the row soft-wraps into the next.
    pub wraps: u8,
    /// With `grapheme_len` > 1: where the whole cluster starts in the
    /// snapshot's grapheme buffer.
    pub grapheme: u32,
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
    /// `CURSOR_*`.
    pub cursor_style: u8,
    /// Non-zero when the terminal asks for a blinking cursor (mode 12 or
    /// a DECSCUSR blink variant).
    pub cursor_blinking: u8,
}

/// One viewport row's run of search highlight: cells `start..end`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtSpan {
    pub row: u16,
    pub start: u16,
    pub end: u16,
    /// Non-zero for the current match.
    pub current: u8,
}

/// One image placement to draw.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtPlacement {
    /// Of the image's pixels: a new one means new pixels.
    pub generation: u64,
    pub image_id: u32,
    /// The order within a layer, lowest first.
    pub z: i32,
    /// The top-left cell, from the viewport's; negative when the placement
    /// starts off screen.
    pub col: i32,
    pub row: i32,
    /// Pixels into that cell.
    pub offset_x: u32,
    pub offset_y: u32,
    /// The size to draw at, in pixels.
    pub width: u32,
    pub height: u32,
    /// The part of the image to draw, in image pixels.
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    /// `LAYER_*`.
    pub layer: u8,
}

/// A stored image's pixels, borrowed until the terminal next changes.
#[repr(C)]
pub struct WtPixels {
    pub data: *const u8,
    pub len: usize,
    pub width: u32,
    pub height: u32,
    /// `FORMAT_*`.
    pub format: u8,
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
pub type NotificationFn = unsafe extern "C" fn(*mut c_void, *const u8, usize, *const u8, usize);

/// Decodes a PNG into the buffer `wt_ghostty_png_pixels` gives for `sink`.
pub type PngFn = unsafe extern "C" fn(data: *const u8, len: usize, sink: *mut c_void) -> bool;

extern "C" {
    pub fn wt_ghostty_new(cols: u16, rows: u16) -> *mut WtGhostty;
    pub fn wt_ghostty_free(g: *mut WtGhostty);
    pub fn wt_ghostty_set_effect(
        g: *mut WtGhostty,
        effect: Option<EffectFn>,
        userdata: *mut c_void,
    );
    pub fn wt_ghostty_set_notification(g: *mut WtGhostty, notification: Option<NotificationFn>);
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
    /// `style` is a `CURSOR_*`.
    pub fn wt_ghostty_set_cursor_default(g: *mut WtGhostty, style: u8, blink: bool) -> c_int;
    pub fn wt_ghostty_set_image_limit(g: *mut WtGhostty, bytes: u64) -> c_int;
    /// `fg`, `bg` and `cursor` point at 3 bytes (RGB), `ansi` at 16 × 3.
    pub fn wt_ghostty_set_colors(
        g: *mut WtGhostty,
        fg: *const u8,
        bg: *const u8,
        cursor: *const u8,
        ansi: *const u8,
    ) -> c_int;
    pub fn wt_ghostty_scroll(g: *mut WtGhostty, place: c_int, delta: isize);
    pub fn wt_ghostty_free_bytes(ptr: *mut u8, len: usize);
    pub fn wt_ghostty_screen_text(g: *mut WtGhostty, out: *mut *mut u8, len: *mut usize) -> c_int;
    pub fn wt_ghostty_selection_text(
        g: *mut WtGhostty,
        out: *mut *mut u8,
        len: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_select_all(g: *mut WtGhostty) -> c_int;
    pub fn wt_ghostty_select_output(g: *mut WtGhostty) -> c_int;
    pub fn wt_ghostty_clear_selection(g: *mut WtGhostty);
    pub fn wt_ghostty_selection_press(
        g: *mut WtGhostty,
        col: u16,
        row: u16,
        x: f64,
        y: f64,
        time_ns: u64,
        repeat_ns: u64,
        repeat_px: f64,
    ) -> c_int;
    pub fn wt_ghostty_selection_drag(
        g: *mut WtGhostty,
        col: u16,
        row: u16,
        x: f64,
        y: f64,
        cell_w: u32,
        height: u32,
    ) -> c_int;
    pub fn wt_ghostty_selection_release(g: *mut WtGhostty, col: u16, row: u16);
    pub fn wt_ghostty_selection_cancel(g: *mut WtGhostty);
    pub fn wt_ghostty_search(
        g: *mut WtGhostty,
        needle: *const u8,
        len: usize,
        backward: bool,
    ) -> c_int;
    pub fn wt_ghostty_search_clear(g: *mut WtGhostty);
    pub fn wt_ghostty_search_spans(
        g: *mut WtGhostty,
        out: *mut WtSpan,
        cap: usize,
        len: *mut usize,
    ) -> c_int;
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
        graphemes: *mut u32,
        graphemes_cap: usize,
        graphemes_len: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_set_png_decoder(decoder: Option<PngFn>);
    pub fn wt_ghostty_png_pixels(sink: *mut c_void, width: u32, height: u32) -> *mut u8;
    pub fn wt_ghostty_animation_tick(g: *mut WtGhostty, now_ms: u64, next_ms: *mut u64) -> c_int;
    pub fn wt_ghostty_placements(
        g: *mut WtGhostty,
        out: *mut WtPlacement,
        cap: usize,
        len: *mut usize,
    ) -> c_int;
    pub fn wt_ghostty_image_pixels(g: *mut WtGhostty, image_id: u32, out: *mut WtPixels) -> c_int;
    /// The last snapshot's links, valid until the next one: `count`
    /// entries, entry n, from 1, being `uris` up to `ends[n - 1]`, starting
    /// where the entry before it ends. Null while there are none.
    pub fn wt_ghostty_links(
        g: *mut WtGhostty,
        uris: *mut *const u8,
        ends: *mut *const usize,
        count: *mut usize,
    );
}

// ABI checks for the mirrors above (engine tests).
#[cfg(test)]
extern "C" {
    pub fn wt_ghostty_cell_size() -> usize;
    pub fn wt_ghostty_frame_size() -> usize;
    pub fn wt_ghostty_span_size() -> usize;
    pub fn wt_ghostty_placement_size() -> usize;
    pub fn wt_ghostty_pixels_size() -> usize;
    pub fn wt_ghostty_cell_grapheme_offset() -> usize;
    pub fn wt_ghostty_cell_grapheme_len_offset() -> usize;
    pub fn wt_ghostty_placement_layer_offset() -> usize;
    pub fn wt_ghostty_pixels_format_offset() -> usize;
    pub fn wt_ghostty_storage_limit(g: *mut WtGhostty) -> u64;
    pub fn wt_ghostty_cell_link_offset() -> usize;
    pub fn wt_ghostty_cell_wraps_offset() -> usize;
    pub fn wt_ghostty_cell_underline_offset() -> usize;
    pub fn wt_ghostty_cell_underline_rgb_offset() -> usize;
    pub fn wt_ghostty_cell_underline_has_rgb_offset() -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrap_flag_sits_where_c_puts_it() {
        // SAFETY: a plain getter.
        let wraps = unsafe { wt_ghostty_cell_wraps_offset() };
        assert_eq!(std::mem::offset_of!(WtCell, wraps), wraps);
    }
}
