//! [`Engine`]: a safe, display-free owner of one libghostty-vt terminal.
//! The backend and the render widget share it; tests and the benchmark use
//! it without GTK.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::Once;

use gtk4::prelude::*;
use gtk4::{gdk, glib};

use super::ffi;
pub use super::ffi::{
    WtCell as Cell, WtFrame as FrameInfo, WtMouse as MouseEvent, WtPlacement as Placement,
    WtSpan as Span,
};
use crate::terminal::color_scheme::{ColorScheme, Rgb};

/// What the terminal asked for while processing bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A reply for the remote side (device attributes, status reports).
    Pty(Vec<u8>),
    Bell,
    Title(String),
    /// The directory the shell reported, raw: a `file://` URI for OSC 7.
    Pwd(String),
    /// A program set a clipboard (OSC 52, OSC 1337 Copy).
    ClipboardWrite {
        destination: ClipboardDestination,
        text: String,
    },
}

/// The clipboard a program's write goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardDestination {
    System,
    /// PRIMARY: OSC 52's `p` and `s`.
    Selection,
}

/// The most decoded bytes a clipboard write may carry. OSC 52 stops short
/// of it: libghostty-vt drops a sequence over 8 MiB, about 6 MiB decoded.
const CLIPBOARD_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Where [`Engine::scroll`] moves the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Top,
    Bottom,
    /// Rows; up is negative.
    By(isize),
    /// Shell prompts from the viewport's top; up is negative. Clamped to
    /// the oldest and newest, and nothing moves without OSC 133 marks.
    Prompts(isize),
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
    /// The clusters of cells holding more than one codepoint, where their
    /// `grapheme` fields point; grown as needed, never shrunk.
    pub graphemes: Vec<u32>,
    /// The images on screen, in drawing order: by layer, then z, then
    /// image, the tie-break Ghostty's own renderer uses.
    pub placements: Vec<Placement>,
    /// The URIs of the OSC 8 hyperlinks in the viewport, end to end.
    uris: Vec<u8>,
    /// Where each entry of `uris` ends; a cell's `link`, from 1, picks one.
    link_ends: Vec<usize>,
}

/// A stored image's pixels, its current frame's for an animation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    /// `ffi::FORMAT_*`.
    pub format: u8,
    /// Rows from the top, with no padding between them.
    pub data: Vec<u8>,
}

impl Frame {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if col >= usize::from(self.info.cols) {
            return None;
        }
        self.cells.get(row * usize::from(self.info.cols) + col)
    }

    /// A cell's whole grapheme cluster, base codepoint first; empty for an
    /// empty cell and for the tail of a wide one.
    pub fn grapheme(&self, row: usize, col: usize) -> &[u32] {
        let Some(cell) = self.cell(row, col) else {
            return &[];
        };
        match cell.grapheme_len {
            0 => &[],
            1 => std::slice::from_ref(&cell.codepoint),
            len => {
                let start = cell.grapheme as usize;
                self.graphemes
                    .get(start..start + usize::from(len))
                    .unwrap_or(&[])
            }
        }
    }

    /// The URI of a cell's OSC 8 hyperlink. `None` without one, and for a
    /// URI that is not UTF-8, which no handler could open as written.
    pub fn link(&self, row: usize, col: usize) -> Option<&str> {
        let entry = usize::from(self.cell(row, col)?.link).checked_sub(1)?;
        let start = match entry.checked_sub(1) {
            Some(previous) => *self.link_ends.get(previous)?,
            None => 0,
        };
        let uri = self.uris.get(start..*self.link_ends.get(entry)?)?;
        std::str::from_utf8(uri).ok()
    }

    /// One viewport row as text, empty cells as spaces.
    pub fn row_text(&self, row: usize) -> String {
        let mut text = String::new();
        for col in 0..usize::from(self.info.cols) {
            push_cluster(&mut text, self.grapheme(row, col));
        }
        text
    }

    /// Whether blinking text shows on screen: cells past the grid are left
    /// over from a larger one, and blank or invisible cells show no change.
    pub fn blinks(&self) -> bool {
        let grid = usize::from(self.info.cols) * usize::from(self.info.rows);
        self.cells.iter().take(grid).any(|cell| {
            cell.flags & ffi::CELL_BLINK != 0
                && cell.flags & ffi::CELL_INVISIBLE == 0
                && char::from_u32(cell.codepoint).is_some_and(|c| c != '\0' && !c.is_whitespace())
        })
    }
}

/// Append a cell's cluster to `text`, or a space for an empty cell (the
/// tail of a wide one included), so each cell adds at least one character.
pub(super) fn push_cluster(text: &mut String, cluster: &[u32]) {
    let start = text.len();
    text.extend(
        cluster
            .iter()
            .filter_map(|&c| char::from_u32(c))
            .filter(|&c| c != '\0'),
    );
    if text.len() == start {
        text.push(' ');
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

/// A program's clipboard write, whole or not at all: one too large, not
/// UTF-8, or holding a NUL, which would cut GDK's text short, is dropped
/// with a note that leaves its contents out.
fn clipboard_effect(destination: ClipboardDestination, bytes: &[u8]) -> Option<Effect> {
    if bytes.len() > CLIPBOARD_MAX_BYTES {
        eprintln!(
            "Wayterm: ignored a clipboard write of {} bytes, over the {CLIPBOARD_MAX_BYTES} limit",
            bytes.len()
        );
        return None;
    }
    let text = std::str::from_utf8(bytes)
        .ok()
        .filter(|t| !t.contains('\0'));
    let Some(text) = text else {
        eprintln!("Wayterm: ignored a clipboard write that is not text");
        return None;
    };
    Some(Effect::ClipboardWrite {
        destination,
        text: text.to_owned(),
    })
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
        ffi::EFFECT_CLIPBOARD_SYSTEM | ffi::EFFECT_CLIPBOARD_PRIMARY => {
            let destination = if kind == ffi::EFFECT_CLIPBOARD_SYSTEM {
                ClipboardDestination::System
            } else {
                ClipboardDestination::Selection
            };
            let Some(write) = clipboard_effect(destination, bytes) else {
                return;
            };
            write
        }
        _ => return,
    };
    if let Ok(mut effects) = effects.try_borrow_mut() {
        effects.push(effect);
    }
}

/// A PNG's size, read from its header; `None` if it does not start as one.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";
    let size = png.strip_prefix(SIGNATURE)?;
    let width = size.first_chunk::<4>()?;
    let height = size.get(4..)?.first_chunk::<4>()?;
    Some((u32::from_be_bytes(*width), u32::from_be_bytes(*height)))
}

/// Whether a payload is worth decoding. It is the remote side's, so its
/// header is checked first: what the storage could never hold is not worth
/// the memory a decode would take, and nothing but PNG reaches GDK's other
/// image loaders.
fn decodable(png: &[u8]) -> bool {
    png_size(png).is_some_and(|(width, height)| {
        let pixels = u64::from(width) * u64::from(height);
        pixels
            .checked_mul(4)
            .is_some_and(|bytes| bytes <= ffi::KITTY_STORAGE_LIMIT)
    })
}

