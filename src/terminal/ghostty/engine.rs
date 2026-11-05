//! [`Engine`]: a safe, display-free owner of one libghostty-vt terminal.
//! The backend and the render widget share it; tests and the benchmark use
//! it without GTK.

mod effects;
mod frame;
mod png;

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::Once;

use super::ffi;
pub use super::ffi::{
    WtCell as Cell, WtFrame as FrameInfo, WtMouse as MouseEvent, WtPlacement as Placement,
    WtSpan as Span,
};
use crate::terminal::color_scheme::{ColorScheme, Rgb};
use effects::on_effect;
pub use effects::{ClipboardDestination, Effect};
pub(super) use frame::push_cluster;
pub use frame::{Frame, Pixels};
use png::decode_png;

/// Where [`Engine::scroll`] moves the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Top,
    Bottom,
    /// Rows; up is negative.
    By(isize),
}

/// The mouse tracking mode a program enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracking {
    /// Mode 9: presses only.
    X10,
    /// Mode 1000: presses and releases.
    Normal,
    /// Mode 1002: also motion with a button held.
    Button,
    /// Mode 1003: all motion.
    Any,
}

/// The terminal modes that decide where a mouse event goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModes {
    /// `None` without mouse tracking.
    pub tracking: Option<Tracking>,
    /// Mode 1007, on until a program resets it: on the alternate screen
    /// the wheel sends arrow keys.
    pub alt_scroll: bool,
    /// Mode 1047 or 1049.
    pub alt_screen: bool,
}

/// One libghostty-vt terminal. Not `Send`: it lives with its widget on the
/// main thread. Effects raised while bytes are processed are collected and
/// returned by the call that raised them.
pub struct Engine {
    raw: NonNull<ffi::WtGhostty>,
    /// The effect callback's userdata; boxed so its address is stable.
    effects: Box<RefCell<Vec<Effect>>>,
}

/// A byte slice the shim lends for the length of a callback.
///
/// # Safety
/// `data`/`len` is null or empty, or valid for reads for `'a`.
unsafe fn borrowed<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if data.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: as the caller promises.
        unsafe { std::slice::from_raw_parts(data, len) }
    }
}

/// Notification callback: records like [`on_effect`]. The terminal's
/// parser drops control bytes, so the strings hold no NUL.
unsafe extern "C" fn on_notification(
    userdata: *mut c_void,
    title: *const u8,
    title_len: usize,
    body: *const u8,
    body_len: usize,
) {
    // SAFETY: as in `on_effect`; both strings are borrowed for the call.
    let effects = unsafe { &*(userdata as *const RefCell<Vec<Effect>>) };
    let text = |bytes| String::from_utf8_lossy(bytes).into_owned();
    let effect = Effect::Notification {
        title: text(unsafe { borrowed(title, title_len) }),
        body: text(unsafe { borrowed(body, body_len) }),
    };
    if let Ok(mut effects) = effects.try_borrow_mut() {
        effects.push(effect);
    }
}

/// Take ownership of a buffer libghostty-vt allocated.
fn take_bytes(ptr: *mut u8, len: usize) -> Vec<u8> {
    if ptr.is_null() {
        return Vec::new();
    }
    // SAFETY: ptr/len come from a successful *_alloc call; copied, then
    // freed with the allocator that made them.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    unsafe { ffi::wt_ghostty_free_bytes(ptr, len) };
    bytes
}

