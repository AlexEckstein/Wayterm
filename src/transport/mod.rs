//! SSH and serial transports and the generic [`Transport`] trait (doc 04).
//!
//! The local PTY transport is still a Phase 5 placeholder in the factory.

pub mod auth;
pub mod connection;
pub mod credentials;
pub mod hostkey;
pub mod reconnect;
pub mod serial;
pub mod ssh;
pub mod supervisor;
pub mod tab;
pub mod types;
