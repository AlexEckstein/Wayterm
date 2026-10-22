//! VNC tabs (doc 08 §VNC, Path B): a pure-Rust RFB client on tokio.
//!
//! VNC is deliberately not a `Transport` (doc 03 §Transport factory): the
//! engine here hands the UI a framebuffer and an event stream instead of a
//! byte stream. Display-free; the GTK tab lives in `ui/`.

pub mod client;
pub mod framebuffer;

pub use ::vnc::{ClientKeyEvent, ClientMouseEvent, X11Event};
pub use client::{VncEngine, VncError, VncInput, VncSession, VncUiEvent};
pub use framebuffer::{FramebufferError, PixelRect, VncPixels};