impl Engine {
    /// A terminal of `cols` × `rows` cells; `None` if allocation fails.
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        static PNG_DECODER: Once = Once::new();
        // SAFETY: a plain function, set once before any terminal uses it.
        PNG_DECODER.call_once(|| unsafe { ffi::wt_ghostty_set_png_decoder(Some(decode_png)) });
        // SAFETY: plain constructor; null on failure.
        let raw = NonNull::new(unsafe { ffi::wt_ghostty_new(cols.max(1), rows.max(1)) })?;
        let effects: Box<RefCell<Vec<Effect>>> = Box::default();
        // SAFETY: the box outlives the terminal (see Drop: the terminal is
        // freed first, while `effects` is still alive).
        unsafe {
            ffi::wt_ghostty_set_effect(
                raw.as_ptr(),
                Some(on_effect),
                &*effects as *const RefCell<Vec<Effect>> as *mut c_void,
            );
            ffi::wt_ghostty_set_notification(raw.as_ptr(), Some(on_notification));
        }
        Some(Self { raw, effects })
    }

    pub(super) fn ptr(&self) -> *mut ffi::WtGhostty {
        self.raw.as_ptr()
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.borrow_mut())
    }

    /// Feed remote bytes through the VT parser.
    pub fn write(&self, data: &[u8]) -> Vec<Effect> {
        // SAFETY: valid terminal; the slice is borrowed for the call.
        unsafe { ffi::wt_ghostty_write(self.ptr(), data.as_ptr(), data.len()) };
        self.take_effects()
    }

    /// Resize the grid (reflowing it); cell pixels feed size reports.
    pub fn resize(&self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) -> bool {
        // SAFETY: valid terminal.
        let result =
            unsafe { ffi::wt_ghostty_resize(self.ptr(), cols.max(1), rows.max(1), cell_w, cell_h) };
        result == ffi::SUCCESS
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let (mut cols, mut rows) = (0, 0);
        // SAFETY: valid terminal and out-pointers.
        unsafe { ffi::wt_ghostty_size(self.ptr(), &mut cols, &mut rows) };
        (cols, rows)
    }

    pub fn reset(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_reset(self.ptr()) };
    }

    pub fn set_scrollback_lines(&self, lines: usize) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_set_scrollback_lines(self.ptr(), lines) };
    }

    /// Make `scheme` the default colors: foreground, background, cursor and
    /// the 16 ANSI colors; 256-color indices 16–255 keep xterm's cube and
    /// grayscale ramp. The next snapshot resolves through them, cells
    /// already on screen included; colors a program set stay in force.
    pub fn set_color_scheme(&self, scheme: &ColorScheme) -> bool {
        let bytes = |Rgb(r, g, b): Rgb| [r, g, b];
        let (fg, bg, cursor) = (
            bytes(scheme.foreground),
            bytes(scheme.background),
            bytes(scheme.cursor),
        );
        let ansi = scheme.palette.map(bytes);
        // SAFETY: valid terminal; the shim reads 3 bytes from each color
        // and 16 × 3 from `ansi`, all live for the call.
        let result = unsafe {
            ffi::wt_ghostty_set_colors(
                self.ptr(),
                fg.as_ptr(),
                bg.as_ptr(),
                cursor.as_ptr(),
                ansi.as_ptr().cast(),
            )
        };
        result == ffi::SUCCESS
    }

    pub fn scroll(&self, scroll: Scroll) {
        let (place, delta) = match scroll {
            Scroll::Top => (0, 0),
            Scroll::Bottom => (1, 0),
            Scroll::By(rows) => (2, rows),
        };
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_scroll(self.ptr(), place, delta) };
    }

    /// The active screen as plain text, scrollback included.
    pub fn screen_text(&self) -> Option<String> {
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
        // SAFETY: valid terminal and out-pointers.
        let result = unsafe { ffi::wt_ghostty_screen_text(self.ptr(), &mut ptr, &mut len) };
        (result == ffi::SUCCESS)
            .then(|| String::from_utf8_lossy(&take_bytes(ptr, len)).into_owned())
    }

    /// The selection as plain text; `None` without a selection.
    pub fn selection_text(&self) -> Option<String> {
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
        // SAFETY: valid terminal and out-pointers.
        let result = unsafe { ffi::wt_ghostty_selection_text(self.ptr(), &mut ptr, &mut len) };
        let bytes = take_bytes(ptr, len);
        (result == ffi::SUCCESS).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn select_all(&self) -> bool {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_select_all(self.ptr()) == ffi::SUCCESS }
    }

    pub fn clear_selection(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_clear_selection(self.ptr()) };
    }

    /// A selecting press on viewport `cell`, at pixel `at`. A press within
    /// `repeat_ms` and `repeat_px` of the last continues its click count:
    /// the second selects the word under it, the third the line, and a
    /// first press selects nothing.
    pub fn selection_press(
        &self,
        cell: (u16, u16),
        at: (f64, f64),
        time_ms: u32,
        repeat_ms: u32,
        repeat_px: f64,
    ) -> bool {
        let ns = |ms: u32| u64::from(ms) * 1_000_000;
        // SAFETY: valid terminal; out-of-range points are rejected by C.
        unsafe {
            ffi::wt_ghostty_selection_press(
                self.ptr(),
                cell.0,
                cell.1,
                at.0,
                at.1,
                ns(time_ms),
                ns(repeat_ms),
                repeat_px,
            ) == ffi::SUCCESS
        }
    }

    /// The pointer dragged to `cell`: extends by cell, word or line, as
    /// the press that began it. `cell_w` and `height` are in pixels; a
    /// cell counts once the pointer crosses most of it.
    pub fn selection_drag(
        &self,
        cell: (u16, u16),
        at: (f64, f64),
        cell_w: u32,
        height: u32,
    ) -> bool {
        // SAFETY: valid terminal; out-of-range points are rejected by C.
        unsafe {
            ffi::wt_ghostty_selection_drag(self.ptr(), cell.0, cell.1, at.0, at.1, cell_w, height)
                == ffi::SUCCESS
        }
    }

    pub fn selection_release(&self, cell: (u16, u16)) {
        // SAFETY: valid terminal; out-of-range points are rejected by C.
        unsafe { ffi::wt_ghostty_selection_release(self.ptr(), cell.0, cell.1) };
    }

    /// Forget the click sequence: the next press is a first press.
    pub fn selection_cancel(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_selection_cancel(self.ptr()) };
    }

    /// Make the next match of `needle` current, `backward` toward older
    /// lines, and scroll it into view: plain text, case-sensitive, through
    /// the scrollback, wrapping around. A new query starts from the
    /// viewport, forward from its top and backward from its bottom. `false`
    /// when nothing matches; an empty needle ends the search. The selection
    /// is left alone.
    pub fn search(&self, needle: &str, backward: bool) -> bool {
        // SAFETY: valid terminal; the needle is borrowed for the call.
        let result =
            unsafe { ffi::wt_ghostty_search(self.ptr(), needle.as_ptr(), needle.len(), backward) };
        result == ffi::SUCCESS
    }

    /// End the search; nothing is highlighted until the next one.
    pub fn clear_search(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_search_clear(self.ptr()) };
    }

    /// The search's highlight on the viewport: a span per row each match
    /// covers, the current match's marked `current`. Empty without a
    /// search. Catches the search up with the terminal first.
    pub fn search_spans(&self) -> Vec<Span> {
        let mut spans = Vec::new();
        let mut needed = 0;
        for _ in 0..2 {
            // SAFETY: valid terminal; `spans` has `len` initialized slots.
            let result = unsafe {
                ffi::wt_ghostty_search_spans(
                    self.ptr(),
                    spans.as_mut_ptr(),
                    spans.len(),
                    &mut needed,
                )
            };
            match result {
                ffi::SUCCESS => {
                    spans.truncate(needed);
                    return spans;
                }
                ffi::OUT_OF_SPACE => spans.resize(needed, Span::default()),
                _ => break,
            }
        }
        Vec::new()
    }

    /// Call `encode(out, cap, written)` with a buffer that grows once if the
    /// first attempt reports how much it needs.
    fn encoded(mut encode: impl FnMut(*mut c_char, usize, &mut usize) -> c_int) -> Vec<u8> {
        let mut out = vec![0u8; 128];
        let mut written = 0;
        let mut result = encode(out.as_mut_ptr().cast(), out.len(), &mut written);
        if result == ffi::OUT_OF_SPACE {
            out.resize(written, 0);
            result = encode(out.as_mut_ptr().cast(), out.len(), &mut written);
        }
        if result != ffi::SUCCESS {
            return Vec::new();
        }
        out.truncate(written);
        out
    }

    /// Pasted text as the remote side should receive it: bracketed when
    /// the program enabled mode 2004, unsafe control bytes stripped.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        let mut input = text.as_bytes().to_vec();
        Self::encoded(|out, cap, written| {
            // The encoder rewrites its input in place; start from a fresh copy.
            input.copy_from_slice(text.as_bytes());
            // SAFETY: valid terminal; input/out are live buffers of the
            // stated lengths.
            unsafe {
                ffi::wt_ghostty_encode_paste(
                    self.ptr(),
                    input.as_mut_ptr().cast(),
                    input.len(),
                    out,
                    cap,
                    written,
                )
            }
        })
    }

    /// One key press, encoded for the terminal's current modes; empty when
    /// the key produces nothing.
    pub fn encode_key(&self, keysym: u32, unshifted: u32, mods: u32, text: &str) -> Vec<u8> {
        let key = ffi::WtKey {
            keysym,
            unshifted,
            mods,
            utf8: text.as_ptr().cast(),
            utf8_len: text.len(),
        };
        Self::encoded(|out, cap, written| {
            // SAFETY: valid terminal; `key` borrows `text` for the call.
            unsafe { ffi::wt_ghostty_encode_key(self.ptr(), &key, out, cap, written) }
        })
    }

    fn dec_mode(&self, value: u16) -> bool {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_dec_mode(self.ptr(), value) }
    }

    /// The program's mouse modes; with several tracking modes set, the
    /// one reporting the most wins.
    pub fn mouse_modes(&self) -> MouseModes {
        let tracking = [
            (1003, Tracking::Any),
            (1002, Tracking::Button),
            (1000, Tracking::Normal),
            (9, Tracking::X10),
        ]
        .into_iter()
        .find(|&(mode, _)| self.dec_mode(mode))
        .map(|(_, tracking)| tracking);
        MouseModes {
            tracking,
            alt_scroll: self.dec_mode(1007),
            alt_screen: self.dec_mode(1047) || self.dec_mode(1049),
        }
    }

    /// One mouse event, encoded for the terminal's tracking mode and
    /// format (X10, SGR, …); empty when the mode does not report it.
    pub fn encode_mouse(&self, event: &MouseEvent) -> Vec<u8> {
        Self::encoded(|out, cap, written| {
            // SAFETY: valid terminal; `event` is borrowed for the call.
            unsafe { ffi::wt_ghostty_encode_mouse(self.ptr(), event, out, cap, written) }
        })
    }

    /// A focus change as the program asked for it (mode 1004); empty when
    /// it did not.
    pub fn encode_focus(&self, gained: bool) -> Vec<u8> {
        Self::encoded(|out, cap, written| {
            // SAFETY: valid terminal.
            unsafe { ffi::wt_ghostty_encode_focus(self.ptr(), gained, out, cap, written) }
        })
    }

    /// Move playing animations on to `now_ms`, a clock that only goes
    /// forward; once per frame, before [`Self::snapshot`]. When the next
    /// frame is due on that clock, `None` while nothing plays.
    pub fn tick_animations(&self, now_ms: u64) -> Option<u64> {
        let mut next_ms = 0;
        // SAFETY: valid terminal and out-pointer.
        let result = unsafe { ffi::wt_ghostty_animation_tick(self.ptr(), now_ms, &mut next_ms) };
        (result == ffi::SUCCESS).then_some(next_ms)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: we own the terminal; freed before `effects`.
        unsafe { ffi::wt_ghostty_free(self.ptr()) };
    }
}

#[cfg(test)]
mod tests;
