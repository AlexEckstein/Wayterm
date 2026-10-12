//! Display-free pixel buffer for the VNC engine (doc 08 §VNC, doc 04
//! §Threading model).
//!
//! `VncPixels` is plain `Vec<u32>` so it is `Send` and lives on the engine
//! side; the GTK side copies a snapshot into a cairo surface in its draw
//! handler. Pixels are BGRA32 packed little-endian (`a << 24 | r << 16 |
//! g << 8 | b`), which is exactly what `u32::from_le_bytes` yields for the
//! `[b, g, r, a]` network-order bytes vnc-rs 0.6.0 delivers under
//! `PixelFormat::bgra()`. The alpha byte on the wire is padding, so every
//! write forces it opaque; the buffer can be handed to cairo as ARGB32 or
//! RGB24 without a transparency surprise from a server that sends `0`.

use thiserror::Error;

/// `(x, y, width, height)` in framebuffer pixels.
pub type PixelRect = (u16, u16, u16, u16);

/// Opaque black: the fill for fresh and newly exposed pixels.
const OPAQUE_BLACK: u32 = 0xFF00_0000;
const ALPHA_OPAQUE: u32 = 0xFF00_0000;

/// The widest or tallest desktop the engine holds. Cairo image surfaces
/// stop at 32767; this still covers multi-monitor 8K walls.
pub const MAX_DIMENSION: u16 = 16_384;
/// The most pixels the engine holds (64 Mi — 256 MiB of `u32`, dual 8K
/// fits). The server picks the size and every pixel is filled up front,
/// so an unchecked 65535 × 65535 would commit ~17 GB before a frame.
pub const MAX_PIXELS: usize = 1 << 26;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FramebufferError {
    /// The delivered byte count does not equal `width * height * 4` for the
    /// rect, so its row stride is unknown. Nothing is written.
    #[error("rect data is {got} bytes, expected {expected}")]
    RectLength { expected: usize, got: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VncPixels {
    width: u16,
    height: u16,
    pixels: Vec<u32>,
}

impl VncPixels {
    /// A `width` x `height` buffer filled with opaque black. 0x0 (or any
    /// zero dimension) is valid and holds no pixels.
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: vec![OPAQUE_BLACK; len(width, height)],
        }
    }

    pub fn dims(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Whether a `width` x `height` desktop is within [`MAX_DIMENSION`]
    /// and [`MAX_PIXELS`]. The engine checks a server's size with this
    /// before [`resize`](Self::resize) allocates it.
    pub fn fits(width: u16, height: u16) -> bool {
        width <= MAX_DIMENSION && height <= MAX_DIMENSION && len(width, height) <= MAX_PIXELS
    }

    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// Replace `out` with the current pixels (row-major, `width * height`).
    /// Reuses `out`'s allocation; the UI keeps one scratch vec per tab.
    pub fn write_snapshot(&self, out: &mut Vec<u32>) {
        out.clear();
        out.extend_from_slice(&self.pixels);
    }

    /// Change the dimensions keeping the pixels in the overlap of the old and
    /// new areas anchored at the top-left (a resize does not shift content).
    /// Newly exposed pixels are opaque black; cropped pixels are dropped.
    pub fn resize(&mut self, width: u16, height: u16) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        let mut next = vec![OPAQUE_BLACK; len(width, height)];
        let keep_w = usize::from(self.width.min(width));
        let keep_h = usize::from(self.height.min(height));
        for row in 0..keep_h {
            let src = row * usize::from(self.width);
            let dst = row * usize::from(width);
            next[dst..dst + keep_w].copy_from_slice(&self.pixels[src..src + keep_w]);
        }
        self.width = width;
        self.height = height;
        self.pixels = next;
    }

    /// Write BGRA byte rows into `rect`. `bytes` must be exactly
    /// `w * h * 4` long (rows are `w * 4` apart); otherwise nothing is
    /// written and an error is returned. Parts of the rect outside the
    /// buffer are dropped, so a server that lies about geometry can neither
    /// panic the engine nor touch pixels outside the buffer.
    pub fn write_rect(&mut self, rect: PixelRect, bytes: &[u8]) -> Result<(), FramebufferError> {
        let (x, y, w, h) = (
            usize::from(rect.0),
            usize::from(rect.1),
            usize::from(rect.2),
            usize::from(rect.3),
        );
        let expected = w * h * 4;
        if bytes.len() != expected {
            return Err(FramebufferError::RectLength {
                expected,
                got: bytes.len(),
            });
        }
        let stride = usize::from(self.width);
        let visible_w = w.min(stride.saturating_sub(x));
        let visible_h = h.min(usize::from(self.height).saturating_sub(y));
        for row in 0..visible_h {
            let src = &bytes[row * w * 4..(row * w + visible_w) * 4];
            let (src, _) = src.as_chunks::<4>();
            let dst = (y + row) * stride + x;
            for (px, bgra) in self.pixels[dst..dst + visible_w].iter_mut().zip(src) {
                *px = u32::from_le_bytes(*bgra) | ALPHA_OPAQUE;
            }
        }
        Ok(())
    }

    /// RFB CopyRect: copy the block at `src` to the position of `dst`. The
    /// block is the common size of the two rects, clamped so both the source
    /// and the destination stay in bounds. Overlapping blocks do not smear:
    /// rows are visited in the direction that reads each source row before
    /// it is overwritten, and each row moves with a memmove.
    pub fn copy(&mut self, dst: PixelRect, src: PixelRect) {
        let stride = usize::from(self.width);
        let (dx, dy) = (usize::from(dst.0), usize::from(dst.1));
        let (sx, sy) = (usize::from(src.0), usize::from(src.1));
        let w = usize::from(dst.2.min(src.2))
            .min(stride.saturating_sub(dx))
            .min(stride.saturating_sub(sx));
        let h = usize::from(dst.3.min(src.3))
            .min(usize::from(self.height).saturating_sub(dy))
            .min(usize::from(self.height).saturating_sub(sy));
        let mut move_row = |row: usize| {
            let from = (sy + row) * stride + sx;
            let to = (dy + row) * stride + dx;
            self.pixels.copy_within(from..from + w, to);
        };
        if dy <= sy {
            (0..h).for_each(&mut move_row);
        } else {
            (0..h).rev().for_each(&mut move_row);
        }
    }
}

fn len(width: u16, height: u16) -> usize {
    usize::from(width) * usize::from(height)
}

#[cfg(test)]
mod tests;
