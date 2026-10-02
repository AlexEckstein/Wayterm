//! Session store layer (doc 06).
//!
//! Task 2 ships the typed data model. The SQLite layer (`db.rs`) and the
//! UI that consume these types land in the later Phase-1 tasks, so until
//! then they are referenced only by tests — which the dead-code and
//! unused-import lints flag in a binary crate.
// TODO(phase1): drop the two allows below once the store types are
// consumed by non-test code (db.rs lands in Task 3 and modifies this
// file).
#![allow(dead_code, unused_imports)]

pub mod model;

pub use model::{
    AuthMethod, Folder, ForwardKind, ForwardRule, HostKeyPolicy, RdpOptions, SessionConfig,
    SessionType, SshOptions, VncOptions,
};
