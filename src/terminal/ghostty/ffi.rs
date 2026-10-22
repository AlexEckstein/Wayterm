//! Raw bindings to `shim.c`, the narrow C layer over libghostty-vt; use
//! [`Engine`](super::engine::Engine) instead of calling these directly.
//! The structs mirror `WtCell`/`WtFrame` there; the unit tests check the
//! sizes against the C compiler's.

use std::ffi::{c_char, c_int, c_void};

/// Opaque `WtGhostty`: a terminal plus its render state and key encoder.
#[repr(C)]
pub struct WtGhostty {
    /// Zero-sized: only ever behind a pointer.
    _private: [u8; 0],
}

/// Status: the call succeeded.
pub const SUCCESS: c_int = 0;
/// Status: the output buffer was too small; `written` holds the size needed.
pub const OUT_OF_SPACE: c_int = -3;

/// Effect kind: bytes to send to the remote side.
pub const EFFECT_PTY: c_int = 1;
/// Effect kind: BEL.
pub const EFFECT_BELL: c_int = 2;
/// Effect kind: a new window title (UTF-8).
pub const EFFECT_TITLE: c_int = 3;

/// Cell flag: bold.
pub const CELL_BOLD: u8 = 1 << 0;
/// Cell flag: italic.
pub const CELL_ITALIC: u8 = 1 << 1;
/// Cell flag: underline.
pub const CELL_UNDERLINE: u8 = 1 << 2;
/// Cell flag: strikethrough.
pub const CELL_STRIKE: u8 = 1 << 3;
/// Cell flag: inverse video.
pub const CELL_INVERSE: u8 = 1 << 4;
/// Cell flag: inside the selection.
pub const CELL_SELECTED: u8 = 1 << 5;
/// Cell flag: invisible (SGR 8).
pub const CELL_INVISIBLE: u8 = 1 << 6;
/// Cell flag: faint.
pub const CELL_FAINT: u8 = 1 << 7;

/// Key modifier: Shift.
pub const MOD_SHIFT: u32 = 1 << 0;
/// Key modifier: Ctrl.
pub const MOD_CTRL: u32 = 1 << 1;
/// Key modifier: Alt.
pub const MOD_ALT: u32 = 1 << 2;
/// Key modifier: Super.
pub const MOD_SUPER: u32 = 1 << 3;

/// One grid cell, colors resolved to RGB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtCell {
    /// 0 = empty.
    pub codepoint: u32,
    /// Resolved foreground RGB.
    pub fg: [u8; 3],
    /// Resolved background RGB, valid when `has_bg` is set.
    pub bg: [u8; 3],
    /// `CELL_*` bits.
    pub flags: u8,
    /// 0 = the default background.
    pub has_bg: u8,
}

/// Frame-wide render state.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WtFrame {
    /// Viewport width in cells.
    pub cols: u16,
    /// Viewport height in cells.
    pub rows: u16,
    /// Cursor column.
    pub cursor_x: u16,
    /// Cursor row.
    pub cursor_y: u16,
    /// Non-zero when the cursor shows.
    pub cursor_visible: u8,
    /// Default foreground RGB.
    pub fg: [u8; 3],
    /// Default background RGB.
    pub bg: [u8; 3],
    /// Cursor color, valid when `cursor_has_rgb` is set.
    pub cursor_rgb: [u8; 3],
    /// Non-zero when the program set a cursor color.
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
    /// Length of `utf8` in bytes.
    pub utf8_len: usize,
}

/// Called for every effect raised while parsing; `data` is borrowed.
pub type EffectFn =
    unsafe extern "C" fn(userdata: *mut c_void, kind: c_int, data: *const u8, len: usize);

extern "C" {
    /// A new terminal; null if allocation fails.
    pub fn wt_ghostty_new(cols: u16, rows: u16) -> *mut WtGhostty;
    /// Free a terminal from [`wt_ghostty_new`].
    pub fn wt_ghostty_free(g: *mut WtGhostty);
    /// Install the effect callback.
    pub fn wt_ghostty_set_effect(
        g: *mut WtGhostty,
        effect: Option<EffectFn>,
        userdata: *mut c_void,
    );
    /// Feed remote bytes to the parser.
    pub fn wt_ghostty_write(g: *mut WtGhostty, data: *const u8, len: usize);
    /// Resize the grid with reflow; cell pixels feed size reports.
    pub fn wt_ghostty_resize(
        g: *mut WtGhostty,
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    ) -> c_int;
    /// Current grid size.
    pub fn wt_ghostty_size(g: *mut WtGhostty, cols: *mut u16, rows: *mut u16);
    /// Full reset.
    pub fn wt_ghostty_reset(g: *mut WtGhostty);
    /// Cap the scrollback.
    pub fn wt_ghostty_set_scrollback_lines(g: *mut WtGhostty, lines: usize) -> c_int;
    /// Scroll the viewport: `place` 0 top, 1 bottom, 2 by `delta` rows.
    pub fn wt_ghostty_scroll(g: *mut WtGhostty, place: c_int, delta: isize);
    /// Free bytes the shim allocated.
    pub fn wt_ghostty_free_bytes(ptr: *mut u8, len: usize);
    /// The screen and scrollback as text, shim-allocated.
    pub fn wt_ghostty_screen_text(g: *mut WtGhostty, out: *mut *mut u8, len: *mut usize) -> c_int;
    /// The selection as text, shim-allocated.
    pub fn wt_ghostty_selection_text(
        g: *mut WtGhostty,
        out: *mut *mut u8,
        len: *mut usize,
    ) -> c_int;
    /// Select the whole buffer.
    pub fn wt_ghostty_select_all(g: *mut WtGhostty) -> c_int;
    /// Select viewport cells `(x0, y0)` through `(x1, y1)`.
    pub fn wt_ghostty_select_viewport(
        g: *mut WtGhostty,
        x0: u16,
        y0: u16,
        x1: u16,
        y1: u16,
    ) -> c_int;
    /// Drop the selection.
    pub fn wt_ghostty_clear_selection(g: *mut WtGhostty);
    /// Encode a paste for the current modes into `out`.
    pub fn wt_ghostty_encode_paste(
        g: *mut WtGhostty,
        text: *mut c_char,
        len: usize,
        out: *mut c_char,
        cap: usize,
        written: *mut usize,
    ) -> c_int;
    /// Encode a key press for the current modes into `out`.
    pub fn wt_ghostty_encode_key(
        g: *mut WtGhostty,
        key: *const WtKey,
        out: *mut c_char,
        cap: usize,
        written: *mut usize,
    ) -> c_int;
    /// Snapshot the viewport into `frame` and up to `cap` cells.
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
    /// `sizeof(WtCell)` as the C compiler sees it.
    pub fn wt_ghostty_cell_size() -> usize;
    /// `sizeof(WtFrame)` as the C compiler sees it.
    pub fn wt_ghostty_frame_size() -> usize;
}
