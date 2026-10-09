//! The byte-stream [`Transport`] trait and its vocabulary (doc 04).

use std::io;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq)]
pub enum TransportState {
    Disconnected,
    Connecting,
    VerifyingHostKey,
    Authenticating,
    Connected,
    Reconnecting { attempt: u32 },
    Error(String),
}

impl TransportState {
    /// Best-effort `StateChanged` notification; a full channel drops it.
    pub(crate) fn emit(&self, tx: &mpsc::Sender<TransportEvent>) {
        let _ = tx.try_send(TransportEvent::StateChanged(self.clone()));
    }
}

/// Everything a transport reports. Bytes arrive only as `DataReceived`,
/// pushed by the transport's reader task — there is no `read()`.
#[derive(Clone, Debug)]
pub enum TransportEvent {
    DataReceived(Vec<u8>),
    StateChanged(TransportState),
    Error(String),
    ConnectionLost,
}

#[derive(Clone, Debug)]
#[allow(clippy::upper_case_acronyms)]
pub enum TransportType {
    SSH,
    LocalPTY,
    Serial,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),
    #[error("host key rejected: {0}")]
    HostKeyRejected(String),
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("channel error: {0}")]
    ChannelError(String),
    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),
    #[error("disconnected")]
    Disconnected,
    #[error("cancelled")]
    Cancelled,
}

impl TransportError {
    pub fn from_russh(e: russh::Error) -> Self {
        Self::ConnectionFailed(e.to_string())
    }
}

/// A byte-stream transport. It owns one channel, never the SSH session:
/// that lives in a shared [`SshConnection`](super::connection::SshConnection)
/// so the SFTP sidebar and tunnels can use it too.
#[async_trait]
pub trait Transport: Send {
    /// Open the channel, request the PTY, start the shell (or tmux), and
    /// spawn the reader task that pushes `DataReceived`.
    async fn connect(&mut self) -> Result<(), TransportError>;

    /// Stop the reader and close the channel. Idempotent.
    async fn disconnect(&mut self) -> Result<(), TransportError>;

    fn state(&self) -> TransportState;

    /// Bytes → remote. Called from the pump task, never from GTK.
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;

    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError>;

    /// tmux persistence; must be set before `connect`.
    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String);

    fn transport_type(&self) -> TransportType;

    /// E.g. `user@host:22`.
    fn description(&self) -> String;
}
