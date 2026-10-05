pub mod backend;
pub mod fake;
pub mod vte;

// Public re-exports intentionally unused at this milestone (P2-2). They are
// the module's API surface and will be consumed by the transport pump (P2-4)
// and the tab UI (P2-7).
#[allow(unused_imports)]
pub use backend::{
    create_terminal, TerminalBackend, TerminalEvent, TerminalFeatures, TerminalHandle,
};
#[allow(unused_imports)]
pub use fake::FakeBackend;
#[allow(unused_imports)]
pub use vte::VteBackend;
