//! [`Engine`]: a safe, display-free owner of one libghostty-vt terminal.
//! The backend and the render widget share it; tests and the benchmark use
//! it without GTK.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::ptr::NonNull;

use super::ffi;
pub use super::ffi::{WtCell as Cell, WtFrame as FrameInfo};

/// What the terminal asked for while processing bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A reply for the remote side (device attributes, status reports).
    Pty(Vec<u8>),
    /// BEL.
    Bell,
    /// OSC 0/2 window title.
    Title(String),
}

/// Where [`Engine::scroll`] moves the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// The oldest scrollback line.
    Top,
    /// The live screen.
    Bottom,
    /// Rows; up is negative.
    By(isize),
}

/// A snapshot of the viewport, reused between frames.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Grid size, cursor, and default colors.
    pub info: FrameInfo,
    /// Row-major, `info.cols` per row.
    pub cells: Vec<Cell>,
}

impl Frame {
    /// The cell at `row`, `col` of the viewport, if inside it.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if col >= usize::from(self.info.cols) {
            return None;
        }
        self.cells.get(row * usize::from(self.info.cols) + col)
    }

    /// One viewport row as text, empty cells as spaces.
    pub fn row_text(&self, row: usize) -> String {
        (0..usize::from(self.info.cols))
            .filter_map(|col| self.cell(row, col))
            .map(|cell| {
                char::from_u32(cell.codepoint)
                    .filter(|c| *c != '\0')
                    .unwrap_or(' ')
            })
            .collect()
    }
}

/// One libghostty-vt terminal. Not `Send`: it lives with its widget on the
/// main thread. Effects raised while bytes are processed are collected and
/// returned by the call that raised them.
pub struct Engine {
    /// The libghostty-vt terminal, freed on drop.
    raw: NonNull<ffi::WtGhostty>,
    /// The effect callback's userdata; boxed so its address is stable.
    effects: Box<RefCell<Vec<Effect>>>,
}

