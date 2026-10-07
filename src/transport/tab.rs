//! TerminalTab — the transport ↔ terminal-backend wiring layer.
//!
//! Doc 04 §Wiring defines this type: one pump per tab forwards
//! [`TerminalEvent`]s to the [`Transport`], and a bridge task routes
//! [`TransportEvent`]s back to the terminal on the GTK main thread.
//!
//! # Placement
//!
//! Doc 04 (the transport-layer design) owns [`TerminalTab`] and the struct
//! holds a `Box<dyn Transport>`, so this module lives in `src/transport/`,
//! not `src/ui/` (which holds the GTK notebook widgets, P2-7).
//!
//! # Deviations from doc 04's sketch (reported, not silent)
//!
//! * **`glib::MainContext::channel` no longer exists** in the pinned glib
//!   0.22 (removed upstream after doc 04 was written). The doc's "channel
//!   attached to `main_ctx`, receiver runs `backend.write` on the main
//!   thread" becomes a bounded `tokio::sync::mpsc` drained by a
//!   `spawn_local` future on `main_ctx` — cross-thread wake is handled by
//!   glib's own task-source waker, and the main-thread-only invariant
//!   holds because the drain future is `!Send`. Output is bounded (256)
//!   so the doc's backpressure chain holds: main thread stalls → bridge
//!   blocks → `event_rx` fills → reader awaits → SSH flow control slows
//!   the server. The second (state) channel is unbounded and handed to
//!   the UI as a receiver.
//! * **`new()` takes the terminal-event receiver** (`events`): doc 04's
//!   sketch creates the term channel inside `new()` while also stating the
//!   backend was already constructed with `term_tx` — the caller has to
//!   own that channel (see [`create_terminal`](crate::terminal::create_terminal)),
//!   so the receiver half is a parameter and `term_tx` goes to the backend
//!   before `new()` is called. It also returns `Result` — the sketch's `?`
//!   implies it, and doc 04 forbids panicking on a malformed session row.
//! * **No `transport` field**: the sketch moves `transport` into the pump
//!   task, so the struct field could never be populated. Connection state
//!   reaches the UI through the second channel instead (deliverable: status
//!   bar / reconnect prompt, consumed in P2-7).
//! * **Task handles are `Option<JoinHandle>`** so `close()` can take, join
//!   and be idempotent — the same discipline `SSHTransport::disconnect`
//!   applies to its reader task.
//! * **The `TransportEvent` bridge is its own task** (doc 04's diagram
//!   shows the pump and the channel-reader path as separate tasks). Folding
//!   it into the pump would deadlock under simultaneous full-duplex load:
//!   a blocked `transport.write` would stop draining `event_rx`, which
//!   closes the SSH receive window, which blocks the server, which stops
//!   it reading our writes.

use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::store::model::{SessionConfig, SessionType};
use crate::terminal::{TerminalBackend, TerminalEvent};
use crate::transport::connection::SshConnection;
use crate::transport::ssh::SSHTransport;
use crate::transport::types::{Transport, TransportError, TransportEvent};

/// How long `close()` gives the pump to exit on the cancel token before it
/// aborts the task outright (doc 04 §Lifecycle: "abort tasks").
const CLOSE_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Transport factory (doc 04 §Transport Factory).
///
/// Maps a [`SessionConfig`] to the byte-stream transport for that session
/// type. Local PTY and serial transports are Phase 5 (doc 04 describes
/// their design; they are not implemented yet), and RDP/VNC are not
/// byte-stream transports at all — both return `Err`, never panic, so a
/// malformed session row cannot crash the app.
pub fn create_transport(
    config: &SessionConfig,
    connection: Option<Arc<SshConnection>>,
    event_tx: mpsc::Sender<TransportEvent>,
    initial_rows: u32,
    initial_cols: u32,
) -> Result<Box<dyn Transport>, TransportError> {
    match &config.session_type {
        SessionType::SSH => Ok(Box::new(SSHTransport::new(
            connection.ok_or_else(|| TransportError::ChannelError("no connection".into()))?,
            event_tx,
            initial_rows,
            initial_cols,
        ))),
        SessionType::LocalShell | SessionType::Serial => Err(TransportError::ChannelError(
            "local PTY and serial transports are Phase 5 (doc 04)".into(),
        )),
        // RDP/VNC return Err, not panic!() — a malformed session row in the
        // DB must not crash the app (doc 04 §Transport Factory).
        SessionType::RDP | SessionType::VNC => Err(TransportError::ChannelError(
            "RDP/VNC don't use byte-stream transport (doc 08)".into(),
        )),
    }
}

