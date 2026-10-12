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
    assert!(matches!(next_state(&mut h.state_rx).await, TransportEvent::Error(m) if m == "boom"));
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
