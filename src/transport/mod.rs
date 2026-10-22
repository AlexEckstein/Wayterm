//! Byte-stream transports — SSH, local PTY, and serial — behind the generic
//! [`Transport`](types::Transport) trait (doc 03).
//!
//! RDP/VNC are not byte streams (doc 08); they never appear here.

pub mod auth;
pub mod connection;
pub mod credentials;
pub mod factory;
pub mod forward;
pub mod hostkey;
pub mod jump;
pub mod local;
pub mod reconnect;
/// Serial device transport (doc 06).
pub mod serial;
/// SSH session-channel transport (doc 04).
pub mod ssh;
pub mod supervisor;
pub mod types;
