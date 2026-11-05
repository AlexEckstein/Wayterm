//! [`Frame`]: a snapshot of the viewport, and how the engine takes one.

use super::{Cell, Engine, FrameInfo, Placement};
use crate::terminal::ghostty::ffi;

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
pub(in crate::terminal::ghostty) fn push_cluster(text: &mut String, cluster: &[u32]) {
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

impl Engine {
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
