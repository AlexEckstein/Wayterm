//! Session store layer (doc 06).

// Task 5 consumes SessionConfig and all option structs, but the
// binary crate still has no non-test callers of the public re-exports
// below. Keep a narrowly-scoped allow rather than the previous blanket
// suppression so the lint fires once a future task actually wires the
// store into the UI (doc 01/07/09).
#![allow(dead_code, unused_imports)]

pub mod db;
pub mod model;

pub use db::SessionStore;
pub use model::{
    AuthMethod, Folder, ForwardKind, ForwardRule, HostKeyPolicy, RdpOptions, SessionConfig,
    SessionType, SshOptions, VncOptions,
};