/// One wired-up terminal tab: a [`Transport`], a shared
/// [`TerminalBackend`], and the two tasks that shuttle bytes between them
/// (doc 04 §Wiring + §Lifecycle & Cancellation).
///
/// The transport itself is owned by the pump task (see the module docs);
/// `close()` tears both tasks down in order.
pub struct TerminalTab {
    /// GTK-side handle to the terminal — the tab UI embeds
    /// `backend.widget()` (P2-7) and the output-path future writes into it.
    backend: Arc<dyn TerminalBackend>,

    /// The pump task: `TerminalEvent` → `Transport::write` / `resize`,
    /// plus `transport.disconnect()` as its last act.
    pump_task: Option<JoinHandle<()>>,

    /// The bridge task: `TransportEvent` → terminal output channel /
    /// state channel. Separate from the pump (see module docs).
    bridge_task: Option<JoinHandle<()>>,

    /// Cancelled by [`TerminalTab::close`]; both tasks select on it.
    cancel: CancellationToken,

    /// Second channel (doc 04 §Wiring: "status bar, reconnect prompt"):
    /// every `TransportEvent` that is not terminal output. The UI attaches
    /// a consumer in P2-7 via [`TerminalTab::take_state_rx`].
    state_rx: Option<mpsc::UnboundedReceiver<TransportEvent>>,
}

impl TerminalTab {
    /// Wire a transport for `config` to `backend` (doc 04 §Wiring).
    ///
    /// `events` is the receiver half of the backend's `TerminalEvent`
    /// channel: the caller creates that channel, passes the sender to
    /// [`create_terminal`](crate::terminal::create_terminal) when it builds
    /// the backend, and passes the receiver here. `initial_rows` /
    /// `initial_cols` set the PTY size requested at connect time — they
    /// come from the real terminal widget at the UI integration step, never
    /// a hardcoded 80×24 (doc 10, Phase 2).
    ///
    /// Must be called from the thread that runs `main_ctx` (it attaches the
    /// output-path future there) and inside a tokio runtime (it spawns the
    /// pump and bridge tasks).
    pub fn new(
        config: &SessionConfig,
        connection: Option<Arc<SshConnection>>,
        backend: Arc<dyn TerminalBackend>,
        events: mpsc::Receiver<TerminalEvent>,
        main_ctx: &glib::MainContext,
        initial_rows: u32,
        initial_cols: u32,
    ) -> Result<Self, TransportError> {
        let (event_tx, event_rx) = mpsc::channel(256);
        let transport = create_transport(config, connection, event_tx, initial_rows, initial_cols)?;
        Ok(Self::wire(transport, event_rx, backend, events, main_ctx))
    }

    /// Test seam: wire a hand-built transport (e.g. a mock) to `backend`
    /// without going through [`create_transport`]. The caller pairs
    /// `event_rx` with the `event_tx` it handed to the transport.
    #[cfg(test)]
    fn for_test(
        transport: Box<dyn Transport>,
        event_rx: mpsc::Receiver<TransportEvent>,
        backend: Arc<dyn TerminalBackend>,
        events: mpsc::Receiver<TerminalEvent>,
        main_ctx: &glib::MainContext,
    ) -> Self {
        Self::wire(transport, event_rx, backend, events, main_ctx)
    }

