//! Terminal backends behind the [`TerminalBackend`] trait (doc 02): VTE,
//! the opt-in Ghostty engine, and an in-memory fake for tests.

pub mod backend;
pub mod color_scheme;
pub mod defaults;
pub mod fake;
#[cfg(feature = "backend-ghostty")]
pub mod ghostty;
pub mod options;
pub mod vte;

pub use backend::{
    backend_preference, create_terminal, create_terminal_for, ghostty_version,
    install_backend_preference, BackendKind, TerminalBackend, TerminalEvent, TerminalFeatures,
    TerminalHandle, DEFAULT_BACKEND_SETTING, GHOSTTY_COMPILED, TRANSLUCENT_CLASS,
};
pub use color_scheme::{
    ColorScheme, SchemeChoice, DEFAULT_COLOR_SCHEME_SETTING, GHOSTTY_COLOR_SCHEME_SETTING,
    VTE_COLOR_SCHEME_SETTING,
};
pub use defaults::TerminalDefaults;
pub use fake::FakeBackend;
#[cfg(feature = "backend-ghostty")]
pub use ghostty::GhosttyBackend;
pub use vte::{vte_version, VteBackend};
