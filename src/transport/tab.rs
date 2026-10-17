//! [`TerminalTab`]: the wiring between one transport and one terminal
//! backend (doc 04 §Wiring).
//!
//! Three tasks per tab, each crossing the GTK/tokio boundary once:
//!
//! * the **pump** (tokio) connects the transport, then forwards
//!   [`TerminalEvent`]s to `write`/`resize`, and disconnects last;
//! * the **bridge** (tokio) routes [`TransportEvent`]s: output to the
//!   terminal, everything else to the state channel returned by
//!   [`TerminalTab::new`];
//! * the **drain** (GTK main context) feeds output into `backend.write`.
//!
//! The bridge is separate from the pump on purpose: if it were folded in, a
//! `transport.write` blocked on a full SSH window would stop draining
//! output, the server's window would close in turn, and both sides would
//! deadlock under full-duplex load.

use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::store::model::{SessionConfig, SessionType};
use crate::terminal::{TerminalEvent, TerminalHandle};
use crate::transport::connection::SshConnection;
use crate::transport::local::LocalTransport;
use crate::transport::serial::SerialTransport;
use crate::transport::ssh::SSHTransport;
use crate::transport::types::{Transport, TransportError, TransportEvent};

/// How long [`TerminalTab::close`] lets the pump disconnect before aborting it.
const CLOSE_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Transport factory (doc 04 §Transport Factory). Unsupported session types
/// are errors, never panics — a malformed row must not crash the app.
pub fn create_transport(
    config: &SessionConfig,
    connection: Option<Arc<SshConnection>>,
    event_tx: mpsc::Sender<TransportEvent>,
    initial_rows: u32,
    initial_cols: u32,
) -> Result<Box<dyn Transport>, TransportError> {
    match config.session_type {
        SessionType::SSH => {
            let connection =
                connection.ok_or_else(|| TransportError::ChannelError("no connection".into()))?;
            let mut transport = SSHTransport::new(connection, event_tx, initial_rows, initial_cols);
            if let Some(suffix) = config.tmux_session_suffix() {
                transport.set_tmux_enabled(true, suffix);
            }
            Ok(Box::new(transport))
        }
        SessionType::Serial => Ok(Box::new(SerialTransport::new(config, event_tx))),
        SessionType::LocalShell => Ok(Box::new(LocalTransport::new(
            config,
            event_tx,
            initial_rows,
            initial_cols,
        ))),
        SessionType::RDP | SessionType::VNC => Err(TransportError::ChannelError(
            "RDP/VNC don't use byte-stream transport (doc 08)".into(),
        )),
    }
}

/// A transport wired to a terminal backend. Dropping it cancels the tasks
/// (the pump still disconnects on its way out); [`close`](Self::close) also
/// waits for them.
pub struct TerminalTab {
    pump_task: Option<JoinHandle<()>>,
    bridge_task: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    description: String,
}

impl TerminalTab {
    /// Wire a transport for `config` to `terminal`. `initial_rows` and
    /// `initial_cols` are the widget's measured grid (never a hardcoded
    /// 80×24). Returns the tab and its state channel: every transport event
    /// that is not terminal output, including the pump's connect failure.
    ///
    /// Must run on the thread that owns `main_ctx`, inside a tokio runtime.
    pub fn new(
        config: &SessionConfig,
        connection: Option<Arc<SshConnection>>,
        terminal: TerminalHandle,
        main_ctx: &glib::MainContext,
        initial_rows: u32,
        initial_cols: u32,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TransportEvent>), TransportError> {
        let (event_tx, event_rx) = mpsc::channel(256);
        let transport = create_transport(
            config,
            connection,
            event_tx.clone(),
            initial_rows,
            initial_cols,
        )?;
        Ok(Self::wire(
            transport, event_tx, event_rx, terminal, main_ctx,
        ))
    }

    /// `events` is the transport's own event channel, which the pump
    /// reports a failed connect on.
    fn wire(
        mut transport: Box<dyn Transport>,
        events: mpsc::Sender<TransportEvent>,
        mut event_rx: mpsc::Receiver<TransportEvent>,
        terminal: TerminalHandle,
        main_ctx: &glib::MainContext,
    ) -> (Self, mpsc::UnboundedReceiver<TransportEvent>) {
        let TerminalHandle {
            backend,
            events: mut term_rx,
        } = terminal;
        let cancel = CancellationToken::new();
        let description = transport.description();

        // Drain: bounded, so a stalled main thread backs up into the
        // bridge, the reader, and finally SSH flow control.
        let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(256);
        main_ctx.spawn_local(async move {
            while let Some(chunk) = output_rx.recv().await {
                backend.write(&chunk);
            }
        });

        // State events are rare and must never block the bridge.
        let (state_tx, state_rx) = mpsc::unbounded_channel();

        let bridge_cancel = cancel.clone();
        let bridge_task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = bridge_cancel.cancelled() => break,
                    event = event_rx.recv() => event,
                };
                match event {
                    None => break,
                    Some(TransportEvent::DataReceived(chunk)) => {
                        // Still cancellable while the drain is backed up.
                        let sent = tokio::select! {
                            biased;
                            _ = bridge_cancel.cancelled() => break,
                            sent = output_tx.send(chunk) => sent,
                        };
                        if sent.is_err() {
                            break;
                        }
                    }
                    Some(other) => {
                        let _ = state_tx.send(other);
                    }
                }
            }
        });

        let pump_cancel = cancel.clone();
        let pump_task = tokio::spawn(async move {
            if let Err(err) = transport.connect().await {
                // Behind whatever the transport announced while failing, so
                // the bridge delivers the error last: the tab ends on it.
                tokio::select! {
                    biased;
                    _ = pump_cancel.cancelled() => {}
                    _ = events.send(TransportEvent::Error(err.to_string())) => {}
                }
                transport.disconnect().await.ok();
                return;
            }
            // From here on only the transport's reader sends.
            drop(events);
            loop {
                let event = tokio::select! {
                    biased;
                    _ = pump_cancel.cancelled() => break,
                    event = term_rx.recv() => event,
                };
                match event {
                    Some(TerminalEvent::DataOut(data)) => {
                        transport.write(&data).await.ok();
                    }
                    Some(TerminalEvent::SizeChange { rows, cols }) => {
                        transport.resize(rows, cols).await.ok();
                    }
                    Some(TerminalEvent::Closed) | None => break,
                    Some(_) => {}
                }
            }
            transport.disconnect().await.ok();
        });

        let tab = Self {
            pump_task: Some(pump_task),
            bridge_task: Some(bridge_task),
            cancel,
            description,
        };
        (tab, state_rx)
    }

    /// The transport's description (shell path, `device@baud`), shown in
    /// the status bar for sessions without a host.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Cancel both tasks and wait for them (doc 04 §Lifecycle). The pump gets
    /// `CLOSE_JOIN_TIMEOUT` to disconnect before it is aborted. Idempotent.
    pub async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(mut pump) = self.pump_task.take() {
            if tokio::time::timeout(CLOSE_JOIN_TIMEOUT, &mut pump)
                .await
                .is_err()
            {
                pump.abort();
                let _ = pump.await;
            }
        }
        if let Some(bridge) = self.bridge_task.take() {
            let _ = bridge.await;
        }
    }
}

/// The tasks keep each other alive (drain → backend → widget signals →
/// pump → transport → bridge → drain), so a tab dropped without `close`
/// — a page removed from the notebook — must still cancel them, or the SSH
/// channel and its connection are never released.
impl Drop for TerminalTab {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests;
