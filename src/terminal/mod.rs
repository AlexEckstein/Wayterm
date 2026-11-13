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
    BackendKind, DEFAULT_BACKEND_SETTING, GHOSTTY_COMPILED, TRANSLUCENT_CLASS, TerminalBackend,
    TerminalEvent, TerminalFeatures, TerminalHandle, backend_preference, create_terminal,
    create_terminal_for, ghostty_version, install_backend_preference,
};
pub use color_scheme::{
    ColorScheme, DEFAULT_COLOR_SCHEME_SETTING, GHOSTTY_COLOR_SCHEME_SETTING, SchemeChoice,
    VTE_COLOR_SCHEME_SETTING,
};
pub use defaults::TerminalDefaults;
pub use fake::FakeBackend;
#[cfg(feature = "backend-ghostty")]
pub use ghostty::GhosttyBackend;
pub use vte::{VteBackend, vte_version};
