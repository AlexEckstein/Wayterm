pub mod backend;
pub mod fake;

// Public re-exports intentionally unused at this milestone (P2-1). They are
// the module's API surface and will be consumed by VteBackend (P2-2), the
// transport pump (P2-4), and the tab UI (P2-7).
#[allow(unused_imports)]
pub use backend::{
    create_terminal, TerminalBackend, TerminalEvent, TerminalFeatures, TerminalHandle,
};
#[allow(unused_imports)]
pub use fake::FakeBackend;
