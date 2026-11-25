//! Byte-stream transports — SSH, local PTY, and serial — behind the generic
//! [`Transport`](types::Transport) trait (doc 03).
//!
//! RDP/VNC are not byte streams (doc 08); they never appear here.

pub mod auth;
pub mod bandwidth;
pub mod connection;
pub mod credentials;
pub mod disks;
pub mod factory;
pub mod forward;
pub mod host_stats;
pub mod hostkey;
pub mod jump;
pub mod load;
pub mod local;
pub mod reconnect;
pub(crate) mod remote_script;
pub mod serial;
pub mod ssh;
pub mod supervisor;
pub mod traffic;
pub mod types;
