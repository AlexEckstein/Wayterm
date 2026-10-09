//! Byte-stream transports — SSH and local PTY — behind the generic
//! [`Transport`] trait (doc 04).

pub mod auth;
pub mod connection;
pub mod credentials;
pub mod hostkey;
pub mod local;
pub mod reconnect;
pub mod ssh;
pub mod supervisor;
pub mod tab;
pub mod types;
