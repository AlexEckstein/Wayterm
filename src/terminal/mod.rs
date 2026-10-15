//! Terminal backends behind the [`TerminalBackend`] trait (doc 02).

pub mod backend;
pub mod fake;
#[cfg(feature = "backend-ghostty")]
pub mod ghostty;
pub mod vte;

pub use backend::{
    create_terminal, TerminalBackend, TerminalEvent, TerminalFeatures, TerminalHandle,
};
pub use fake::FakeBackend;
pub use vte::VteBackend;
