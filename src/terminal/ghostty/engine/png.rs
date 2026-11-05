//! Decoding the PNG payloads of Kitty images.

use std::ffi::c_void;

use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::terminal::ghostty::ffi;

/// A PNG's size, read from its header; `None` if it does not start as one.
pub(super) fn png_size(png: &[u8]) -> Option<(u32, u32)> {
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
pub(super) fn decodable(png: &[u8]) -> bool {
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
pub(super) unsafe extern "C" fn decode_png(data: *const u8, len: usize, sink: *mut c_void) -> bool {
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
