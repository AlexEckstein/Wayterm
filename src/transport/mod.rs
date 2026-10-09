//! SSH transport and the generic [`Transport`] trait (doc 04).
//!
//! v1 implements only the SSH byte-stream transport; local PTY and serial
//! transports are intentionally left as structural placeholders so the
//! module tree matches the target layout.

pub mod auth;
pub mod connection;
pub mod credentials;
pub mod hostkey;
pub mod reconnect;
pub mod ssh;
pub mod supervisor;
pub mod tab;
pub mod types;
