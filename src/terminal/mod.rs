//! Terminal backends behind the [`TerminalBackend`] trait (doc 02): VTE,
//! the opt-in Ghostty engine, and an in-memory fake for tests.

pub mod backend;
pub mod color_scheme;
pub mod fake;
#[cfg(feature = "backend-ghostty")]
pub mod ghostty;
pub mod vte;

pub use backend::{
    backend_preference, create_terminal, install_backend_preference, BackendKind, TerminalBackend,
    TerminalEvent, TerminalFeatures, TerminalHandle, DEFAULT_BACKEND_SETTING, GHOSTTY_COMPILED,
};
pub use color_scheme::{ColorScheme, SchemeChoice, DEFAULT_COLOR_SCHEME_SETTING};
pub use fake::FakeBackend;
#[cfg(feature = "backend-ghostty")]
pub use ghostty::GhosttyBackend;
pub use vte::VteBackend;