/// Effect callback: only records, so it never re-enters the terminal.
unsafe extern "C" fn on_effect(userdata: *mut c_void, kind: c_int, data: *const u8, len: usize) {
    // SAFETY: userdata is the engine's boxed effect list, alive for as long
    // as the terminal that calls this; data/len is a borrowed byte slice.
    let effects = unsafe { &*(userdata as *const RefCell<Vec<Effect>>) };
    let bytes = if data.is_null() || len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    let effect = match kind {
        ffi::EFFECT_PTY => Effect::Pty(bytes.to_vec()),
        ffi::EFFECT_BELL => Effect::Bell,
        ffi::EFFECT_TITLE => Effect::Title(String::from_utf8_lossy(bytes).into_owned()),
        _ => return,
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
        }
        Some(Self { raw, effects })
    }

    /// The raw terminal for FFI calls.
    fn ptr(&self) -> *mut ffi::WtGhostty {
        self.raw.as_ptr()
    }

    /// Drain the effects recorded since the last call.
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

    /// Full reset: screen, scrollback, modes.
    pub fn reset(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_reset(self.ptr()) };
    }

    /// Cap the scrollback at `lines`.
    pub fn set_scrollback_lines(&self, lines: usize) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_set_scrollback_lines(self.ptr(), lines) };
    }

    /// Move the viewport.
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

    /// Select the whole buffer; `false` if the terminal refused.
    pub fn select_all(&self) -> bool {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_select_all(self.ptr()) == ffi::SUCCESS }
    }

    /// Select viewport cells `from` through `to`, each `(col, row)`.
    pub fn select(&self, from: (u16, u16), to: (u16, u16)) -> bool {
        // SAFETY: valid terminal; out-of-range points are rejected by C.
        unsafe {
            ffi::wt_ghostty_select_viewport(self.ptr(), from.0, from.1, to.0, to.1) == ffi::SUCCESS
        }
    }

    /// Drop the selection.
    pub fn clear_selection(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_clear_selection(self.ptr()) };
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

    /// Refresh `frame` from the terminal's viewport.
    pub fn snapshot(&self, frame: &mut Frame) -> bool {
        for _ in 0..2 {
            // SAFETY: valid terminal; `cells` has `len` initialized slots.
            let result = unsafe {
                ffi::wt_ghostty_snapshot(
                    self.ptr(),
                    &mut frame.info,
                    frame.cells.as_mut_ptr(),
                    frame.cells.len(),
                )
            };
            match result {
                ffi::SUCCESS => return true,
                ffi::OUT_OF_SPACE => {
                    let needed = usize::from(frame.info.cols) * usize::from(frame.info.rows);
                    frame.cells.resize(needed, Cell::default());
                }
                _ => return false,
            }
        }
        false
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: we own the terminal; freed before `effects`.
        unsafe { ffi::wt_ghostty_free(self.ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(20, 5).expect("engine")
    }

    fn frame(engine: &Engine) -> Frame {
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        frame
    }

    #[test]
    fn rust_mirrors_match_the_c_layout() {
        // SAFETY: plain getters.
        let (cell, info) = unsafe { (ffi::wt_ghostty_cell_size(), ffi::wt_ghostty_frame_size()) };
        assert_eq!(std::mem::size_of::<Cell>(), cell);
        assert_eq!(std::mem::size_of::<FrameInfo>(), info);
    }

    #[test]
    fn text_lands_in_the_grid_and_cursor_moves() {
        let engine = engine();
        engine.write(b"hello\r\nworld");
        let frame = frame(&engine);
        assert_eq!((frame.info.cols, frame.info.rows), (20, 5));
        assert_eq!(frame.row_text(0).trim_end(), "hello");
        assert_eq!(frame.row_text(1).trim_end(), "world");
        assert_eq!((frame.info.cursor_x, frame.info.cursor_y), (5, 1));
    }

    #[test]
    fn sgr_styles_and_colors_are_resolved() {
        let engine = engine();
        engine.write(b"\x1b[1;38;2;255;128;0;48;5;4mX\x1b[0mY");
        let frame = frame(&engine);
        let x = frame.cell(0, 0).unwrap();
        assert_eq!(x.codepoint, u32::from('X'));
        assert_ne!(x.flags & ffi::CELL_BOLD, 0);
        assert_eq!(x.fg, [255, 128, 0]);
        assert_eq!(x.has_bg, 1);
        let y = frame.cell(0, 1).unwrap();
        assert_eq!(y.flags, 0);
        assert_eq!(y.has_bg, 0);
    }

    #[test]
    fn bell_title_and_device_attributes_raise_effects() {
        let engine = engine();
        let effects = engine.write(b"\x07\x1b]2;remote title\x07\x1b[c");
        assert!(effects.contains(&Effect::Bell));
        assert!(effects.contains(&Effect::Title("remote title".into())));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Pty(reply) if reply.starts_with(b"\x1b[?"))),
            "a primary DA reply: {effects:?}"
        );
        assert!(engine.write(b"plain").is_empty());
    }

    #[test]
    fn resize_reflows_and_reports_the_new_grid() {
        let engine = engine();
        assert!(engine.resize(40, 10, 8, 16));
        assert_eq!(engine.size(), (40, 10));
        assert_eq!(frame(&engine).cells.len(), 400);
    }

    #[test]
    fn screen_text_includes_scrollback() {
        let engine = engine();
        for line in 0..20 {
            engine.write(format!("line {line}\r\n").as_bytes());
        }
        let text = engine.screen_text().unwrap();
        assert!(text.contains("line 0\n"), "{text:?}");
        assert!(text.contains("line 19"), "{text:?}");
    }

    #[test]
    fn selection_select_all_and_clear() {
        let engine = engine();
        engine.write(b"abc def");
        assert_eq!(engine.selection_text(), None);
        assert!(engine.select((0, 0), (2, 0)));
        assert_eq!(engine.selection_text().as_deref(), Some("abc"));
        let frame = frame(&engine);
        assert_ne!(frame.cell(0, 0).unwrap().flags & ffi::CELL_SELECTED, 0);
        assert_eq!(frame.cell(0, 4).unwrap().flags & ffi::CELL_SELECTED, 0);
        assert!(engine.select_all());
        assert!(engine.selection_text().unwrap().contains("abc def"));
        engine.clear_selection();
        assert_eq!(engine.selection_text(), None);
    }

    #[test]
    fn keys_follow_terminal_modes() {
        let engine = engine();
        assert_eq!(
            engine.encode_key(u32::from('a'), u32::from('a'), 0, "a"),
            b"a"
        );
        assert_eq!(engine.encode_key(0xff0d, 0, 0, "\r"), b"\r");
        assert_eq!(
            engine.encode_key(u32::from('c'), u32::from('c'), ffi::MOD_CTRL, "c"),
            b"\x03"
        );
        assert_eq!(engine.encode_key(0xff52, 0, 0, ""), b"\x1b[A");
        // DECCKM (application cursor keys) switches arrows to SS3.
        engine.write(b"\x1b[?1h");
        assert_eq!(engine.encode_key(0xff52, 0, 0, ""), b"\x1bOA");
    }

    #[test]
    fn paste_is_bracketed_only_when_the_program_asks() {
        let engine = engine();
        assert_eq!(engine.encode_paste("a\nb"), b"a\rb");
        engine.write(b"\x1b[?2004h");
        assert_eq!(engine.encode_paste("a\nb"), b"\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn reset_clears_the_screen() {
        let engine = engine();
        engine.write(b"junk");
        engine.reset();
        assert_eq!(frame(&engine).row_text(0).trim(), "");
    }

    #[test]
    fn scrolling_moves_the_viewport() {
        let engine = engine();
        for line in 0..20 {
            engine.write(format!("line {line}\r\n").as_bytes());
        }
        engine.scroll(Scroll::Top);
        assert_eq!(frame(&engine).row_text(0).trim_end(), "line 0");
        engine.scroll(Scroll::Bottom);
        assert_ne!(frame(&engine).row_text(0).trim_end(), "line 0");
    }
}
