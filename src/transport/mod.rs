//! SSH transport and the generic [`Transport`] trait (doc 04).
//!
//! v1 implements only the SSH byte-stream transport; local PTY and serial
//! transports are intentionally left as structural placeholders so the
//! module tree matches the target layout.

pub mod connection;
pub mod hostkey;
pub mod ssh;
pub mod types;

#[allow(unused_imports)]
pub use connection::{AuthMethod, SshConnection};
#[allow(unused_imports)]
pub use hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};
#[allow(unused_imports)]
pub use ssh::SSHTransport;
#[allow(unused_imports)]
pub use types::{Transport, TransportError, TransportEvent, TransportState, TransportType};
