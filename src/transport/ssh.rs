//! [`SSHTransport`]: a PTY session channel on a shared [`SshConnection`],
//! running the login shell or tmux (doc 04 §SSHTransport).

use std::sync::Arc;

use async_trait::async_trait;
use russh::{client::Msg, Channel, ChannelMsg, ChannelWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::store::model::TMUX_NAME_PREFIX;
use crate::transport::connection::SshConnection;
use crate::transport::types::{
    SharedState, Transport, TransportError, TransportEvent, TransportState, TransportType,
};

/// SSH byte-stream transport: one PTY channel on a shared, supervisor-owned
/// [`SshConnection`] (doc 04). Disconnecting closes only this channel.
pub struct SSHTransport {
    /// The shared connection the channel is opened on.
    connection: Arc<SshConnection>,
    /// The channel's write half, while connected.
    channel_write: Option<ChannelWriteHalf<Msg>>,
    /// Owns the read half and pushes events.
    reader_task: Option<JoinHandle<()>>,
    /// Where the reader sends events.
    event_tx: mpsc::Sender<TransportEvent>,
    /// Stops the reader; fresh per connect.
    cancel_token: CancellationToken,
    /// The terminal's measured grid at connect time.
    initial_rows: u32,
    /// Columns of the measured grid at connect time.
    initial_cols: u32,
    /// State shared with the reader.
    state: SharedState,
    /// The tmux session's bare name while persistence is on (doc 04).
    tmux_session: Option<String>,
}

impl SSHTransport {
    /// A disconnected transport for `connection`, opening at `rows` × `cols`.
    pub fn new(
        connection: Arc<SshConnection>,
        event_tx: mpsc::Sender<TransportEvent>,
        initial_rows: u32,
        initial_cols: u32,
    ) -> Self {
        Self {
            connection,
            channel_write: None,
            reader_task: None,
            state: SharedState::new(event_tx.clone()),
            event_tx,
            cancel_token: CancellationToken::new(),
            initial_rows,
            initial_cols,
            tmux_session: None,
        }
    }

    /// A session channel with its PTY, running the shell (or tmux).
    async fn open_terminal(&self) -> Result<Channel<Msg>, TransportError> {
        let channel = self.connection.open_session_channel().await?;
        channel
            .request_pty(
                false,
                "xterm-256color",
                self.initial_cols,
                self.initial_rows,
                0,
                0,
                &[],
            )
            .await
            .map_err(TransportError::from_russh)?;
        let started = match &self.tmux_session {
            Some(suffix) => {
                // `-A` attaches to the session if it already exists.
                let name = format!("{TMUX_NAME_PREFIX}{suffix}");
                let command = format!("tmux new -A -s {}", shell_quote(&name));
                channel.exec(false, command.as_bytes()).await
            }
            None => channel.request_shell(false).await,
        };
        started.map_err(TransportError::from_russh)?;
        Ok(channel)
    }
}

/// Quote one argument for the remote `/bin/sh -c` (tmux session names may
/// contain `;` and other metacharacters).
pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "_-./@+".contains(c))
    {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[async_trait]
impl Transport for SSHTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        if self.state.get() == TransportState::Connected {
            return Err(TransportError::ChannelError("already connected".into()));
        }
        self.state.set(TransportState::Connecting);
        let channel = match self.open_terminal().await {
            Ok(channel) => channel,
            Err(err) => {
                self.state.set(TransportState::Disconnected);
                return Err(err);
            }
        };

        // The reader task owns the read half: the only path bytes take in.
        let (mut read_half, write_half) = channel.split();
        self.channel_write = Some(write_half);
        // Connected before the reader starts, so an end it reports at once
        // is never overwritten.
        self.state.set(TransportState::Connected);
        let (event_tx, state) = (self.event_tx.clone(), self.state.clone());
        // A fresh token per connect: a reconnect must not inherit a cancel.
        self.cancel_token = CancellationToken::new();
        let cancel = self.cancel_token.clone();
        self.reader_task = Some(tokio::spawn(async move {
            // An exit status (or signal) means the remote process ended, so
            // the close that follows is the session ending, not a drop.
            let mut exited = false;
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = read_half.wait() => msg,
                };
                match msg {
                    // v1 shows stderr inline with stdout.
                    Some(ChannelMsg::Data { data })
                    | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let event = TransportEvent::DataReceived(data.to_vec());
                        if event_tx.send(event).await.is_err() {
                            break; // tab closed
                        }
                    }
                    Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::ExitSignal { .. }) => {
                        exited = true;
                    }
                    // OpenSSH sends EOF *before* the exit status, so EOF
                    // alone ends nothing: wait for the close.
                    Some(ChannelMsg::Close) | None => {
                        // Recorded before it is reported, so `state()`
                        // agrees with the event.
                        state.set(TransportState::Disconnected);
                        let event = if exited {
                            TransportEvent::SessionEnded
                        } else {
                            TransportEvent::ConnectionLost
                        };
                        let _ = event_tx.send(event).await;
                        break;
                    }
                    Some(_) => {}
                }
            }
        }));
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.cancel_token.cancel();
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        // Dropping the write half closes our side; the shared connection
        // stays up for other consumers.
        self.channel_write = None;
        self.state.set(TransportState::Disconnected);
        Ok(())
    }

    fn state(&self) -> TransportState {
        self.state.get()
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let channel = self
            .channel_write
            .as_mut()
            .ok_or(TransportError::Disconnected)?;
        channel
            .data_bytes(data.to_vec())
            .await
            .map_err(TransportError::from_russh)
    }

    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError> {
        let channel = self
            .channel_write
            .as_mut()
            .ok_or(TransportError::Disconnected)?;
        channel
            .window_change(cols, rows, 0, 0)
            .await
            .map_err(TransportError::from_russh)
    }

    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String) {
        self.tmux_session = enabled.then_some(session_name);
    }

    fn transport_type(&self) -> TransportType {
        TransportType::SSH
    }

    fn description(&self) -> String {
        let c = &self.connection;
        format!("{}@{}:{}", c.username(), c.host(), c.port())
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_escapes_metacharacters() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("wayterm-prod_1"), "wayterm-prod_1");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
