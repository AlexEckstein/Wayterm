use async_trait::async_trait;
use std::io;
use thiserror::Error;
use tokio::sync::mpsc;

/// Current lifecycle state of a byte-stream transport.
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

/// Events pushed by a transport — the only way data leaves a transport.
///
/// There is deliberately no `read()` method on [`Transport`]; bytes flow
/// out through `DataReceived`.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum TransportEvent {
    /// Pushed by the transport's reader task — the ONLY way bytes arrive
    /// from the remote side. (There is no `read()` method.)
    DataReceived(Vec<u8>),
    StateChanged(TransportState),
    Error(String),
    ConnectionLost,
}

/// Transport family.
#[derive(Clone, Debug)]
#[allow(clippy::upper_case_acronyms)]
pub enum TransportType {
    SSH,
    LocalPTY,
    Serial,
}

/// Errors from transport operations.
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
    /// Convenience constructor for the common case of wrapping a russh error.
    pub fn from_russh(e: russh::Error) -> Self {
        TransportError::ConnectionFailed(e.to_string())
    }
}

/// The transport owns ONE channel (session shell/exec/PTY), never the
/// underlying SSH session. The session lives in [`super::connection::SshConnection`]
/// so the SFTP sidebar (doc 09) and future tunnels can share it.
#[async_trait]
pub trait Transport: Send {
    /// Connect: establish session (if needed), open channel, request PTY,
    /// start shell (or tmux), and SPAWN the reader task that pushes
    /// `DataReceived` events.
    async fn connect(&mut self) -> Result<(), TransportError>;

    /// Disconnect and clean up. Cancels the reader task. Idempotent.
    async fn disconnect(&mut self) -> Result<(), TransportError>;

    fn state(&self) -> TransportState;

    /// Bytes → remote (called by the pump task on tokio, never from GTK).
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;

    /// Notify remote PTY of terminal size change.
    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError>;

    /// Enable/disable tmux session persistence (must be set pre-connect).
    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String);

    fn transport_type(&self) -> TransportType;

    /// Human-readable description (e.g. "user@host:22").
    fn description(&self) -> String;
}

impl TransportState {
    /// Helper used by both transport implementations and tests.
    pub(crate) fn emit(&self, tx: &mpsc::Sender<TransportEvent>) {
        let _ = tx.try_send(TransportEvent::StateChanged(self.clone()));
    }
}

#[allow(
    dead_code,
    clippy::let_underscore_future,
    clippy::used_underscore_binding
)]
async fn _suppress_dead_code_until_wired(tx: mpsc::Sender<TransportEvent>, mut t: impl Transport) {
    let _ = (
        TransportState::Disconnected,
        TransportState::Connecting,
        TransportState::Authenticating,
        TransportState::Connected,
        TransportState::Reconnecting { attempt: 0 },
        TransportState::Error(String::new()),
        TransportEvent::DataReceived(Vec::new()),
        TransportEvent::StateChanged(TransportState::Disconnected),
        TransportEvent::Error(String::new()),
        TransportEvent::ConnectionLost,
        TransportType::SSH,
        TransportType::LocalPTY,
        TransportType::Serial,
        TransportError::HostKeyRejected(String::new()),
        TransportError::AuthFailed(String::new()),
        TransportError::ChannelError(String::new()),
        TransportError::Disconnected,
        TransportError::Cancelled,
    );
    TransportState::Disconnected.emit(&tx);
    let _ = t.connect().await;
    let _ = t.disconnect().await;
    let _ = t.state();
    let _ = t.write(&[]).await;
    let _ = t.resize(0, 0).await;
    t.set_tmux_enabled(false, String::new());
    let _ = t.transport_type();
    let _ = t.description();
}

#[allow(dead_code)]
fn _suppress_transport_constructors() {
    use crate::transport::hostkey::HostKeyVerifier;
    use crate::transport::SSHTransport;
    let _ = SSHTransport::new;
    let _ = HostKeyVerifier::new;
}

#[allow(dead_code)]
fn _suppress_connection_methods(_c: &crate::transport::SshConnection) {
    use crate::transport::connection::authenticate;
    let _ = authenticate;
}