    /// The wiring itself: output path, state channel, bridge task, pump
    /// task (doc 04 §Wiring steps 1–3).
    fn wire(
        mut transport: Box<dyn Transport>,
        event_rx: mpsc::Receiver<TransportEvent>,
        backend: Arc<dyn TerminalBackend>,
        events: mpsc::Receiver<TerminalEvent>,
        main_ctx: &glib::MainContext,
    ) -> Self {
        // 1. GTK side: terminal output arrives as `Vec<u8>` chunks; the
        //    drain future runs on the main context, so `backend.write`
        //    stays on the main thread.
        let (gtk_tx, mut gtk_rx) = mpsc::channel::<Vec<u8>>(256);
        {
            let backend = backend.clone();
            let _drain = main_ctx.spawn_local(async move {
                while let Some(chunk) = gtk_rx.recv().await {
                    backend.write(&chunk);
                }
            });
        }

        // 2. Second channel: StateChanged / Error / ConnectionLost —
        //    status bar, reconnect prompt (doc 04 §Wiring). Unbounded:
        //    these are rare and must never block the bridge.
        let (state_tx, state_rx) = mpsc::unbounded_channel::<TransportEvent>();

        let cancel = CancellationToken::new();

        // 3a. Bridge task: `TransportEvent` → the two GTK-side channels.
        let bridge_task = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let mut event_rx = event_rx;
                'bridge: loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        ev = event_rx.recv() => match ev {
                            // Transport gone (pump finished, transport dropped).
                            None => break,
                            Some(TransportEvent::DataReceived(chunk)) => {
                                // Output path; select on cancel so close()
                                // stays responsive even when the main
                                // thread is stalled and the queue is full.
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => break 'bridge,
                                    res = gtk_tx.send(chunk) => {
                                        if res.is_err() {
                                            break 'bridge; // main-side future gone
                                        }
                                    }
                                }
                            }
                            // StateChanged / Error / ConnectionLost.
                            Some(ev) => {
                                // Receiver dropped only when the UI is done
                                // with it; keep bridging output either way.
                                let _ = state_tx.send(ev);
                            }
                        }
                    }
                }
            })
        };

        // 4. Tokio side: pump task (doc 04 §Wiring sketch).
        let pump_task = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let mut events = events;
                if transport.connect().await.is_err() {
                    return;
                }
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        ev = events.recv() => match ev {
                            Some(TerminalEvent::DataOut(d)) => {
                                transport.write(&d).await.ok();
                            }
                            Some(TerminalEvent::SizeChange { rows, cols }) => {
                                transport.resize(rows, cols).await.ok();
                            }
                            Some(TerminalEvent::Closed) | None => break,
                            _ => {}
                        },
                    }
                }
                transport.disconnect().await.ok();
            })
        };

        Self {
            backend,
            pump_task: Some(pump_task),
            bridge_task: Some(bridge_task),
            cancel,
            state_rx: Some(state_rx),
        }
    }

    /// Cancel both tasks and wait for them (doc 04 §Lifecycle: cancel →
    /// pump exits → `transport.disconnect()`).
    ///
    /// The pump gets [`CLOSE_JOIN_TIMEOUT`] to exit cleanly — its last act
    /// is `disconnect()` — before it is aborted. Idempotent: subsequent
    /// calls are no-ops.
    pub async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.pump_task.take() {
            let deadline = tokio::time::Instant::now() + CLOSE_JOIN_TIMEOUT;
            while !task.is_finished() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            if !task.is_finished() {
                task.abort();
            }
            let _ = task.await;
        }
        if let Some(task) = self.bridge_task.take() {
            let _ = task.await;
        }
    }

    /// The terminal to embed in the tab UI (P2-7).
    pub fn backend(&self) -> &Arc<dyn TerminalBackend> {
        &self.backend
    }

    /// Take the receiver half of the state channel (doc 04 §Wiring's
    /// "second glib channel"). The UI consumes it once, in P2-7.
    pub fn take_state_rx(&mut self) -> Option<mpsc::UnboundedReceiver<TransportEvent>> {
        self.state_rx.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    use async_trait::async_trait;

    use crate::terminal::FakeBackend;
    use crate::transport::types::{TransportState, TransportType};

    #[derive(Clone)]
    struct MockHandle {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        resizes: Arc<Mutex<Vec<(u32, u32)>>>,
        connects: Arc<AtomicUsize>,
        disconnects: Arc<AtomicUsize>,
        event_tx: mpsc::Sender<TransportEvent>,
    }

    impl MockHandle {
        /// Push a canned transport event — the mock's reader task.
        fn push(&self, ev: TransportEvent) {
            self.event_tx.try_send(ev).expect("event channel open");
        }
        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        fn resizes(&self) -> Vec<(u32, u32)> {
            self.resizes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        fn connects(&self) -> usize {
            self.connects.load(Ordering::SeqCst)
        }
        fn disconnects(&self) -> usize {
            self.disconnects.load(Ordering::SeqCst)
        }
    }

    /// Test transport: records writes/resizes/disconnects. The reader half
    /// of a real transport is its own task pushing into `event_tx`; the
    /// mock's "reader" is the test thread pushing via [`MockHandle`].
    struct MockTransport {
        state: TransportState,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        resizes: Arc<Mutex<Vec<(u32, u32)>>>,
        connects: Arc<AtomicUsize>,
        disconnects: Arc<AtomicUsize>,
    }

    impl MockTransport {
        fn new(event_tx: mpsc::Sender<TransportEvent>) -> (Self, MockHandle) {
            let handle = MockHandle {
                writes: Arc::new(Mutex::new(Vec::new())),
                resizes: Arc::new(Mutex::new(Vec::new())),
                connects: Arc::new(AtomicUsize::new(0)),
                disconnects: Arc::new(AtomicUsize::new(0)),
                event_tx,
            };
            let transport = Self {
                state: TransportState::Disconnected,
                writes: handle.writes.clone(),
                resizes: handle.resizes.clone(),
                connects: handle.connects.clone(),
                disconnects: handle.disconnects.clone(),
            };
            (transport, handle)
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            self.state = TransportState::Connected;
            Ok(())
        }

        async fn disconnect(&mut self) -> Result<(), TransportError> {
            self.disconnects.fetch_add(1, Ordering::SeqCst);
            self.state = TransportState::Disconnected;
            Ok(())
        }

        fn state(&self) -> TransportState {
            self.state.clone()
        }

        async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(data.to_vec());
            Ok(())
        }

        async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError> {
            self.resizes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((rows, cols));
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

    struct Harness {
        tab: TerminalTab,
        ctx: glib::MainContext,
        handle: MockHandle,
        fake: FakeBackend,
    }

    fn harness() -> Harness {
        let ctx = glib::MainContext::new();
        let (event_tx, event_rx) = mpsc::channel(256);
        let (transport, handle) = MockTransport::new(event_tx);
        let (term_tx, term_rx) = mpsc::channel(256);
        let fake = FakeBackend::new();
        let mut emitter = fake.clone();
        emitter.event_tx = Some(term_tx);
        let tab =
            TerminalTab::for_test(Box::new(transport), event_rx, Arc::new(fake), term_rx, &ctx);
        Harness {
            tab,
            ctx,
            handle,
            fake: emitter,
        }
    }

    /// Drive `main_ctx` (and, via its sleeps, the tokio tasks) until
    /// `ready()` holds. Display-free: `iteration(false)` needs no GTK.
    fn drive(ctx: &glib::MainContext, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Pump path: a backend `DataOut` event reaches `Transport::write`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_forwards_keystrokes_to_transport() {
        let mut h = harness();
        h.fake.emit(TerminalEvent::DataOut(b"ls\r".to_vec()));
        drive(&h.ctx, "transport.write", || !h.handle.writes().is_empty());
        assert_eq!(h.handle.writes(), vec![b"ls\r".to_vec()]);
        h.tab.close().await;
        assert_eq!(h.handle.disconnects(), 1);
    }

    /// Reader path: `DataReceived` reaches `backend.write` on the main
    /// context (doc 04 §Wiring step 1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reader_routes_data_received_into_backend() {
        let mut h = harness();
        h.handle
            .push(TransportEvent::DataReceived(b"hello".to_vec()));
        drive(&h.ctx, "backend.write", || !h.fake.written().is_empty());
        assert_eq!(h.fake.written(), b"hello");
        h.tab.close().await;
        assert_eq!(h.handle.disconnects(), 1);
    }

    /// Cancellation: close() joins the pump (which disconnects), and a
    /// second close() is a no-op (SSHTransport::disconnect discipline).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_cancels_pump_disconnects_once_and_is_idempotent() {
        let mut h = harness();
        drive(&h.ctx, "transport.connect", || h.handle.connects() == 1);
        h.tab.close().await;
        assert_eq!(h.handle.disconnects(), 1);
        h.tab.close().await;
        assert_eq!(h.handle.disconnects(), 1);
    }

    /// Resize propagation: `SizeChange` through the pump reaches
    /// `Transport::resize` with rows/cols in doc 04's order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn size_change_reaches_transport_resize() {
        let mut h = harness();
        h.fake.emit(TerminalEvent::SizeChange {
            rows: 40,
            cols: 120,
        });
        drive(&h.ctx, "transport.resize", || {
            !h.handle.resizes().is_empty()
        });
        assert_eq!(h.handle.resizes(), vec![(40, 120)]);
        h.tab.close().await;
        assert_eq!(h.handle.disconnects(), 1);
    }

    /// The second channel: non-data transport events reach the state
    /// receiver, in order (doc 04 §Wiring: status bar, reconnect prompt).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_data_transport_events_reach_state_channel() {
        let mut h = harness();
        h.handle
            .push(TransportEvent::StateChanged(TransportState::Connected));
        h.handle.push(TransportEvent::Error("boom".into()));
        h.handle.push(TransportEvent::ConnectionLost);
        let mut rx = h.tab.take_state_rx().expect("state receiver taken once");

        let mut received = Vec::new();
        for _ in 0..3 {
            received.push(
                tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("state event before timeout")
                    .expect("state channel open"),
            );
        }
        assert!(matches!(
            received[0],
            TransportEvent::StateChanged(TransportState::Connected)
        ));
        assert!(matches!(received[1], TransportEvent::Error(ref m) if m == "boom"));
        assert!(matches!(received[2], TransportEvent::ConnectionLost));
        assert!(h.tab.take_state_rx().is_none());
        h.tab.close().await;
    }

    /// Doc 04 §Transport Factory mapping — every non-SSH type is an error,
    /// never a panic (malformed session rows must not crash the app).
    #[test]
    fn create_transport_maps_session_types_per_doc04() {
        let (tx, _rx) = mpsc::channel::<TransportEvent>(1);
        let cfg = |session_type| SessionConfig {
            session_type,
            ..SessionConfig::default()
        };

        let ssh = cfg(SessionType::SSH);
        match create_transport(&ssh, None, tx.clone(), 24, 80) {
            Err(TransportError::ChannelError(m)) => assert_eq!(m, "no connection"),
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("SSH without a connection must fail"),
        }

        for unimplemented in [SessionType::LocalShell, SessionType::Serial] {
            let cfg = cfg(unimplemented);
            match create_transport(&cfg, None, tx.clone(), 24, 80) {
                Err(TransportError::ChannelError(m)) => {
                    assert!(m.contains("Phase 5"), "got: {m}")
                }
                Err(e) => panic!("unexpected error: {e}"),
                Ok(_) => panic!("{cfg:?} must not build a transport yet"),
            }
        }

        for non_stream in [SessionType::RDP, SessionType::VNC] {
            let cfg = cfg(non_stream);
            match create_transport(&cfg, None, tx.clone(), 24, 80) {
                Err(TransportError::ChannelError(m)) => {
                    assert_eq!(m, "RDP/VNC don't use byte-stream transport (doc 08)")
                }
                Err(e) => panic!("unexpected error: {e}"),
                Ok(_) => panic!("{cfg:?} must not build a byte-stream transport"),
            }
        }
    }
}
