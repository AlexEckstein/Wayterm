//! [`Engine`]: a safe, display-free owner of one libghostty-vt terminal.
//! The backend and the render widget share it; tests and the benchmark use
//! it without GTK.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::ptr::NonNull;

use super::ffi;
pub use super::ffi::{WtCell as Cell, WtFrame as FrameInfo, WtMouse as MouseEvent, WtSpan as Span};

/// What the terminal asked for while processing bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A reply for the remote side (device attributes, status reports).
    Pty(Vec<u8>),
    Bell,
    Title(String),
    /// The directory the shell reported, raw: a `file://` URI for OSC 7.
    Pwd(String),
}

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

/// A snapshot of the viewport, reused between frames.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub info: FrameInfo,
    /// Row-major, `info.cols` per row.
    pub cells: Vec<Cell>,
}

impl Frame {
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
        ffi::EFFECT_PWD => Effect::Pwd(String::from_utf8_lossy(bytes).into_owned()),
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

    fn ptr(&self) -> *mut ffi::WtGhostty {
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

    /// Select viewport cells `from` through `to`, each `(col, row)`.
    pub fn select(&self, from: (u16, u16), to: (u16, u16)) -> bool {
        // SAFETY: valid terminal; out-of-range points are rejected by C.
        unsafe {
            ffi::wt_ghostty_select_viewport(self.ptr(), from.0, from.1, to.0, to.1) == ffi::SUCCESS
        }
    }

    pub fn clear_selection(&self) {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_clear_selection(self.ptr()) };
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
        let (cell, info, span) = unsafe {
            (
                ffi::wt_ghostty_cell_size(),
                ffi::wt_ghostty_frame_size(),
                ffi::wt_ghostty_span_size(),
            )
        };
        assert_eq!(std::mem::size_of::<Cell>(), cell);
        assert_eq!(std::mem::size_of::<FrameInfo>(), info);
        assert_eq!(std::mem::size_of::<Span>(), span);
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
    fn an_osc_7_report_raises_the_raw_uri() {
        let engine = engine();
        assert_eq!(
            engine.write(b"\x1b]7;file://box/srv/my%20site\x07"),
            vec![Effect::Pwd("file://box/srv/my%20site".into())]
        );
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

    /// A mouse event over a 20×5 grid of 10×20-pixel cells, at the centre
    /// of cell `(col, row)`.
    fn mouse(action: u32, button: u32, mods: u32, (col, row): (u16, u16)) -> MouseEvent {
        MouseEvent {
            action,
            button,
            mods,
            x: f32::from(col) * 10.0 + 5.0,
            y: f32::from(row) * 20.0 + 10.0,
            width: 200,
            height: 100,
            cell_w: 10,
            cell_h: 20,
            any_pressed: u32::from(action != ffi::MOUSE_RELEASE && button == ffi::BUTTON_LEFT),
        }
    }

    #[test]
    fn mouse_modes_report_tracking_alt_scroll_and_the_alt_screen() {
        let engine = engine();
        // libghostty-vt starts with alternate scroll on.
        let start = MouseModes {
            tracking: None,
            alt_scroll: true,
            alt_screen: false,
        };
        assert_eq!(engine.mouse_modes(), start);
        engine.write(b"\x1b[?1007l");
        assert!(!engine.mouse_modes().alt_scroll);
        engine.write(b"\x1b[?9h");
        assert_eq!(engine.mouse_modes().tracking, Some(Tracking::X10));
        engine.write(b"\x1b[?1000h");
        assert_eq!(engine.mouse_modes().tracking, Some(Tracking::Normal));
        engine.write(b"\x1b[?1002h");
        assert_eq!(engine.mouse_modes().tracking, Some(Tracking::Button));
        engine.write(b"\x1b[?1003h");
        assert_eq!(engine.mouse_modes().tracking, Some(Tracking::Any));
        engine.write(b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l\x1b[?1007h");
        let modes = engine.mouse_modes();
        assert_eq!(modes.tracking, None);
        assert!(modes.alt_scroll && !modes.alt_screen);
        engine.write(b"\x1b[?1049h");
        assert!(engine.mouse_modes().alt_screen);
        engine.write(b"\x1b[?1049l\x1b[?1047h");
        assert!(engine.mouse_modes().alt_screen);
        engine.write(b"\x1b[?1047l");
        assert!(!engine.mouse_modes().alt_screen);
    }

    #[test]
    fn sgr_mouse_reports_press_release_wheel_and_modifiers() {
        let engine = engine();
        engine.write(b"\x1b[?1000h\x1b[?1006h");
        let press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, 0, (3, 2));
        // SGR: CSI < button ; column ; row, 1-based; M presses, m releases.
        assert_eq!(engine.encode_mouse(&press), b"\x1b[<0;4;3M");
        let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, 0, (3, 2));
        assert_eq!(engine.encode_mouse(&release), b"\x1b[<0;4;3m");
        // Normal tracking (1000) reports no motion.
        let motion = mouse(ffi::MOUSE_MOTION, ffi::BUTTON_LEFT, 0, (4, 2));
        assert_eq!(engine.encode_mouse(&motion), b"");
        // The wheel is buttons 4 and 5, codes 64 and 65.
        let up = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_UP, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&up), b"\x1b[<64;1;1M");
        let down = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_DOWN, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&down), b"\x1b[<65;1;1M");
        // Shift adds 4, Ctrl 16.
        let mods = mouse(
            ffi::MOUSE_PRESS,
            ffi::BUTTON_LEFT,
            ffi::MOD_SHIFT | ffi::MOD_CTRL,
            (0, 0),
        );
        assert_eq!(engine.encode_mouse(&mods), b"\x1b[<20;1;1M");
    }

    #[test]
    fn button_tracking_reports_drags_and_any_tracking_bare_motion() {
        let engine = engine();
        engine.write(b"\x1b[?1002h\x1b[?1006h");
        // Motion adds 32 to the button code.
        let drag = mouse(ffi::MOUSE_MOTION, ffi::BUTTON_LEFT, 0, (4, 2));
        assert_eq!(engine.encode_mouse(&drag), b"\x1b[<32;5;3M");
        let hover = mouse(ffi::MOUSE_MOTION, ffi::BUTTON_NONE, 0, (4, 2));
        assert_eq!(engine.encode_mouse(&hover), b"", "1002 needs a button");
        engine.write(b"\x1b[?1003h");
        // No button is code 3, plus 32 for motion.
        assert_eq!(engine.encode_mouse(&hover), b"\x1b[<35;5;3M");
    }

    #[test]
    fn without_sgr_the_mouse_uses_the_x10_byte_format() {
        let engine = engine();
        engine.write(b"\x1b[?1000h");
        // CSI M, then button, column and row as 32 + value (1-based).
        let press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, 0, (3, 2));
        assert_eq!(engine.encode_mouse(&press), b"\x1b[M $#");
        // Legacy releases do not say which button: code 3.
        let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, 0, (3, 2));
        assert_eq!(engine.encode_mouse(&release), b"\x1b[M#$#");
    }

    #[test]
    fn x10_mode_reports_presses_only_and_no_tracking_reports_nothing() {
        let engine = engine();
        let press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, ffi::MOD_SHIFT, (0, 0));
        assert_eq!(engine.encode_mouse(&press), b"");
        engine.write(b"\x1b[?9h");
        // X10 carries no modifiers.
        assert_eq!(engine.encode_mouse(&press), b"\x1b[M !!");
        let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&release), b"");
        let wheel = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_UP, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&wheel), b"");
    }

    #[test]
    fn pixels_past_the_grid_count_as_its_last_cell() {
        let engine = engine();
        engine.write(b"\x1b[?1000h\x1b[?1006h");
        // The widget is 7 pixels wider and taller than the 20×5 grid.
        let mut press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, 0, (0, 0));
        (press.width, press.height, press.x, press.y) = (207, 107, 204.0, 105.0);
        assert_eq!(engine.encode_mouse(&press), b"\x1b[<0;20;5M");
    }

    #[test]
    fn focus_events_only_when_the_program_asks() {
        let engine = engine();
        assert_eq!(engine.encode_focus(true), b"");
        assert_eq!(engine.encode_focus(false), b"");
        engine.write(b"\x1b[?1004h");
        assert_eq!(engine.encode_focus(true), b"\x1b[I");
        assert_eq!(engine.encode_focus(false), b"\x1b[O");
        engine.write(b"\x1b[?1004l");
        assert_eq!(engine.encode_focus(true), b"");
    }

    #[test]
    fn reset_clears_the_screen() {
        let engine = engine();
        engine.write(b"junk");
        engine.reset();
        assert_eq!(frame(&engine).row_text(0).trim(), "");
    }

    /// 30 lines on a 5-row screen, then the cursor's empty line: lines 0-25
    /// are scrollback, 26-29 on screen. "Fox" is on lines 3, 12 (both
    /// scrollback) and 27 (on screen), "fox" on line 20.
    fn foxes() -> Engine {
        let engine = engine();
        for line in 0..30 {
            let word = match line {
                3 | 12 | 27 => " Fox",
                20 => " fox",
                _ => "",
            };
            engine.write(format!("line {line}{word}\r\n").as_bytes());
        }
        engine
    }

    /// The viewport row the current match is on, as text.
    fn current_line(engine: &Engine) -> Option<String> {
        let span = engine.search_spans().into_iter().find(|s| s.current != 0)?;
        Some(
            frame(engine)
                .row_text(span.row.into())
                .trim_end()
                .to_string(),
        )
    }

    /// Search `needle` `steps` times; the line of each current match.
    fn walk(engine: &Engine, needle: &str, backward: bool, steps: usize) -> Vec<String> {
        (0..steps)
            .map(|_| {
                assert!(engine.search(needle, backward), "{needle:?} not found");
                current_line(engine).expect("a highlighted current match")
            })
            .collect()
    }

    #[test]
    fn forward_search_starts_at_the_viewport_top_and_wraps() {
        let engine = foxes();
        assert_eq!(
            walk(&engine, "Fox", false, 4),
            ["line 27 Fox", "line 3 Fox", "line 12 Fox", "line 27 Fox"]
        );
    }

    #[test]
    fn backward_search_starts_at_the_viewport_bottom_and_wraps() {
        let engine = foxes();
        assert_eq!(
            walk(&engine, "Fox", true, 4),
            ["line 27 Fox", "line 12 Fox", "line 3 Fox", "line 27 Fox"]
        );
        // Turning around steps back the way it came.
        assert_eq!(walk(&engine, "Fox", false, 1), ["line 3 Fox"]);
    }

    /// The same answers as the VTE widget on the same screen.
    #[test]
    fn a_new_search_starts_from_a_scrolled_viewport() {
        for backward in [false, true] {
            let engine = foxes();
            engine.scroll(Scroll::Top);
            assert_eq!(walk(&engine, "Fox", backward, 1), ["line 3 Fox"]);

            // Lines 10-14 on screen.
            let engine = foxes();
            engine.scroll(Scroll::Top);
            engine.scroll(Scroll::By(10));
            assert_eq!(walk(&engine, "Fox", backward, 1), ["line 12 Fox"]);
            assert_eq!(frame(&engine).row_text(0).trim_end(), "line 10");
        }
    }

    #[test]
    fn search_is_case_sensitive() {
        let engine = foxes();
        assert_eq!(
            walk(&engine, "fox", true, 2),
            ["line 20 fox", "line 20 fox"]
        );
        assert!(!engine.search("FOX", false));
        assert_eq!(engine.search_spans(), []);
    }

    #[test]
    fn a_match_in_the_scrollback_is_scrolled_to_the_middle() {
        let engine = foxes();
        let bottom = frame(&engine).row_text(0);
        assert!(engine.search("line 3 Fox", false));
        assert_ne!(frame(&engine).row_text(0), bottom, "the viewport moved");
        let current = Span {
            row: 2,
            start: 0,
            end: 10,
            current: 1,
        };
        assert_eq!(engine.search_spans(), [current]);
        assert_eq!(frame(&engine).row_text(2).trim_end(), "line 3 Fox");
    }

    #[test]
    fn a_visible_match_does_not_scroll() {
        let engine = foxes();
        let bottom = frame(&engine).row_text(0);
        assert!(engine.search("line 28", true));
        assert_eq!(frame(&engine).row_text(0), bottom);
    }

    #[test]
    fn an_empty_query_clears_the_highlight_and_restarts() {
        let engine = foxes();
        assert_eq!(
            walk(&engine, "Fox", true, 2),
            ["line 27 Fox", "line 12 Fox"]
        );
        assert!(!engine.search("", true));
        assert_eq!(engine.search_spans(), []);
        // The next search starts again from the viewport, where line 12
        // sits in the middle: from its bottom, backward, line 12 is next.
        assert_eq!(walk(&engine, "Fox", true, 1), ["line 12 Fox"]);
        engine.clear_search();
        assert_eq!(engine.search_spans(), []);
    }

    #[test]
    fn every_visible_match_is_highlighted_and_the_current_one_marked() {
        let engine = engine();
        // "Ab" differs in case; "wrapped" wraps from row 1 onto row 2.
        engine.write(format!("ab Ab ab\r\n{}wrapped", "x".repeat(18)).as_bytes());
        let span = |row, start, end, current| Span {
            row,
            start,
            end,
            current,
        };
        assert!(engine.search("ab", false));
        let mut spans = engine.search_spans();
        spans.sort_by_key(|s| (s.row, s.start));
        assert_eq!(spans, [span(0, 0, 2, 1), span(0, 6, 8, 0)]);
        assert!(engine.search("ab", false));
        let mut spans = engine.search_spans();
        spans.sort_by_key(|s| (s.row, s.start));
        assert_eq!(spans, [span(0, 0, 2, 0), span(0, 6, 8, 1)]);

        assert!(engine.search("wrapped", false));
        let mut spans = engine.search_spans();
        spans.sort_by_key(|s| (s.row, s.start));
        assert_eq!(spans, [span(1, 18, 20, 1), span(2, 0, 5, 1)]);
    }

    #[test]
    fn a_wide_character_is_highlighted_across_both_cells() {
        let engine = engine();
        engine.write("a日本b".as_bytes());
        assert!(engine.search("日本", false));
        let span = Span {
            row: 0,
            start: 1,
            end: 5,
            current: 1,
        };
        assert_eq!(engine.search_spans(), [span]);
        // Letters make the match be read back, wide cells included.
        assert!(engine.search("本b", false));
        let span = Span {
            start: 3,
            end: 6,
            ..span
        };
        assert_eq!(engine.search_spans(), [span]);
    }

    #[test]
    fn spaces_in_a_query_are_read_back_as_typed() {
        let engine = engine();
        engine.write(b"Ab cd Ab");
        assert!(engine.search("Ab ", false));
        let span = Span {
            row: 0,
            start: 0,
            end: 3,
            current: 1,
        };
        assert_eq!(engine.search_spans(), [span]);
    }

    #[test]
    fn a_lone_exact_match_among_case_variants_is_found_quickly() {
        let engine = engine();
        engine.resize(120, 40, 8, 16);
        engine.write(b"the only Exact one\r\n");
        let line = "e ".repeat(59);
        for _ in 0..600 {
            engine.write(format!("{line}\r\n").as_bytes());
        }
        let started = std::time::Instant::now();
        // 35 000 candidates; stepping through them took close to a minute.
        assert!(engine.search("E", true));
        assert!(!engine.search("Z", true));
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }

    #[test]
    fn the_current_match_survives_a_resize() {
        let engine = foxes();
        assert_eq!(
            walk(&engine, "Fox", true, 2),
            ["line 27 Fox", "line 12 Fox"]
        );
        // Narrower than the line: the match wraps onto a second row.
        assert!(engine.resize(9, 5, 8, 16));
        let current: Vec<_> = engine
            .search_spans()
            .into_iter()
            .filter(|s| s.current != 0)
            .map(|s| (s.end - s.start, s.row))
            .collect();
        assert_eq!(current.iter().map(|c| c.0).sum::<u16>(), 3, "{current:?}");
        // It goes on from there, not from the viewport.
        assert!(engine.resize(20, 5, 8, 16));
        assert_eq!(walk(&engine, "Fox", true, 1), ["line 3 Fox"]);
    }

    #[test]
    fn the_alternate_screen_has_its_own_matches() {
        let engine = foxes();
        assert_eq!(walk(&engine, "Fox", true, 1), ["line 27 Fox"]);
        engine.write(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[3Bfull-screen Fox");
        // Lit there too, but the current match stayed behind.
        let lit = Span {
            row: 3,
            start: 12,
            end: 15,
            current: 0,
        };
        assert_eq!(engine.search_spans(), [lit]);
        assert_eq!(walk(&engine, "Fox", true, 2), ["full-screen Fox"; 2]);
        engine.write(b"\x1b[?1049l");
        assert!(engine.search_spans().iter().all(|s| s.current == 0));
        assert_eq!(walk(&engine, "Fox", true, 1), ["line 27 Fox"]);
    }

    #[test]
    fn overwritten_text_is_no_longer_lit() {
        let engine = engine();
        engine.write(b"Fox\r\nFox");
        assert!(engine.search("Fox", true));
        assert_eq!(engine.search_spans().len(), 2);
        engine.write(b"\x1b[2;1Hdog\x1b[1;1Hcat");
        assert_eq!(engine.search_spans(), []);
    }

    #[test]
    fn the_current_match_follows_new_output() {
        let engine = foxes();
        assert_eq!(walk(&engine, "Fox", true, 1), ["line 27 Fox"]);
        for line in 30..40 {
            engine.write(format!("line {line}\r\n").as_bytes());
        }
        // Line 27 scrolled off: nothing current on screen, but the search
        // goes on from it rather than starting over at the bottom.
        assert!(engine.search_spans().iter().all(|s| s.current == 0));
        assert_eq!(walk(&engine, "Fox", true, 1), ["line 12 Fox"]);
    }

    #[test]
    fn search_leaves_the_mouse_selection_alone() {
        let engine = engine();
        engine.write(b"abc abc");
        assert!(engine.select((0, 0), (2, 0)));
        assert!(engine.search("abc", true));
        assert_eq!(engine.selection_text().as_deref(), Some("abc"));
        let frame = frame(&engine);
        let selected = |col| frame.cell(0, col).unwrap().flags & ffi::CELL_SELECTED != 0;
        assert!(selected(0) && !selected(4));
        assert!(!engine.search_spans().is_empty());
        engine.clear_selection();
        assert!(!engine.search_spans().is_empty(), "the highlight stays");
    }

    #[test]
    fn reset_ends_the_search() {
        let engine = foxes();
        assert!(engine.search("Fox", true));
        engine.reset();
        assert_eq!(engine.search_spans(), []);
        engine.write(b"Fox");
        assert_eq!(engine.search_spans(), [], "no search to follow");
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
