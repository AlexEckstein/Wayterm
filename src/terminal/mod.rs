//! Terminal backends behind the [`TerminalBackend`] trait (doc 02).

pub mod backend;
pub mod fake;
#[cfg(feature = "backend-ghostty")]
pub mod ghostty;
pub mod vte;

pub use backend::{
    backend_preference, create_terminal, install_backend_preference, BackendKind, TerminalBackend,
    TerminalEvent, TerminalFeatures, TerminalHandle, DEFAULT_BACKEND_SETTING, GHOSTTY_COMPILED,
};
pub use fake::FakeBackend;
#[cfg(feature = "backend-ghostty")]
pub use ghostty::GhosttyBackend;
pub use vte::VteBackend;
