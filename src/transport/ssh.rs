use std::sync::Arc;

use async_trait::async_trait;
use russh::{client::Msg, ChannelMsg, ChannelWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::transport::connection::SshConnection;
use crate::transport::types::{
    Transport, TransportError, TransportEvent, TransportState, TransportType,
};

/// SSH byte-stream transport: one channel on a shared, supervisor-owned
/// [`SshConnection`].
///
/// The session is NOT owned here; `disconnect()` closes only this channel and
/// cancels the reader task. The supervisor drops the session when the last `Arc`
/// is released.
pub struct SSHTransport {
    // Shared, supervisor-owned — NOT owned here (see doc 04).
    connection: Arc<SshConnection>,

    // Write half of our channel on that connection.
    channel_write: Option<ChannelWriteHalf<Msg>>,

    // Reader task JoinHandle — spawned in connect(), aborted in disconnect().
    reader_task: Option<JoinHandle<()>>,

    // Where reader task pushes DataReceived chunks (bound to the GTK side).
    event_tx: mpsc::Sender<TransportEvent>,

    // Cancellation token shared with the reader task.
    cancel_token: CancellationToken,

    // Initial PTY size, taken from the terminal backend at connect time —
    // NOT a hardcoded 80x24.
    initial_rows: u32,
    initial_cols: u32,

    state: TransportState,
    tmux_enabled: bool,
    tmux_session_name: String,
}

impl SSHTransport {
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
            event_tx,
            cancel_token: CancellationToken::new(),
            initial_rows,
            initial_cols,
            state: TransportState::Disconnected,
            tmux_enabled: false,
            tmux_session_name: String::new(),
        }
    }

    fn emit_state(&self) {
        self.state.emit(&self.event_tx);
    }

    /// Shell-quote a single argument for `/bin/sh -c` execution on the remote.
    ///
    /// This is a minimal implementation: if the string contains shell
    /// metacharacters we wrap it in single quotes and escape embedded single
    /// quotes. Good enough for tmux session names; full shell quoting is a
    /// Phase-3 utility candidate.
    fn shell_quote(s: &str) -> String {
        if s.is_empty() {
            return "''".into();
        }
        if s.chars()
            .all(|c| c.is_alphanumeric() || "_-./@+".contains(c))
        {
            return s.into();
        }
        let escaped = s.replace('\'', "'\\''");
        format!("'{}'", escaped)
    }
}

#[async_trait]
impl Transport for SSHTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        if self.state == TransportState::Connected {
            return Err(TransportError::ChannelError("already connected".into()));
        }

        self.state = TransportState::Connecting;
        self.emit_state();

        // 2. Open our channel.
        let channel = self.connection.open_session_channel().await?;

        // 3. Request PTY with the ACTUAL current terminal size.
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

        // 4. Shell or tmux.
        if self.tmux_enabled {
            let session_name = format!("wayterm-{}", self.tmux_session_name);
            let cmd = format!("tmux new -A -s {}", Self::shell_quote(&session_name));
            channel
                .exec(false, cmd.as_bytes())
                .await
                .map_err(TransportError::from_russh)?;
        } else {
            channel
                .request_shell(false)
                .await
                .map_err(TransportError::from_russh)?;
        }

        // 5. Split the channel so the reader task owns the read half and we keep
        //    the write half for write()/resize().
        let (mut read_half, write_half) = channel.split();

        // 6. Spawn the reader task — THE read path. Pushes chunks until EOF.
        let event_tx = self.event_tx.clone();
        let cancel = self.cancel_token.clone();
        self.reader_task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = read_half.wait() => match msg {
                        Some(ChannelMsg::Data { data }) => {
                            if event_tx
                                .send(TransportEvent::DataReceived(data.to_vec()))
                                .await
                                .is_err()
                            {
                                break; // UI side gone (tab closed)
                            }
                        }
                        // ExtendedData is treated the same as Data for now — v1
                        // does not separate stderr into a distinct UI stream.
                        Some(ChannelMsg::ExtendedData { data, .. }) => {
                            if event_tx
                                .send(TransportEvent::DataReceived(data.to_vec()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                            let _ = event_tx.send(TransportEvent::ConnectionLost).await;
                            break;
                        }
                        None => {
                            let _ = event_tx.send(TransportEvent::ConnectionLost).await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }));

        self.channel_write = Some(write_half);

        self.state = TransportState::Connected;
        self.emit_state();

        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        // Idempotent: safe to call twice, safe if never connected.
        self.cancel_token.cancel();
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        // ChannelWriteHalf has no close() in russh 0.64; dropping it is sufficient
        // because the reader task already owns the read half and will exit on cancel.
        let _ = self.channel_write.take();
        // NOTE: the SshConnection itself is NOT disconnected here — other
        // tabs/the SFTP sidebar may share it. The supervisor disconnects
        // it when the last Arc drops.
        self.state = TransportState::Disconnected;
        self.emit_state();
        Ok(())
    }

    fn state(&self) -> TransportState {
        self.state.clone()
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if let Some(ch) = &mut self.channel_write {
            ch.data_bytes(bytes::Bytes::copy_from_slice(data))
                .await
                .map_err(TransportError::from_russh)
        } else {
            Err(TransportError::Disconnected)
        }
    }

    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError> {
        if let Some(ch) = &mut self.channel_write {
            ch.window_change(cols, rows, 0, 0)
                .await
                .map_err(TransportError::from_russh)
        } else {
            Err(TransportError::Disconnected)
        }
    }

    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String) {
        self.tmux_enabled = enabled;
        self.tmux_session_name = session_name;
    }

    fn transport_type(&self) -> TransportType {
        TransportType::SSH
    }

    fn description(&self) -> String {
        format!(
            "{}@{}:{}",
            self.connection.username(),
            self.connection.host(),
            self.connection.port()
        )
    }
}

#[cfg(test)]
mod tests;
