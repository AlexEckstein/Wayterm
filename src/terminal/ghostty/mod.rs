//! The experimental libghostty-vt backend (doc 02 §Engine backends), built
//! only with the `backend-ghostty` feature.
//!
//! - [`ffi`] declares `shim.c`, a narrow C layer over the pinned
//!   libghostty-vt C API (docs/plans/phase10-pin.md).
//! - [`engine::Engine`] owns one terminal safely and needs no display.

pub mod engine;
pub mod ffi;
