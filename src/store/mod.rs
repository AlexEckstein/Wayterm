//! Session store layer (doc 06).
//!
//! Task 2 shipped the typed data model; Task 3 adds the SQLite layer
//! (`db.rs`). The CRUD methods that translate rows into these types
//! land in Tasks 4-5, so until then the model is still referenced
//! only by tests — which the dead-code and unused-import lints flag
//! in a binary crate.
// TODO(phase1): drop the two allows below once the store types are
// consumed by non-test code (Task 5's session CRUD inserts/reads
// SessionConfig; folder CRUD alone still doesn't cover every type).
#![allow(dead_code, unused_imports)]

pub mod db;
pub mod model;

pub use db::SessionStore;
pub use model::{
    AuthMethod, Folder, ForwardKind, ForwardRule, HostKeyPolicy, RdpOptions, SessionConfig,
    SessionType, SshOptions, VncOptions,
};