/// Decode a PNG to `(width, height, stride, RGBA rows)`.
fn decoded_png(png: &[u8]) -> Option<(u32, u32, usize, glib::Bytes)> {
    if !decodable(png) {
        return None;
    }
    let texture = gdk::Texture::from_bytes(&glib::Bytes::from(png)).ok()?;
    let mut downloader = gdk::TextureDownloader::new(&texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    let width = u32::try_from(texture.width()).ok()?;
    let height = u32::try_from(texture.height()).ok()?;
    Some((width, height, stride, bytes))
}

/// PNG callback: runs on the thread feeding the terminal, inside
/// [`Engine::write`], and fills the buffer the shim gives it for `sink`.
unsafe extern "C" fn decode_png(data: *const u8, len: usize, sink: *mut c_void) -> bool {
    if data.is_null() {
        return false;
    }
    // SAFETY: data/len is the payload, borrowed for the call.
    let png = unsafe { std::slice::from_raw_parts(data, len) };
    let Some((width, height, stride, bytes)) = decoded_png(png) else {
        return false;
    };
    let row = width as usize * 4;
    // A panic here would abort the process, for the remote side to trigger.
    if row == 0 || stride < row || bytes.len() < stride * height.saturating_sub(1) as usize + row {
        return false;
    }
    // SAFETY: sink is the shim's for this call.
    let out = unsafe { ffi::wt_ghostty_png_pixels(sink, width, height) };
    if out.is_null() {
        return false;
    }
    // SAFETY: `out` holds `height` rows of `row` bytes, and `bytes` was
    // checked above to have them, `stride` apart.
    unsafe {
        let out = std::slice::from_raw_parts_mut(out, row * height as usize);
        for (to, from) in out.chunks_exact_mut(row).zip(bytes.chunks(stride)) {
            to.copy_from_slice(&from[..row]);
        }
    }
    true
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
            Scroll::Prompts(prompts) => (3, prompts),
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

    /// Select one command's output by its OSC 133 marks. Scrolled back,
    /// that is the first output from the viewport's top down, so a jump to
    /// a prompt selects what that command printed; at the bottom, the last
    /// output from the cursor up. `false`, the selection left alone, when
    /// no marks show one.
    pub fn select_output(&self) -> bool {
        // SAFETY: valid terminal.
        unsafe { ffi::wt_ghostty_select_output(self.ptr()) == ffi::SUCCESS }
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

    /// An image's pixels; `None` once it is deleted.
    pub fn image_pixels(&self, image_id: u32) -> Option<Pixels> {
        let mut pixels = ffi::WtPixels {
            data: std::ptr::null(),
            len: 0,
            width: 0,
            height: 0,
            format: 0,
        };
        // SAFETY: valid terminal and out-pointer.
        let result = unsafe { ffi::wt_ghostty_image_pixels(self.ptr(), image_id, &mut pixels) };
        if result != ffi::SUCCESS || pixels.data.is_null() {
            return None;
        }
        // SAFETY: data/len stay valid until the terminal changes, and
        // nothing touches it before the copy.
        let data = unsafe { std::slice::from_raw_parts(pixels.data, pixels.len) }.to_vec();
        Some(Pixels {
            width: pixels.width,
            height: pixels.height,
            format: pixels.format,
            data,
        })
    }

    /// Refresh `placements` from the last snapshot, in drawing order.
    fn read_placements(&self, placements: &mut Vec<Placement>) {
        let mut needed = 0;
        for _ in 0..2 {
            // SAFETY: valid terminal; `placements` has `len` initialized
            // slots.
            let result = unsafe {
                ffi::wt_ghostty_placements(
                    self.ptr(),
                    placements.as_mut_ptr(),
                    placements.len(),
                    &mut needed,
                )
            };
            match result {
                ffi::SUCCESS => {
                    placements.truncate(needed);
                    placements.sort_by_key(|p| (p.layer, p.z, p.image_id));
                    return;
                }
                ffi::OUT_OF_SPACE => placements.resize(needed, Placement::default()),
                _ => break,
            }
        }
        placements.clear();
    }

    /// Refresh `frame` from the terminal's viewport.
    pub fn snapshot(&self, frame: &mut Frame) -> bool {
        // The cells and then the graphemes may each need to grow once.
        for _ in 0..3 {
            let mut graphemes = 0;
            // SAFETY: valid terminal; `cells` and `graphemes` have `len`
            // initialized slots each.
            let result = unsafe {
                ffi::wt_ghostty_snapshot(
                    self.ptr(),
                    &mut frame.info,
                    frame.cells.as_mut_ptr(),
                    frame.cells.len(),
                    frame.graphemes.as_mut_ptr(),
                    frame.graphemes.len(),
                    &mut graphemes,
                )
            };
            match result {
                ffi::SUCCESS => {
                    self.read_placements(&mut frame.placements);
                    self.copy_links(frame);
                    return true;
                }
                ffi::OUT_OF_SPACE => {
                    let needed = usize::from(frame.info.cols) * usize::from(frame.info.rows);
                    if frame.cells.len() < needed {
                        frame.cells.resize(needed, Cell::default());
                    }
                    if frame.graphemes.len() < graphemes {
                        // Doubling, so a count creeping up frame by frame
                        // costs few second passes.
                        let grown = graphemes.max(frame.graphemes.len() * 2);
                        frame.graphemes.resize(grown, 0);
                    }
                }
                _ => return false,
            }
        }
        false
    }

    /// Copy the links of the snapshot just taken into `frame`.
    fn copy_links(&self, frame: &mut Frame) {
        frame.uris.clear();
        frame.link_ends.clear();
        let (mut uris, mut ends, mut count) = (std::ptr::null(), std::ptr::null(), 0);
        // SAFETY: valid terminal; the out pointers are valid.
        unsafe { ffi::wt_ghostty_links(self.ptr(), &mut uris, &mut ends, &mut count) };
        if count == 0 {
            return;
        }
        // SAFETY: with entries, the shim's buffers hold `count` ends, the
        // last of them the length of `uris`, until the next snapshot.
        unsafe {
            frame
                .link_ends
                .extend_from_slice(std::slice::from_raw_parts(ends, count));
            let len = frame.link_ends[count - 1];
            frame
                .uris
                .extend_from_slice(std::slice::from_raw_parts(uris, len));
        }
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
    use crate::terminal::color_scheme::{DARK, LIGHT};

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
        let (cell, info, span, placement, pixels) = unsafe {
            (
                ffi::wt_ghostty_cell_size(),
                ffi::wt_ghostty_frame_size(),
                ffi::wt_ghostty_span_size(),
                ffi::wt_ghostty_placement_size(),
                ffi::wt_ghostty_pixels_size(),
            )
        };
        assert_eq!(std::mem::size_of::<Cell>(), cell);
        assert_eq!(std::mem::size_of::<FrameInfo>(), info);
        assert_eq!(std::mem::size_of::<Span>(), span);
        assert_eq!(std::mem::size_of::<Placement>(), placement);
        assert_eq!(std::mem::size_of::<ffi::WtPixels>(), pixels);
        // SAFETY: plain getters.
        let (grapheme, grapheme_len) = unsafe {
            (
                ffi::wt_ghostty_cell_grapheme_offset(),
                ffi::wt_ghostty_cell_grapheme_len_offset(),
            )
        };
        assert_eq!(std::mem::offset_of!(Cell, grapheme), grapheme);
        assert_eq!(std::mem::offset_of!(Cell, grapheme_len), grapheme_len);
        // SAFETY: plain getter.
        let link = unsafe { ffi::wt_ghostty_cell_link_offset() };
        assert_eq!(std::mem::offset_of!(Cell, link), link);
        // SAFETY: plain getters.
        let (layer, format) = unsafe {
            (
                ffi::wt_ghostty_placement_layer_offset(),
                ffi::wt_ghostty_pixels_format_offset(),
            )
        };
        assert_eq!(std::mem::offset_of!(Placement, layer), layer);
        assert_eq!(std::mem::offset_of!(ffi::WtPixels, format), format);
        let (underline, underline_rgb, underline_has_rgb) = unsafe {
            (
                ffi::wt_ghostty_cell_underline_offset(),
                ffi::wt_ghostty_cell_underline_rgb_offset(),
                ffi::wt_ghostty_cell_underline_has_rgb_offset(),
            )
        };
        assert_eq!(std::mem::offset_of!(Cell, underline), underline);
        assert_eq!(std::mem::offset_of!(Cell, underline_rgb), underline_rgb);
        assert_eq!(
            std::mem::offset_of!(Cell, underline_has_rgb),
            underline_has_rgb
        );
    }

    /// A Kitty graphics command; `payload` is base64-encoded.
    fn kitty(control: &str, payload: &[u8]) -> Vec<u8> {
        format!("\x1b_G{control};{}\x1b\\", glib::base64_encode(payload)).into_bytes()
    }

    /// A terminal that knows its cell size: 8 × 16 pixels.
    fn sized_engine() -> Engine {
        let engine = engine();
        assert!(engine.resize(20, 5, 8, 16));
        engine
    }

    /// `w` × `h` opaque red RGBA pixels.
    fn red(w: usize, h: usize) -> Vec<u8> {
        [255, 0, 0, 255].repeat(w * h)
    }

    /// Show image `id`, `w` × `h` RGBA pixels, at the cursor, replying to
    /// nothing; `extra` adds to the command.
    fn show(engine: &Engine, id: u32, (w, h): (usize, usize), extra: &str) {
        let control = format!("a=T,f=32,i={id},s={w},v={h},q=2{extra}");
        assert_eq!(engine.write(&kitty(&control, &red(w, h))), []);
    }

    fn placements(engine: &Engine) -> Vec<Placement> {
        frame(engine).placements
    }

    #[test]
    fn the_storage_limit_is_the_one_png_sizes_are_checked_against() {
        let engine = engine();
        // SAFETY: valid terminal.
        let limit = unsafe { ffi::wt_ghostty_storage_limit(engine.ptr()) };
        assert_eq!(limit, ffi::KITTY_STORAGE_LIMIT);
    }

    #[test]
    fn a_placed_image_comes_out_at_its_cell() {
        let engine = sized_engine();
        engine.write(b"\x1b[2;4H");
        show(&engine, 7, (16, 32), ",X=3,Y=5");
        let [placement] = placements(&engine)[..] else {
            panic!("{:?}", placements(&engine));
        };
        assert_eq!(placement.image_id, 7);
        assert_ne!(placement.generation, 0);
        assert_eq!((placement.col, placement.row), (3, 1));
        assert_eq!((placement.offset_x, placement.offset_y), (3, 5));
        assert_eq!((placement.width, placement.height), (16, 32));
        assert_eq!((placement.source_x, placement.source_y), (0, 0));
        assert_eq!((placement.source_width, placement.source_height), (16, 32));
        assert_eq!(placement.layer, ffi::LAYER_ABOVE_TEXT);

        let pixels = engine.image_pixels(7).expect("pixels");
        assert_eq!((pixels.width, pixels.height), (16, 32));
        assert_eq!(pixels.format, ffi::FORMAT_RGBA);
        assert_eq!(pixels.data, red(16, 32));
        assert_eq!(engine.image_pixels(8), None);
    }

    #[test]
    fn a_placement_keeps_its_source_rectangle_and_size() {
        let engine = sized_engine();
        // Two by one cells of a 4 × 4 image's lower right quarter.
        show(&engine, 1, (4, 4), ",x=2,y=2,w=2,h=2,c=2,r=1");
        let [p] = placements(&engine)[..] else {
            panic!("no placement");
        };
        assert_eq!((p.source_x, p.source_y), (2, 2));
        assert_eq!((p.source_width, p.source_height), (2, 2));
        assert_eq!((p.width, p.height), (16, 16));
    }

    #[test]
    fn placements_sort_into_drawing_order_by_layer_and_z() {
        let engine = sized_engine();
        let below_bg = i32::MIN / 2 - 1;
        for (id, z) in [(1, 5), (2, 1), (3, -1), (4, below_bg), (5, -7)] {
            show(&engine, id, (1, 1), &format!(",z={z}"));
        }
        let order: Vec<(u32, u8)> = placements(&engine)
            .iter()
            .map(|p| (p.image_id, p.layer))
            .collect();
        assert_eq!(
            order,
            [
                (4, ffi::LAYER_BELOW_BG),
                (5, ffi::LAYER_BELOW_TEXT),
                (3, ffi::LAYER_BELOW_TEXT),
                (2, ffi::LAYER_ABOVE_TEXT),
                (1, ffi::LAYER_ABOVE_TEXT),
            ]
        );
    }

    #[test]
    fn a_placement_scrolls_with_the_text() {
        let engine = sized_engine();
        engine.write(b"\x1b[3;1H");
        // Three rows tall, from the third row.
        show(&engine, 1, (8, 48), ",C=1");
        let row = |engine: &Engine| placements(engine).first().map(|p| p.row);
        assert_eq!(row(&engine), Some(2));
        engine.write(b"\x1b[5;1H\r\n");
        assert_eq!(row(&engine), Some(1));
        // Partly gone: still reported, from above the viewport.
        engine.write(b"\r\n\r\n");
        assert_eq!(row(&engine), Some(-1));
        // Wholly gone, then back with the scrollback.
        engine.write(b"\r\n\r\n\r\n");
        assert_eq!(row(&engine), None);
        engine.scroll(Scroll::By(-4));
        assert_eq!(row(&engine), Some(0));
    }

    #[test]
    fn a_deleted_image_disappears() {
        let engine = sized_engine();
        show(&engine, 1, (1, 1), "");
        show(&engine, 2, (1, 1), "");
        assert_eq!(placements(&engine).len(), 2);
        engine.write(b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
        let ids: Vec<u32> = placements(&engine).iter().map(|p| p.image_id).collect();
        assert_eq!(ids, [2]);
        assert_eq!(engine.image_pixels(1), None);
    }

    #[test]
    fn a_reset_clears_images_and_leaves_graphics_working() {
        let engine = sized_engine();
        show(&engine, 1, (1, 1), "");
        engine.reset();
        assert_eq!(placements(&engine), []);
        show(&engine, 2, (1, 1), "");
        assert_eq!(placements(&engine).len(), 1);
    }

    #[test]
    fn each_screen_has_its_own_images() {
        let engine = sized_engine();
        show(&engine, 1, (1, 1), "");
        engine.write(b"\x1b[?1049h");
        assert_eq!(placements(&engine), []);
        show(&engine, 2, (1, 1), "");
        assert_eq!(placements(&engine)[0].image_id, 2);
        engine.write(b"\x1b[?1049l");
        assert_eq!(placements(&engine)[0].image_id, 1);
    }

    #[test]
    fn placeholder_cells_give_a_placement_per_row() {
        let engine = sized_engine();
        // A virtual placement of image 3, then its two cells on the second
        // row: the foreground names the image, the marks its row and column.
        let control = "a=T,U=1,f=32,i=3,s=2,v=1,c=2,r=1,q=2";
        assert_eq!(engine.write(&kitty(control, &red(2, 1))), []);
        engine.write("\r\n\x1b[38;5;3m\u{10eeee}\u{305}\u{305}".as_bytes());
        engine.write("\u{10eeee}\u{305}\u{30d}\x1b[39m".as_bytes());
        let [p] = placements(&engine)[..] else {
            panic!("{:?}", placements(&engine));
        };
        assert_eq!((p.image_id, p.col, p.row), (3, 0, 1));
        assert_eq!(p.layer, ffi::LAYER_BELOW_TEXT);
    }

    #[test]
    fn nothing_is_placed_before_the_cell_size_is_known() {
        let engine = engine();
        show(&engine, 1, (8, 16), "");
        assert_eq!(placements(&engine), []);
        assert!(engine.resize(20, 5, 8, 16));
        assert_eq!(placements(&engine).len(), 1);
    }

    #[test]
    fn a_program_can_ask_for_graphics_support() {
        let engine = engine();
        let replies = engine.write(&kitty("a=q,f=24,i=31,s=1,v=1", &[0, 0, 0]));
        let reply = b"\x1b_Gi=31;OK\x1b\\".to_vec();
        assert_eq!(replies, [Effect::Pty(reply)]);
    }

    #[test]
    fn a_remote_program_cannot_read_local_files() {
        let engine = sized_engine();
        let file = std::env::temp_dir().join("wayterm-kitty-medium-test.rgba");
        std::fs::write(&file, red(1, 1)).expect("write");
        let path = file.to_str().expect("utf-8 path").as_bytes();
        for medium in ["f", "t", "s"] {
            let control = format!("a=T,t={medium},f=32,i=9,s=1,v=1");
            let replies = engine.write(&kitty(&control, path));
            let refused = matches!(&replies[..], [Effect::Pty(reply)]
                if !reply.windows(2).any(|w| w == b"OK"));
            assert!(refused, "medium {medium}: {replies:?}");
            assert_eq!(engine.image_pixels(9), None, "medium {medium}");
        }
        assert!(file.exists(), "the temp-file medium deletes what it reads");
        std::fs::remove_file(file).expect("remove");
    }

    /// A PNG of `pixels`, RGBA rows of `width`.
    fn png(width: usize, pixels: &[u8]) -> Vec<u8> {
        let height = pixels.len() / 4 / width;
        gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from(pixels),
            width * 4,
        )
        .save_to_png_bytes()
        .to_vec()
    }

    #[test]
    fn png_payloads_decode_to_rgba() {
        let engine = sized_engine();
        // Translucent pixels stay straight alpha.
        let pixels = [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 9, 8, 7, 255];
        let control = "a=T,f=100,i=4,q=2";
        assert_eq!(engine.write(&kitty(control, &png(2, &pixels))), []);
        assert_eq!(placements(&engine).len(), 1);
        let stored = engine.image_pixels(4).expect("pixels");
        assert_eq!((stored.width, stored.height), (2, 2));
        assert_eq!(stored.format, ffi::FORMAT_RGBA);
        assert_eq!(stored.data[..4], pixels[..4]);
        assert_eq!(stored.data[7], 128);
        assert_eq!(stored.data[11], 0);
        assert_eq!(stored.data[12..], pixels[12..]);
    }

    /// A reply naming an error, and no image `id`.
    fn assert_refused(engine: &Engine, id: u32, payload: &[u8]) {
        let replies = engine.write(&kitty(&format!("a=T,f=100,i={id}"), payload));
        let refused = matches!(&replies[..], [Effect::Pty(reply)]
            if !reply.windows(2).any(|w| w == b"OK"));
        assert!(refused, "{replies:?}");
        assert_eq!(engine.image_pixels(id), None);
    }

    #[test]
    fn a_png_too_big_to_store_is_refused_before_decoding() {
        let sized = |width: u32, height: u32| {
            let mut png = png(1, &red(1, 1));
            png[16..20].copy_from_slice(&width.to_be_bytes());
            png[20..24].copy_from_slice(&height.to_be_bytes());
            png
        };
        assert_eq!(png_size(&sized(30_000, 7)), Some((30_000, 7)));
        // Exactly the limit, at four bytes a pixel, then one row more.
        assert!(decodable(&sized(8_000, 10_000)));
        assert!(!decodable(&sized(8_000, 10_001)));
        // Past what 32 bits hold, which must not wrap around.
        assert!(!decodable(&sized(u32::MAX, u32::MAX)));
        assert_refused(&sized_engine(), 5, &sized(8_000, 10_001));
    }

    #[test]
    fn only_png_is_decoded() {
        let engine = sized_engine();
        let jpeg = b"\xff\xd8\xff\xe0 not a PNG, long enough to parse";
        assert!(!decodable(jpeg));
        assert_refused(&engine, 6, jpeg);
        assert_refused(&engine, 6, &png(1, &red(1, 1))[..30]);
        assert_refused(&engine, 6, b"");
    }

    #[test]
    fn animations_advance_by_tick_and_stop() {
        let engine = sized_engine();
        // Red, then a blue frame for 40 ms; the first shows for 30 ms; play
        // on a loop.
        engine.write(&kitty("a=T,f=32,i=2,s=1,v=1,q=2", &[255, 0, 0, 255]));
        engine.write(&kitty("a=f,f=32,i=2,s=1,v=1,z=40,q=2", &[0, 0, 255, 255]));
        engine.write(b"\x1b_Ga=a,i=2,r=1,z=30,q=2\x1b\\");
        assert_eq!(engine.tick_animations(0), None, "not playing yet");
        engine.write(b"\x1b_Ga=a,i=2,s=3,q=2\x1b\\");

        let due = engine.tick_animations(0).expect("first tick");
        let first = placements(&engine)[0].generation;
        assert_eq!(engine.image_pixels(2).unwrap().data, [255, 0, 0, 255]);
        let later = engine.tick_animations(due).expect("second tick");
        assert!(later > due);
        let second = placements(&engine)[0].generation;
        assert_ne!(second, first, "a new frame is a new generation");
        assert_eq!(engine.image_pixels(2).unwrap().data, [0, 0, 255, 255]);

        engine.write(b"\x1b_Ga=a,i=2,s=1,q=2\x1b\\");
        assert_eq!(engine.tick_animations(later), None, "stopped");
    }

    /// `text` as an OSC 8 hyperlink to `uri`.
    fn hyperlink(uri: &str, text: &str) -> String {
        format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
    }

    #[test]
    fn hyperlink_cells_resolve_to_their_uri() {
        let e = engine();
        let link = hyperlink("https://example.com/a", "ab");
        e.write(format!("x{link}y").as_bytes());
        let f = frame(&e);
        assert_eq!(f.link(0, 0), None);
        assert_eq!(f.link(0, 1), Some("https://example.com/a"));
        assert_eq!(f.link(0, 2), Some("https://example.com/a"));
        assert_eq!(f.link(0, 3), None);
        assert_eq!(f.row_text(0).trim_end(), "xaby");
    }

    #[test]
    fn plain_text_that_looks_like_a_url_is_no_hyperlink() {
        let e = engine();
        e.write(b"https://example.com/a");
        let f = frame(&e);
        assert_eq!(f.cell(0, 3).map(|cell| cell.link), Some(0));
        assert_eq!(f.link(0, 3), None);
    }

    #[test]
    fn a_link_shares_one_entry_and_each_uri_keeps_its_own() {
        let e = engine();
        let a = hyperlink("https://a.example/", "aaa");
        let b = hyperlink("https://b.example/", "bbb");
        e.write(format!("{a} {b}").as_bytes());
        let f = frame(&e);
        let entry = |col| f.cell(0, col).map(|cell| cell.link);
        assert_eq!(entry(0), entry(2));
        assert_ne!(entry(0), entry(4));
        assert_eq!(f.link(0, 2), Some("https://a.example/"));
        assert_eq!(f.link(0, 4), Some("https://b.example/"));
    }

    #[test]
    fn a_wrapped_link_is_one_entry_on_both_rows() {
        let e = Engine::new(4, 3).expect("engine");
        e.write(hyperlink("https://example.com/", "abcdef").as_bytes());
        let f = frame(&e);
        assert_eq!(f.cell(0, 0).map(|c| c.link), f.cell(1, 1).map(|c| c.link));
        assert_eq!(f.link(1, 1), Some("https://example.com/"));
    }

    #[test]
    fn a_wide_character_keeps_its_link_on_both_cells() {
        let e = engine();
        e.write(hyperlink("https://example.com/", "日本").as_bytes());
        let f = frame(&e);
        for col in 0..4 {
            assert_eq!(f.link(0, col), Some("https://example.com/"), "column {col}");
        }
    }

    #[test]
    fn links_follow_the_viewport_through_scrollback() {
        let e = engine();
        e.write(format!("{}\r\n", hyperlink("https://example.com/", "top")).as_bytes());
        for line in 0..40 {
            e.write(format!("line {line}\r\n").as_bytes());
        }
        assert_eq!(frame(&e).link(0, 0), None);
        e.scroll(Scroll::Top);
        assert_eq!(frame(&e).link(0, 0), Some("https://example.com/"));
    }

    #[test]
    fn a_frame_reused_after_a_reset_holds_no_links() {
        let e = engine();
        e.write(hyperlink("https://example.com/", "ab").as_bytes());
        let mut f = frame(&e);
        assert!(f.link(0, 0).is_some());
        e.reset();
        e.write(b"ab");
        assert!(e.snapshot(&mut f));
        assert_eq!(f.link(0, 0), None);
    }

    #[test]
    fn a_screenful_of_long_distinct_links_all_resolve() {
        let e = Engine::new(20, 24).expect("engine");
        let uri = |n: usize| format!("https://example.com/{}/{n}", "x".repeat(200));
        for n in 0..23 {
            e.write(format!("{}\r\n", hyperlink(&uri(n), "link")).as_bytes());
        }
        let f = frame(&e);
        for n in 0..23 {
            assert_eq!(f.link(n, 0), Some(uri(n).as_str()), "row {n}");
        }
    }

    #[test]
    fn a_uri_that_is_not_utf8_is_no_link() {
        let e = engine();
        e.write(b"\x1b]8;;https://ex\xffample/\x1b\\ab\x1b]8;;\x1b\\");
        assert_eq!(frame(&e).link(0, 0), None);
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

    /// DECSCUSR picks the shape and the blink, mode 12 the blink alone;
    /// DECSCUSR 0 and a reset go back to a steady block.
    #[test]
    fn cursor_style_and_blink_follow_the_terminal() {
        let engine = engine();
        let cursor = || {
            let info = frame(&engine).info;
            (info.cursor_style, info.cursor_blinking != 0)
        };
        assert_eq!(cursor(), (ffi::CURSOR_BLOCK, false));
        for (decscusr, style, blinking) in [
            (1, ffi::CURSOR_BLOCK, true),
            (2, ffi::CURSOR_BLOCK, false),
            (3, ffi::CURSOR_UNDERLINE, true),
            (4, ffi::CURSOR_UNDERLINE, false),
            (5, ffi::CURSOR_BAR, true),
            (6, ffi::CURSOR_BAR, false),
        ] {
            engine.write(format!("\x1b[{decscusr} q").as_bytes());
            assert_eq!(cursor(), (style, blinking), "DECSCUSR {decscusr}");
        }
        engine.write(b"\x1b[?12h");
        assert_eq!(cursor(), (ffi::CURSOR_BAR, true));
        engine.write(b"\x1b[?12l");
        assert_eq!(cursor(), (ffi::CURSOR_BAR, false));

        engine.write(b"\x1b[5 q\x1b[0 q");
        assert_eq!(cursor(), (ffi::CURSOR_BLOCK, false));
        engine.write(b"\x1b[3 q");
        engine.reset();
        assert_eq!(cursor(), (ffi::CURSOR_BLOCK, false));
    }

    /// A cell's cluster as a string; empty for an empty cell.
    fn cluster(frame: &Frame, row: usize, col: usize) -> String {
        frame
            .grapheme(row, col)
            .iter()
            .filter_map(|&c| char::from_u32(c))
            .collect()
    }

    #[test]
    fn marks_and_selectors_stay_in_their_base_cell() {
        let engine = engine();
        // A decomposed é, ❤ with VS16, Thai ก with SARA I and Devanagari क
        // with vowel sign U, each followed by a narrow cell; then a wide
        // CJK character.
        let clusters = [
            "e\u{301}",
            "\u{2764}\u{fe0f}",
            "\u{e01}\u{e34}",
            "\u{915}\u{941}",
        ];
        let line = format!("{}\u{4e2d}|", clusters.map(|c| format!("{c}|")).concat());
        engine.write(line.as_bytes());
        let frame = frame(&engine);
        for (i, expected) in clusters.into_iter().enumerate() {
            let col = 2 * i;
            assert_eq!(cluster(&frame, 0, col), expected);
            let cell = frame.cell(0, col).unwrap();
            assert_eq!(usize::from(cell.grapheme_len), expected.chars().count());
            assert_eq!(Some(cell.codepoint), expected.chars().next().map(u32::from));
            assert_eq!(cluster(&frame, 0, col + 1), "|", "after {expected:?}");
        }
        // The wide character fills its cell; its tail holds nothing.
        assert_eq!(frame.grapheme(0, 8), [0x4e2d]);
        assert!(frame.grapheme(0, 9).is_empty());
        assert_eq!(frame.cell(0, 9).unwrap().grapheme_len, 0);
        assert_eq!(cluster(&frame, 0, 10), "|");
        assert_eq!(
            frame.row_text(0).trim_end(),
            line.replace('\u{4e2d}', "\u{4e2d} ")
        );
    }

    #[test]
    fn with_grapheme_clustering_a_wide_cluster_takes_a_cell_and_its_tail() {
        let engine = engine();
        // Mode 2027: the program asks for Unicode grapheme clustering, so
        // a ZWJ sequence, a flag and a VS16 emoji each make one wide cell.
        engine.write(b"\x1b[?2027h");
        let clusters = [
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "\u{1f1e9}\u{1f1ea}",
            "\u{2764}\u{fe0f}",
            "\u{4e2d}",
        ];
        engine.write(clusters.map(|c| format!("{c}|")).concat().as_bytes());
        let frame = frame(&engine);
        for (i, expected) in clusters.into_iter().enumerate() {
            let col = 3 * i;
            assert_eq!(cluster(&frame, 0, col), expected);
            let cell = frame.cell(0, col).unwrap();
            assert_eq!(usize::from(cell.grapheme_len), expected.chars().count());
            assert!(
                frame.grapheme(0, col + 1).is_empty(),
                "the tail of {expected:?}"
            );
            assert_eq!(cluster(&frame, 0, col + 2), "|", "after {expected:?}");
        }
    }

    #[test]
    fn the_cluster_buffer_grows_to_a_full_screen_and_is_reused() {
        let engine = engine();
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        // Each of the 20×5 cells holds two codepoints.
        engine.write("e\u{301}".repeat(100).as_bytes());
        assert!(engine.snapshot(&mut frame));
        for row in 0..5 {
            for col in 0..20 {
                assert_eq!(frame.grapheme(row, col), [0x65, 0x301], "cell {col},{row}");
            }
        }
        // The next frame reuses the buffer; plain cells do not point into it.
        engine.reset();
        engine.write("x\u{301}y".as_bytes());
        assert!(engine.snapshot(&mut frame));
        assert_eq!(frame.grapheme(0, 0), [u32::from('x'), 0x301]);
        assert_eq!(frame.grapheme(0, 1), [u32::from('y')]);
        assert_eq!(frame.row_text(1).trim_end(), "");
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
    fn sgr_5_and_6_set_blink_and_sgr_25_clears_it() {
        let engine = engine();
        engine.write(b"\x1b[5ma\x1b[25mb\x1b[6mc\x1b[25md\x1b[5;6me\x1b[0mf");
        let frame = frame(&engine);
        let blink = |col| frame.cell(0, col).unwrap().flags & ffi::CELL_BLINK != 0;
        assert_eq!(
            [0, 1, 2, 3, 4, 5].map(blink),
            [true, false, true, false, true, false]
        );
    }

    #[test]
    fn a_frame_blinks_only_while_blinking_text_shows() {
        let engine = engine();
        engine.write(b"plain");
        assert!(!frame(&engine).blinks());
        engine.write(b"\x1b[5m  \x1b[8mhidden\x1b[0m");
        assert!(!frame(&engine).blinks(), "nothing there changes");
        engine.write(b"\x1b[6mx\x1b[0m");
        assert!(frame(&engine).blinks());

        // Scrolled out of the viewport, and back.
        engine.write(&b"\r\n".repeat(10));
        assert!(!frame(&engine).blinks());
        engine.scroll(Scroll::Top);
        assert!(frame(&engine).blinks());
    }

    #[test]
    fn cells_left_past_a_shrunk_grid_do_not_blink() {
        let engine = engine();
        engine.write(b"\x1b[5;1H\x1b[5mx\x1b[0m");
        let mut frame = frame(&engine);
        assert!(frame.blinks());
        engine.reset();
        assert!(engine.resize(20, 2, 10, 20));
        assert!(engine.snapshot(&mut frame));
        assert!(!frame.blinks());
    }

    /// Each SGR underline form reaches the cell as its own style, and the
    /// forms that turn it off export none.
    #[test]
    fn sgr_underline_styles_are_exported() {
        let engine = engine();
        engine.write(
            b"\x1b[4ma\x1b[4:1mb\x1b[4:2mc\x1b[21md\x1b[4:3me\x1b[4:4mf\x1b[4:5mg\
              \x1b[4:0mh\x1b[4:3mi\x1b[24mj\x1b[4:3mk\x1b[0ml",
        );
        let frame = frame(&engine);
        let styles: Vec<u8> = (0..12)
            .map(|col| frame.cell(0, col).unwrap().underline)
            .collect();
        assert_eq!(
            styles,
            [
                ffi::UNDERLINE_SINGLE,
                ffi::UNDERLINE_SINGLE,
                ffi::UNDERLINE_DOUBLE,
                ffi::UNDERLINE_DOUBLE,
                ffi::UNDERLINE_CURLY,
                ffi::UNDERLINE_DOTTED,
                ffi::UNDERLINE_DASHED,
                ffi::UNDERLINE_NONE,
                ffi::UNDERLINE_CURLY,
                ffi::UNDERLINE_NONE,
                ffi::UNDERLINE_CURLY,
                ffi::UNDERLINE_NONE,
            ]
        );
    }

    /// SGR 58 sets the underline color in each of its forms, a palette
    /// index through the palette in effect, and SGR 59 and 0 drop it.
    #[test]
    fn sgr_underline_color_is_resolved_and_reset() {
        let engine = engine();
        engine.write(b"\x1b]4;1;rgb:12/34/56\x07");
        engine.write(
            b"\x1b[4ma\x1b[58;2;255;128;0mb\x1b[58:2::1:2:3mc\x1b[58;5;1md\x1b[58:5:1me\
              \x1b[59mf\x1b[58;2;9;9;9mg\x1b[0mh",
        );
        let frame = frame(&engine);
        let color = |col| {
            let cell = frame.cell(0, col).unwrap();
            (cell.underline_has_rgb != 0).then_some(cell.underline_rgb)
        };
        assert_eq!(color(0), None);
        assert_eq!(color(1), Some([255, 128, 0]));
        assert_eq!(color(2), Some([1, 2, 3]));
        assert_eq!(color(3), Some([0x12, 0x34, 0x56]));
        assert_eq!(color(4), Some([0x12, 0x34, 0x56]));
        assert_eq!(color(5), None, "SGR 59");
        assert_eq!(frame.cell(0, 5).unwrap().underline, ffi::UNDERLINE_SINGLE);
        assert_eq!(color(6), Some([9, 9, 9]));
        assert_eq!(color(7), None, "SGR 0");
    }

    fn rgb([r, g, b]: [u8; 3]) -> Rgb {
        Rgb(r, g, b)
    }

    /// Light and Dark replace libghostty-vt's built-in colors, recolor
    /// cells already drawn, and leave the 256-color cube and ramp xterm's.
    #[test]
    fn a_color_scheme_becomes_the_default_colors() {
        let engine = engine();
        // Default text; ANSI red on bright blue; 256-color 196 on 244.
        engine.write(b"d\x1b[31;104mr\x1b[0m\x1b[38;5;196;48;5;244mx");
        let built_in = frame(&engine);
        assert_eq!(built_in.info.cursor_has_rgb, 0);
        for scheme in [&LIGHT, &DARK] {
            assert_ne!(rgb(built_in.info.fg), scheme.foreground);
            assert_ne!(rgb(built_in.info.bg), scheme.background);
            assert_ne!(rgb(built_in.cell(0, 1).unwrap().fg), scheme.palette[1]);
            assert_ne!(rgb(built_in.cell(0, 1).unwrap().bg), scheme.palette[12]);
        }

        for scheme in [&LIGHT, &DARK] {
            assert!(engine.set_color_scheme(scheme));
            let frame = frame(&engine);
            let name = scheme.name;
            assert_eq!(rgb(frame.info.fg), scheme.foreground, "{name}");
            assert_eq!(rgb(frame.info.bg), scheme.background, "{name}");
            assert_eq!(frame.info.cursor_has_rgb, 1, "{name}");
            assert_eq!(rgb(frame.info.cursor_rgb), scheme.cursor, "{name}");
            let text = frame.cell(0, 0).unwrap();
            assert_eq!(rgb(text.fg), scheme.foreground, "{name}");
            assert_eq!(text.has_bg, 0, "{name}");
            assert_eq!(rgb(text.bg), scheme.background, "{name}");
            let ansi = frame.cell(0, 1).unwrap();
            assert_eq!(rgb(ansi.fg), scheme.palette[1], "{name}");
            assert_eq!(rgb(ansi.bg), scheme.palette[12], "{name}");
            let extended = frame.cell(0, 2).unwrap();
            assert_eq!(extended.fg, [0xff, 0x00, 0x00], "{name}: xterm's 196");
            assert_eq!(extended.bg, [0x80, 0x80, 0x80], "{name}: xterm's 244");
        }
    }

    /// Each of the 16 colors lands at its own index, by SGR 30–37 and
    /// 90–97 as by `38;5;n`.
    #[test]
    fn every_ansi_index_resolves_through_the_scheme() {
        let palette: [Rgb; 16] = std::array::from_fn(|i| Rgb(i as u8 * 16, 0x40, 0xff - i as u8));
        let scheme = ColorScheme {
            name: "Test",
            palette,
            ..LIGHT
        };
        let engine = Engine::new(40, 5).expect("engine");
        assert!(engine.set_color_scheme(&scheme));
        for index in 0..16u8 {
            let sgr = if index < 8 { 30 + index } else { 82 + index };
            engine.write(format!("\x1b[{sgr}mA\x1b[38;5;{index}mB\x1b[0m").as_bytes());
        }
        let frame = frame(&engine);
        for (index, color) in palette.into_iter().enumerate() {
            let fg = |col| rgb(frame.cell(0, col).unwrap().fg);
            assert_eq!(fg(2 * index), color, "SGR for {index}");
            assert_eq!(fg(2 * index + 1), color, "38;5;{index}");
        }
    }

    /// A reset keeps the scheme; colors a program set (a palette entry,
    /// the background) survive a scheme change.
    #[test]
    fn the_scheme_outlives_a_reset_and_yields_to_program_colors() {
        let engine = engine();
        assert!(engine.set_color_scheme(&DARK));
        engine.reset();
        engine.write(b"\x1b[31mr");
        let after_reset = frame(&engine);
        assert_eq!(rgb(after_reset.info.bg), DARK.background);
        assert_eq!(rgb(after_reset.cell(0, 0).unwrap().fg), DARK.palette[1]);

        engine.write(b"\x1b]4;1;rgb:12/34/56\x07");
        assert!(engine.set_color_scheme(&LIGHT));
        let switched = frame(&engine);
        assert_eq!(rgb(switched.info.bg), LIGHT.background);
        assert_eq!(switched.cell(0, 0).unwrap().fg, [0x12, 0x34, 0x56]);

        engine.write(b"\x1b]11;rgb:65/43/21\x07");
        assert!(engine.set_color_scheme(&DARK));
        assert_eq!(frame(&engine).info.bg, [0x65, 0x43, 0x21]);
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

    fn clipboard(destination: ClipboardDestination, text: &str) -> Effect {
        Effect::ClipboardWrite {
            destination,
            text: text.into(),
        }
    }

    /// What the terminal answered, all replies in order.
    fn replies(effects: &[Effect]) -> Vec<u8> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Pty(reply) => Some(reply.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    #[test]
    fn osc_52_writes_carry_their_destination_and_decoded_text() {
        use ClipboardDestination::{Selection, System};
        let engine = engine();
        for (sequence, destination) in [
            (&b"\x1b]52;c;aGVsbG8=\x07"[..], System),
            (b"\x1b]52;c;aGVsbG8=\x1b\\", System),
            (b"\x1b]52;;aGVsbG8=\x07", System),
            (b"\x1b]52;p;aGVsbG8=\x07", Selection),
            (b"\x1b]52;s;aGVsbG8=\x07", Selection),
        ] {
            assert_eq!(
                engine.write(sequence),
                vec![clipboard(destination, "hello")],
                "{}",
                sequence.escape_ascii()
            );
        }
    }

    #[test]
    fn an_osc_52_write_may_span_reads_and_lack_its_padding() {
        let engine = engine();
        assert!(engine.write(b"\x1b]52;c;aGVs").is_empty());
        assert_eq!(
            engine.write(b"bG8\x07"),
            vec![clipboard(ClipboardDestination::System, "hello")]
        );
    }

    #[test]
    fn utf_8_survives_an_osc_52_write() {
        // "grüße ✓"
        assert_eq!(
            engine().write(b"\x1b]52;c;Z3LDvMOfZSDinJM=\x07"),
            vec![clipboard(ClipboardDestination::System, "grüße ✓")]
        );
    }

    #[test]
    fn malformed_base64_writes_nothing_at_all() {
        let engine = engine();
        for payload in ["***", "SGVs!!!bG8=", "aGVs bG8="] {
            let sequence = format!("\x1b]52;c;{payload}\x07");
            assert!(engine.write(sequence.as_bytes()).is_empty(), "{payload:?}");
        }
    }

    #[test]
    fn a_control_byte_inside_the_sequence_never_reaches_the_decoder() {
        assert_eq!(
            engine().write(b"\x1b]52;c;aGVs\nbG8=\x07"),
            vec![clipboard(ClipboardDestination::System, "hello")]
        );
    }

    #[test]
    fn a_write_that_is_not_text_writes_nothing_at_all() {
        let engine = engine();
        // A lone 0xff, then "a\0b", which GDK would cut at the NUL.
        assert!(engine.write(b"\x1b]52;c;/w==\x07").is_empty());
        assert!(engine.write(b"\x1b]52;c;YQBi\x07").is_empty());
    }

    #[test]
    fn the_size_guard_takes_the_limit_and_refuses_past_it() {
        let system = ClipboardDestination::System;
        let at_limit = vec![b'a'; CLIPBOARD_MAX_BYTES];
        assert!(clipboard_effect(system, &at_limit).is_some());
        let over = vec![b'a'; CLIPBOARD_MAX_BYTES + 1];
        assert_eq!(clipboard_effect(system, &over), None);
    }

    #[test]
    fn a_large_write_arrives_whole_and_one_past_the_parser_drops() {
        // "QUJD" is "ABC".
        let sequence = |quads: usize| {
            let mut bytes = b"\x1b]52;c;".to_vec();
            bytes.extend(b"QUJD".repeat(quads));
            bytes.push(0x07);
            bytes
        };
        let engine = engine();
        let quads = 5 * 1024 * 1024 / 3;
        // Compared here, not by assert_eq!, which would print all of it.
        let whole = clipboard(ClipboardDestination::System, &"ABC".repeat(quads));
        assert!(engine.write(&sequence(quads)) == [whole]);
        assert!(engine.write(&sequence(2 * 1024 * 1024 + 1)).is_empty());
        // The parser recovers: the next write goes through.
        assert_eq!(
            engine.write(b"\x1b]52;c;aGVsbG8=\x07"),
            vec![clipboard(ClipboardDestination::System, "hello")]
        );
    }

    #[test]
    fn a_clipboard_query_is_never_answered() {
        let engine = engine();
        for sequence in [
            &b"\x1b]52;c;?\x07"[..],
            b"\x1b]52;p;?\x07",
            b"\x1b]52;s;?\x1b\\",
        ] {
            assert!(engine.write(sequence).is_empty());
        }
    }

    #[test]
    fn an_osc_52_clear_changes_nothing() {
        assert!(engine().write(b"\x1b]52;c;\x07").is_empty());
    }

    #[test]
    fn osc_1337_copy_shares_the_write_path() {
        assert_eq!(
            engine().write(b"\x1b]1337;Copy=:aVRlcm0y\x1b\\"),
            vec![clipboard(ClipboardDestination::System, "iTerm2")]
        );
    }

    /// `text/plain` as OSC 5522 spells a MIME type, then a transaction of
    /// the given data packets and its commit.
    fn kitty_write(engine: &Engine, packets: &[&str]) -> Vec<Effect> {
        let mut effects = engine.write(b"\x1b]5522;type=write\x1b\\");
        for packet in packets {
            let packet = format!("\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;{packet}\x1b\\");
            effects.extend(engine.write(packet.as_bytes()));
        }
        effects.extend(engine.write(b"\x1b]5522;type=wdata\x1b\\"));
        effects
    }

    #[test]
    fn kitty_clipboard_writes_stay_refused() {
        let engine = engine();
        for _ in 0..2 {
            let effects = kitty_write(&engine, &["R2hvc3R0eQ=="]);
            assert_eq!(
                String::from_utf8_lossy(&replies(&effects)),
                "\x1b]5522;type=write:status=EFBIG\x1b\\"
            );
            assert!(effects.iter().all(|e| matches!(e, Effect::Pty(_))));
            // A reset does not lift it.
            engine.reset();
        }
    }

    #[test]
    fn kitty_clipboard_writes_without_data_stay_refused() {
        let engine = engine();
        for packets in [&[][..], &[""]] {
            let effects = kitty_write(&engine, packets);
            assert_eq!(
                String::from_utf8_lossy(&replies(&effects)),
                "\x1b]5522;type=write:status=ENOSYS\x1b\\"
            );
            assert!(effects.iter().all(|e| matches!(e, Effect::Pty(_))));
        }
    }

    #[test]
    fn resize_reflows_and_reports_the_new_grid() {
        let engine = engine();
        assert!(engine.resize(40, 10, 8, 16));
        assert_eq!(engine.size(), (40, 10));
        assert_eq!(frame(&engine).cells.len(), 400);
    }

    /// `count` lines of 120 columns into a 120-column terminal limited to
    /// `limit`: how many the terminal keeps.
    fn kept_lines(limit: usize, count: usize) -> usize {
        let engine = Engine::new(120, 24).expect("engine");
        engine.set_scrollback_lines(limit);
        for line in 0..count {
            engine.write(format!("{line:0120}\r\n").as_bytes());
        }
        let text = engine.screen_text().expect("text");
        text.lines().filter(|line| !line.is_empty()).count()
    }

    #[test]
    fn the_line_limit_alone_bounds_the_scrollback() {
        assert_eq!(kept_lines(10_000, 5_000), 5_000);
        // Pruning goes by page: 387 rows at this width.
        let kept = kept_lines(1_000, 5_000);
        assert!(kept.abs_diff(1_000) < 400, "{kept}");
    }

    #[test]
    fn a_limit_of_zero_keeps_no_scrollback() {
        assert!(kept_lines(0, 5_000) <= 24, "{}", kept_lines(0, 5_000));
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

    const CELL_W: u32 = 10;
    const CELL_H: u32 = 20;
    const REPEAT_MS: u32 = 400;

    fn pixel((col, row): (u16, u16)) -> (f64, f64) {
        let w = f64::from(CELL_W);
        let h = f64::from(CELL_H);
        (f64::from(col) * w + 1.0, f64::from(row) * h + 1.0)
    }

    fn click(engine: &Engine, cell: (u16, u16), time_ms: u32) {
        let repeat_px = f64::from(CELL_W);
        assert!(engine.selection_press(cell, pixel(cell), time_ms, REPEAT_MS, repeat_px));
        engine.selection_release(cell);
    }

    fn drag_to(engine: &Engine, cell: (u16, u16)) {
        // The far side of the cell, so the drag takes it in.
        let (x, y) = pixel(cell);
        let at = (x + f64::from(CELL_W) - 2.0, y);
        assert!(engine.selection_drag(cell, at, CELL_W, 24 * CELL_H));
    }

    /// Select cells `from` through `to`, each `(col, row)`, by mouse; a
    /// new gesture each time.
    fn drag_select(engine: &Engine, from: (u16, u16), to: (u16, u16)) {
        engine.selection_cancel();
        let repeat_px = f64::from(CELL_W);
        assert!(engine.selection_press(from, pixel(from), 0, REPEAT_MS, repeat_px));
        drag_to(engine, to);
        engine.selection_release(to);
    }

    #[test]
    fn clicks_select_nothing_then_a_word_then_a_line() {
        let engine = engine();
        engine.write(b"alpha beta gamma");
        click(&engine, (7, 0), 1000);
        assert_eq!(engine.selection_text(), None);
        click(&engine, (7, 0), 1100);
        assert_eq!(engine.selection_text().as_deref(), Some("beta"));
        click(&engine, (7, 0), 1200);
        assert_eq!(engine.selection_text().as_deref(), Some("alpha beta gamma"));
        // Three is the most: a fourth click stays on the line.
        click(&engine, (7, 0), 1300);
        assert_eq!(engine.selection_text().as_deref(), Some("alpha beta gamma"));
    }

    #[test]
    fn clicks_too_slow_or_too_far_apart_start_over() {
        let engine = engine();
        engine.write(b"alpha beta gamma");
        click(&engine, (7, 0), 1000);
        click(&engine, (7, 0), 1000 + REPEAT_MS + 1);
        assert_eq!(engine.selection_text(), None, "too slow");
        click(&engine, (13, 0), 1500);
        assert_eq!(engine.selection_text(), None, "too far");
        click(&engine, (13, 0), 1600);
        assert_eq!(engine.selection_text().as_deref(), Some("gamma"));
    }

    #[test]
    fn a_word_keeps_a_path_whole() {
        let engine = engine();
        engine.write(b"see /usr/local/bin");
        click(&engine, (8, 0), 1000);
        click(&engine, (8, 0), 1100);
        assert_eq!(engine.selection_text().as_deref(), Some("/usr/local/bin"));
    }

    #[test]
    fn a_triple_click_takes_a_wrapped_line_whole() {
        let engine = Engine::new(10, 4).unwrap();
        engine.write(b"0123456789abcde\r\nnext");
        for time in [1000, 1100, 1200] {
            click(&engine, (2, 1), time);
        }
        assert_eq!(engine.selection_text().as_deref(), Some("0123456789abcde"));
    }

    #[test]
    fn a_drag_extends_by_what_the_press_selected() {
        let engine = Engine::new(80, 5).unwrap();
        engine.write(b"alpha beta gamma delta");
        click(&engine, (7, 0), 1000);
        let repeat_px = f64::from(CELL_W);
        assert!(engine.selection_press((7, 0), pixel((7, 0)), 1100, REPEAT_MS, repeat_px));
        drag_to(&engine, (12, 0));
        engine.selection_release((12, 0));
        assert_eq!(engine.selection_text().as_deref(), Some("beta gamma"));
    }

    #[test]
    fn a_drag_back_to_the_pressed_cell_selects_nothing() {
        let engine = engine();
        engine.write(b"abc def");
        drag_select(&engine, (0, 0), (2, 0));
        assert!(engine.selection_text().is_some());
        let repeat_px = f64::from(CELL_W);
        engine.selection_cancel();
        assert!(engine.selection_press((1, 0), pixel((1, 0)), 0, REPEAT_MS, repeat_px));
        drag_to(&engine, (3, 0));
        assert!(engine.selection_text().is_some());
        assert!(engine.selection_drag((1, 0), pixel((1, 0)), CELL_W, 24 * CELL_H));
        assert_eq!(engine.selection_text(), None);
    }

    #[test]
    fn cancelling_makes_the_next_press_a_first_press() {
        let engine = engine();
        engine.write(b"alpha beta");
        click(&engine, (7, 0), 1000);
        engine.selection_cancel();
        click(&engine, (7, 0), 1100);
        assert_eq!(engine.selection_text(), None);
    }

    #[test]
    fn a_gesture_survives_output_a_reset_and_a_screen_switch() {
        let engine = engine();
        engine.write(b"alpha beta");
        click(&engine, (7, 0), 1000);
        for _ in 0..100 {
            engine.write(b"more\r\n");
        }
        engine.selection_drag((1, 1), pixel((1, 1)), CELL_W, 24 * CELL_H);
        engine.write(b"\x1b[?1049h");
        engine.selection_drag((1, 1), pixel((1, 1)), CELL_W, 24 * CELL_H);
        assert_eq!(
            engine.selection_text(),
            None,
            "the press is on another screen"
        );
        engine.write(b"\x1b[?1049l");
        click(&engine, (2, 2), 2000);
        engine.reset();
        engine.selection_drag((1, 1), pixel((1, 1)), CELL_W, 24 * CELL_H);
        assert_eq!(engine.selection_text(), None);
        engine.write(b"alpha beta");
        click(&engine, (7, 0), 3000);
        click(&engine, (7, 0), 3100);
        assert_eq!(engine.selection_text().as_deref(), Some("beta"));
    }

    #[test]
    fn selection_select_all_and_clear() {
        let engine = engine();
        engine.write(b"abc def");
        assert_eq!(engine.selection_text(), None);
        drag_select(&engine, (0, 0), (2, 0));
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
            any_pressed: u32::from(
                action != ffi::MOUSE_RELEASE
                    && matches!(
                        button,
                        ffi::BUTTON_LEFT | ffi::BUTTON_MIDDLE | ffi::BUTTON_RIGHT
                    ),
            ),
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
    fn middle_and_right_buttons_report_press_drag_and_release() {
        let engine = engine();
        engine.write(b"\x1b[?1002h\x1b[?1006h");
        // Button codes 1 and 2; motion adds 32.
        for (button, press, drag, release) in [
            (
                ffi::BUTTON_MIDDLE,
                &b"\x1b[<1;4;3M"[..],
                &b"\x1b[<33;5;3M"[..],
                &b"\x1b[<1;5;3m"[..],
            ),
            (
                ffi::BUTTON_RIGHT,
                b"\x1b[<2;4;3M",
                b"\x1b[<34;5;3M",
                b"\x1b[<2;5;3m",
            ),
        ] {
            let event = mouse(ffi::MOUSE_PRESS, button, 0, (3, 2));
            assert_eq!(engine.encode_mouse(&event), press);
            let event = mouse(ffi::MOUSE_MOTION, button, 0, (4, 2));
            assert_eq!(engine.encode_mouse(&event), drag);
            let event = mouse(ffi::MOUSE_RELEASE, button, 0, (4, 2));
            assert_eq!(engine.encode_mouse(&event), release);
        }
    }

    #[test]
    fn a_held_button_reports_motion_past_the_widget() {
        let engine = engine();
        engine.write(b"\x1b[?1002h\x1b[?1006h");
        for (button, drag) in [
            (ffi::BUTTON_MIDDLE, &b"\x1b[<33;1;1M"[..]),
            (ffi::BUTTON_RIGHT, b"\x1b[<34;1;1M"),
        ] {
            let mut event = mouse(ffi::MOUSE_MOTION, button, 0, (0, 0));
            (event.x, event.y) = (-5.0, -5.0);
            assert_eq!(engine.encode_mouse(&event), drag);
            event.any_pressed = 0;
            assert_eq!(engine.encode_mouse(&event), b"", "no button held");
        }
    }

    #[test]
    fn the_horizontal_wheel_is_buttons_six_and_seven() {
        let engine = engine();
        engine.write(b"\x1b[?1000h\x1b[?1006h");
        let left = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_LEFT, 0, (0, 0));
        let right = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_RIGHT, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&left), b"\x1b[<66;1;1M");
        assert_eq!(engine.encode_mouse(&right), b"\x1b[<67;1;1M");
        // 32 + 66 and 32 + 67 in the byte format.
        engine.write(b"\x1b[?1006l");
        assert_eq!(engine.encode_mouse(&left), b"\x1b[Mb!!");
        assert_eq!(engine.encode_mouse(&right), b"\x1b[Mc!!");
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
        let middle = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_MIDDLE, 0, (3, 2));
        assert_eq!(engine.encode_mouse(&middle), b"\x1b[M!$#");
        let right = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_RIGHT, 0, (3, 2));
        assert_eq!(engine.encode_mouse(&right), b"\x1b[M\"$#");
        let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_RIGHT, 0, (3, 2));
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
        let wheel = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_LEFT, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&wheel), b"");
        let middle = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_MIDDLE, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&middle), b"\x1b[M!!!");
        let right = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_RIGHT, 0, (0, 0));
        assert_eq!(engine.encode_mouse(&right), b"\x1b[M\"!!");
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
        drag_select(&engine, (0, 0), (2, 0));
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

    /// A prompt as a shell marks it, `$ ` then the input.
    fn shell_prompt(engine: &Engine) {
        engine.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    }

    /// A finished command as a shell marks it: prompt, input, output, end.
    fn shell_command(engine: &Engine, command: &str, output: &str) {
        shell_prompt(engine);
        let text = format!("{command}\r\n\x1b]133;C\x07{output}\x1b]133;D;0\x07");
        engine.write(text.as_bytes());
    }

    /// Three commands of four lines each, idle at a fourth prompt. Five
    /// rows on screen: `three`'s prompt is the newest in the scrollback.
    fn shell_session() -> Engine {
        let engine = engine();
        for name in ["one", "two", "three"] {
            let output: String = (1..=4).map(|n| format!("{name} {n}\r\n")).collect();
            shell_command(&engine, name, &output);
        }
        shell_prompt(&engine);
        engine
    }

    fn output_of(name: &str) -> String {
        (1..=4)
            .map(|n| format!("{name} {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn top_row(engine: &Engine) -> String {
        frame(engine).row_text(0).trim_end().to_string()
    }

    #[test]
    fn prompt_scrolling_steps_between_prompts_and_clamps() {
        let engine = shell_session();
        let bottom = top_row(&engine);
        for expected in ["$ three", "$ two", "$ one", "$ one"] {
            engine.scroll(Scroll::Prompts(-1));
            assert_eq!(top_row(&engine), expected);
        }
        for expected in ["$ two", "$ three"] {
            engine.scroll(Scroll::Prompts(1));
            assert_eq!(top_row(&engine), expected);
        }
        // The idle prompt is on screen: the viewport goes back to the bottom.
        engine.scroll(Scroll::Prompts(1));
        assert_eq!(top_row(&engine), bottom);
        engine.scroll(Scroll::Prompts(1));
        assert_eq!(top_row(&engine), bottom);

        engine.scroll(Scroll::Prompts(-2));
        assert_eq!(top_row(&engine), "$ two");
        engine.scroll(Scroll::Prompts(0));
        assert_eq!(top_row(&engine), "$ two");
    }

    #[test]
    fn prompt_scrolling_stays_put_without_marks() {
        let engine = engine();
        for line in 0..20 {
            engine.write(format!("line {line}\r\n").as_bytes());
        }
        let bottom = top_row(&engine);
        engine.scroll(Scroll::Prompts(-1));
        assert_eq!(top_row(&engine), bottom);
        engine.scroll(Scroll::By(-3));
        let scrolled = top_row(&engine);
        assert_ne!(scrolled, bottom);
        engine.scroll(Scroll::Prompts(-1));
        assert_eq!(top_row(&engine), scrolled);
        engine.scroll(Scroll::Prompts(1));
        assert_eq!(top_row(&engine), scrolled);
    }

    #[test]
    fn at_the_bottom_the_last_output_is_selected() {
        let engine = shell_session();
        assert!(engine.select_output());
        assert_eq!(engine.selection_text(), Some(output_of("three")));
        // The prompts of commands that printed nothing are passed over.
        shell_command(&engine, "true", "");
        shell_prompt(&engine);
        assert!(engine.select_output());
        assert_eq!(engine.selection_text(), Some(output_of("three")));
    }

    #[test]
    fn after_a_jump_that_prompts_output_is_selected() {
        let engine = shell_session();
        engine.scroll(Scroll::Prompts(-2));
        assert_eq!(top_row(&engine), "$ two");
        assert!(engine.select_output());
        assert_eq!(engine.selection_text(), Some(output_of("two")));
    }

    #[test]
    fn scrolled_into_output_that_output_is_selected() {
        let engine = shell_session();
        engine.scroll(Scroll::Top);
        engine.scroll(Scroll::By(2));
        assert_eq!(top_row(&engine), "one 2");
        assert!(engine.select_output());
        assert_eq!(engine.selection_text(), Some(output_of("one")));
    }

    #[test]
    fn the_next_prompt_ends_output_without_a_command_end() {
        let engine = engine();
        shell_prompt(&engine);
        engine.write(b"old\r\n\x1b]133;C\x07no end\r\n");
        shell_prompt(&engine);
        assert!(engine.select_output());
        assert_eq!(engine.selection_text().as_deref(), Some("no end"));
    }

    #[test]
    fn a_running_commands_output_so_far_is_selected() {
        let engine = engine();
        shell_prompt(&engine);
        engine.write(b"run\r\n\x1b]133;C\x07so far\r\nstill");
        assert!(engine.select_output());
        assert_eq!(engine.selection_text().as_deref(), Some("so far\nstill"));
    }

    #[test]
    fn wrapped_output_is_selected_whole() {
        let engine = engine();
        let long = "x".repeat(30);
        shell_command(&engine, "long", &format!("{long}\r\n"));
        shell_prompt(&engine);
        assert!(engine.select_output());
        assert_eq!(engine.selection_text(), Some(long));
    }

    #[test]
    fn nothing_is_selected_without_marks_or_output() {
        let engine = engine();
        engine.write(b"plain\r\nbash");
        assert!(!engine.select_output());
        assert_eq!(engine.selection_text(), None);

        // An existing selection stays.
        drag_select(&engine, (0, 0), (4, 0));
        assert!(!engine.select_output());
        assert_eq!(engine.selection_text().as_deref(), Some("plain"));

        // Marks, but nothing printed yet.
        let engine = self::engine();
        shell_prompt(&engine);
        assert!(!engine.select_output());
        assert_eq!(engine.selection_text(), None);

        // The alternate screen has no marks of its own.
        let engine = shell_session();
        engine.write(b"\x1b[?1049hfull screen");
        assert!(!engine.select_output());
        assert_eq!(engine.selection_text(), None);
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
