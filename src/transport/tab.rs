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
        let transport = create_transport(config, connection, event_tx, initial_rows, initial_cols)?;
        Ok(Self::wire(transport, event_rx, terminal, main_ctx))
    }

    fn wire(
        mut transport: Box<dyn Transport>,
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
        let pump_state_tx = state_tx.clone();

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
                let _ = pump_state_tx.send(TransportEvent::Error(err.to_string()));
                transport.disconnect().await.ok();
                return;
            }
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
    /// [`CLOSE_JOIN_TIMEOUT`] to disconnect before it is aborted. Idempotent.
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    use async_trait::async_trait;

    use super::*;
    use crate::terminal::FakeBackend;
    use crate::transport::types::{TransportState, TransportType};

    /// Shared record of what the pump did to the mock transport.
    #[derive(Clone, Default)]
    struct Calls {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        resizes: Arc<Mutex<Vec<(u32, u32)>>>,
        connects: Arc<AtomicUsize>,
        disconnects: Arc<AtomicUsize>,
    }

    impl Calls {
        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes.lock().unwrap().clone()
        }
        fn resizes(&self) -> Vec<(u32, u32)> {
            self.resizes.lock().unwrap().clone()
        }
        fn connects(&self) -> usize {
            self.connects.load(Ordering::SeqCst)
        }
        fn disconnects(&self) -> usize {
            self.disconnects.load(Ordering::SeqCst)
        }
    }

    struct MockTransport {
        calls: Calls,
        fail_connect: bool,
        connected: bool,
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            self.calls.connects.fetch_add(1, Ordering::SeqCst);
            if self.fail_connect {
                return Err(TransportError::ConnectionFailed("connect refused".into()));
            }
            self.connected = true;
            Ok(())
        }

        async fn disconnect(&mut self) -> Result<(), TransportError> {
            self.calls.disconnects.fetch_add(1, Ordering::SeqCst);
            self.connected = false;
            Ok(())
        }

        fn state(&self) -> TransportState {
            if self.connected {
                TransportState::Connected
            } else {
                TransportState::Disconnected
            }
        }

        async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
            self.calls.writes.lock().unwrap().push(data.to_vec());
            Ok(())
        }

        async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError> {
            self.calls.resizes.lock().unwrap().push((rows, cols));
            Ok(())
        }

        fn set_tmux_enabled(&mut self, _enabled: bool, _session_name: String) {}

        fn transport_type(&self) -> TransportType {
            TransportType::SSH
        }

        fn description(&self) -> String {
            "mock".into()
        }
    }

    /// A wired tab over a mock transport. The test plays both outer ends:
    /// `transport_tx` is the transport's reader task, `term_tx` the backend.
    struct Harness {
        tab: TerminalTab,
        state_rx: mpsc::UnboundedReceiver<TransportEvent>,
        ctx: glib::MainContext,
        calls: Calls,
        transport_tx: mpsc::Sender<TransportEvent>,
        term_tx: mpsc::Sender<TerminalEvent>,
        backend: FakeBackend,
    }

    fn harness(fail_connect: bool) -> Harness {
        let ctx = glib::MainContext::new();
        let (transport_tx, event_rx) = mpsc::channel(256);
        let (term_tx, events) = mpsc::channel(256);
        let calls = Calls::default();
        let transport = MockTransport {
            calls: calls.clone(),
            fail_connect,
            connected: false,
        };
        let backend = FakeBackend::new();
        let terminal = TerminalHandle {
            backend: Arc::new(backend.clone()),
            events,
        };
        let (tab, state_rx) = TerminalTab::wire(Box::new(transport), event_rx, terminal, &ctx);
        Harness {
            tab,
            state_rx,
            ctx,
            calls,
            transport_tx,
            term_tx,
            backend,
        }
    }

    /// Drive `ctx` (and, via the sleeps, the tokio tasks) until `ready()`.
    fn drive(ctx: &glib::MainContext, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    async fn next_state(rx: &mut mpsc::UnboundedReceiver<TransportEvent>) -> TransportEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("state event before timeout")
            .expect("state channel open")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_forwards_keystrokes_and_resizes_to_the_transport() {
        let mut h = harness(false);
        h.term_tx
            .try_send(TerminalEvent::DataOut(b"ls\r".to_vec()))
            .unwrap();
        h.term_tx
            .try_send(TerminalEvent::SizeChange {
                rows: 40,
                cols: 120,
            })
            .unwrap();
        drive(&h.ctx, "transport.resize", || !h.calls.resizes().is_empty());
        assert_eq!(h.calls.writes(), vec![b"ls\r".to_vec()]);
        assert_eq!(h.calls.resizes(), vec![(40, 120)]);
        h.tab.close().await;
        assert_eq!(h.calls.disconnects(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_routes_output_to_the_backend_on_the_main_context() {
        let mut h = harness(false);
        h.transport_tx
            .try_send(TransportEvent::DataReceived(b"hello".to_vec()))
            .unwrap();
        drive(&h.ctx, "backend.write", || !h.backend.written().is_empty());
        assert_eq!(h.backend.written(), b"hello");
        h.tab.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_disconnects_once_and_is_idempotent() {
        let mut h = harness(false);
        drive(&h.ctx, "transport.connect", || h.calls.connects() == 1);
        h.tab.close().await;
        h.tab.close().await;
        assert_eq!(h.calls.disconnects(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_tab_still_disconnects() {
        let h = harness(false);
        drive(&h.ctx, "transport.connect", || h.calls.connects() == 1);
        drop(h.tab);
        drive(&h.ctx, "transport.disconnect", || {
            h.calls.disconnects() == 1
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_output_events_reach_the_state_channel_in_order() {
        let mut h = harness(false);
        for event in [
            TransportEvent::StateChanged(TransportState::Connected),
            TransportEvent::Error("boom".into()),
            TransportEvent::ConnectionLost,
            TransportEvent::SessionEnded,
        ] {
            h.transport_tx.try_send(event).unwrap();
        }
        assert!(matches!(
            next_state(&mut h.state_rx).await,
            TransportEvent::StateChanged(TransportState::Connected)
        ));
        assert!(
            matches!(next_state(&mut h.state_rx).await, TransportEvent::Error(m) if m == "boom")
        );
        assert!(matches!(
            next_state(&mut h.state_rx).await,
            TransportEvent::ConnectionLost
        ));
        assert!(matches!(
            next_state(&mut h.state_rx).await,
            TransportEvent::SessionEnded
        ));
        h.tab.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connect_failure_is_reported_and_tears_down_once() {
        let mut h = harness(true);
        let event = next_state(&mut h.state_rx).await;
        assert!(
            matches!(event, TransportEvent::Error(ref m) if m.contains("connect refused")),
            "expected connect error, got {event:?}"
        );
        drive(&h.ctx, "transport.disconnect", || {
            h.calls.disconnects() == 1
        });
        h.tab.close().await;
        assert_eq!(h.calls.disconnects(), 1);
    }

    #[test]
    fn create_transport_rejects_what_it_cannot_build() {
        let (tx, _rx) = mpsc::channel(1);
        let error = |session_type| {
            let config = SessionConfig {
                session_type,
                ..SessionConfig::default()
            };
            match create_transport(&config, None, tx.clone(), 24, 80) {
                Err(TransportError::ChannelError(message)) => message,
                Err(other) => panic!("unexpected error: {other}"),
                Ok(_) => panic!("must not build a transport"),
            }
        };
        assert_eq!(error(SessionType::SSH), "no connection");
        assert!(error(SessionType::RDP).contains("doc 08"));
        assert!(error(SessionType::VNC).contains("doc 08"));
        let serial = SessionConfig {
            session_type: SessionType::Serial,
            ..SessionConfig::default()
        };
        let transport = create_transport(&serial, None, tx.clone(), 24, 80).unwrap();
        assert!(matches!(transport.transport_type(), TransportType::Serial));
    }

    #[test]
    fn create_transport_builds_a_local_shell() {
        let (tx, _rx) = mpsc::channel(1);
        let config = SessionConfig {
            session_type: SessionType::LocalShell,
            ..SessionConfig::default()
        };
        let transport = create_transport(&config, None, tx, 24, 80).expect("local transport");
        assert!(matches!(
            transport.transport_type(),
            TransportType::LocalPTY
        ));
    }
}
