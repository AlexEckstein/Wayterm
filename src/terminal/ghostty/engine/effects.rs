//! What the terminal asks of the embedder while it parses.

use std::cell::RefCell;
use std::ffi::{c_int, c_void};

use crate::terminal::ghostty::ffi;

/// What the terminal asked for while processing bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A reply for the remote side (device attributes, status reports).
    Pty(Vec<u8>),
    Bell,
    /// The directory the shell reported, raw: a `file://` URI for OSC 7.
    Pwd(String),
    /// A desktop notification (OSC 9, OSC 777); OSC 9 has no title, so
    /// `title` is empty.
    Notification {
        title: String,
        body: String,
    },
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
pub(super) const CLIPBOARD_MAX_BYTES: usize = 8 * 1024 * 1024;

/// A program's clipboard write, whole or not at all: one too large, not
/// UTF-8, or holding a NUL, which would cut GDK's text short, is dropped
/// with a note that leaves its contents out.
pub(super) fn clipboard_effect(destination: ClipboardDestination, bytes: &[u8]) -> Option<Effect> {
    if bytes.len() > CLIPBOARD_MAX_BYTES {
        tracing::warn!(
            bytes = bytes.len(),
            limit = CLIPBOARD_MAX_BYTES,
            "ignored a clipboard write over the size limit"
        );
        return None;
    }
    let text = std::str::from_utf8(bytes)
        .ok()
        .filter(|t| !t.contains('\0'));
    let Some(text) = text else {
        tracing::warn!("ignored a clipboard write that is not text");
        return None;
    };
    Some(Effect::ClipboardWrite {
        destination,
        text: text.to_owned(),
    })
}

/// Effect callback: only records, so it never re-enters the terminal.
pub(super) unsafe extern "C" fn on_effect(
    userdata: *mut c_void,
    kind: c_int,
    data: *const u8,
    len: usize,
) {
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
