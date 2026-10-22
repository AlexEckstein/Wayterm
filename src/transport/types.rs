//! The byte-stream [`Transport`] trait and its vocabulary (doc 03).

use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

/// Where a transport is in its lifecycle (doc 03 §Lifecycle).
#[derive(Clone, Debug, PartialEq)]
pub enum TransportState {
    /// Not connected; the start and end state.
    Disconnected,
    /// Dialing.
    Connecting,
    /// Checking the server's host key.
    VerifyingHostKey,
    /// Running the auth chain.
    Authenticating,
    /// Streaming.
    Connected,
    /// The reconnect driver is on attempt `attempt`.
    Reconnecting { attempt: u32 },
    /// Failed; the message is shown on the tab.
    Error(String),
}

/// A transport's state, shared with its reader task (which may end the
/// connection on its own) and announced as `StateChanged` on each change.
/// Clones share the state.
#[derive(Clone)]
pub(crate) struct SharedState {
    /// The state itself.
    current: Arc<Mutex<TransportState>>,
    /// Where changes are announced.
    events: mpsc::Sender<TransportEvent>,
}

impl SharedState {
    /// `Disconnected`, announcing changes on `events`.
    pub(crate) fn new(events: mpsc::Sender<TransportEvent>) -> Self {
        Self {
            current: Arc::new(Mutex::new(TransportState::Disconnected)),
            events,
        }
    }

    /// The current state.
    pub(crate) fn get(&self) -> TransportState {
        self.lock().clone()
    }

    /// Record `state` and announce it, unless nothing changed. The
    /// announcement is best effort: a full channel drops it.
    pub(crate) fn set(&self, state: TransportState) {
        let mut current = self.lock();
        if *current != state {
            *current = state;
            let _ = self
                .events
                .try_send(TransportEvent::StateChanged(current.clone()));
        }
    }

    /// Lock, ignoring poison.
    fn lock(&self) -> std::sync::MutexGuard<'_, TransportState> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Everything a transport reports. Bytes arrive only as `DataReceived`,
/// pushed by the transport's reader task — there is no `read()`.
#[derive(Clone, Debug)]
pub enum TransportEvent {
    /// Output bytes for the terminal.
    DataReceived(Vec<u8>),
    /// The state changed.
    StateChanged(TransportState),
    /// A failure to show; the state follows separately.
    Error(String),
    /// The link dropped under a live session (reconnect per policy).
    ConnectionLost,
    /// The shell exited on its own (`exit`, logout): there is nothing to
    /// reconnect to automatically.
    SessionEnded,
}

/// Which kind of byte stream a transport is.
#[derive(Clone, Debug)]
#[allow(clippy::upper_case_acronyms)]
pub enum TransportType {
    /// A session channel on an SSH connection.
    SSH,
    /// A local shell on a PTY.
    LocalPTY,
    /// A serial device.
    Serial,
}

/// Why a transport call failed.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Could not reach or dial the host.
    #[error("connection failed: {0}")]
    ConnectionFailed(String),
    /// The host key was refused; never retried as an auth failure.
    #[error("host key rejected: {0}")]
    HostKeyRejected(String),
    /// Every auth method failed.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// Opening or using a channel failed.
    #[error("channel error: {0}")]
    ChannelError(String),
    /// Local I/O failed.
    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),
    /// The transport is not connected.
    #[error("disconnected")]
    Disconnected,
    /// The user dismissed a prompt or closed the tab; never retried.
    #[error("cancelled")]
    Cancelled,
}

impl TransportError {
    /// Wrap a russh error as a connect failure.
    pub fn from_russh(e: russh::Error) -> Self {
        Self::ConnectionFailed(e.to_string())
    }
}

/// A byte-stream transport. It owns one channel, never the SSH session:
/// that lives in a shared [`SshConnection`](super::connection::SshConnection)
/// so the SFTP sidebar and port forwards can use it too.
#[async_trait]
pub trait Transport: Send {
    /// Open the channel, request the PTY, start the shell (or tmux), and
    /// spawn the reader task that pushes `DataReceived`.
    async fn connect(&mut self) -> Result<(), TransportError>;

    /// Stop the reader and close the channel. Idempotent.
    async fn disconnect(&mut self) -> Result<(), TransportError>;

    /// The current state.
    fn state(&self) -> TransportState;

    /// Bytes → remote. Called from the pump task, never from GTK.
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;

    /// Tell the remote side the grid size.
    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError>;

    /// tmux persistence; must be set before `connect`.
    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String);

    /// SSH, local PTY or serial.
    fn transport_type(&self) -> TransportType;

    /// E.g. `user@host:22`.
    fn description(&self) -> String;
}
