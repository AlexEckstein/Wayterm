//! Session store layer (doc 06).

pub mod db;
pub mod model;

// Task 7 will wire the store into the UI (session tree). Until then,
// the binary crate has no non-test callers of these public re-exports.
#[allow(dead_code, unused_imports)]
pub use db::SessionStore;
#[allow(dead_code, unused_imports)]
pub use model::{
    AuthMethod, Folder, ForwardKind, ForwardRule, HostKeyPolicy, RdpOptions, SessionConfig,
    SessionType, SshOptions, VncOptions,
};
