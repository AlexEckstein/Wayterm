//! Reconnect-driver integration tests against a real sshd.
//!
//! Skipped unless the environment variables are set:
//!   SSH_TEST_HOST, SSH_TEST_PORT, SSH_TEST_USER, SSH_TEST_PASSWORD,
//!   SSH_TEST_KEY_PATH
//!
//! # What this does and does not prove
//!
//! The interesting case — the *remote* side killing the channel so the
//! reader task emits `ConnectionLost` — cannot be provoked from the client
//! without either a network fault or cooperation from a server we do not
//! control, and neither belongs in a suite that runs against a stock sshd.
//! So these tests drive the driver's real path honestly instead: connect
//! for real, tear the transport down on the client side (which is what a
//! drop looks like to the driver — the transport stops working and a fresh
//! `connect()` is required), then let [`ReconnectDriver`] reconnect against
//! the same live server and assert the resulting transport actually
//! carries bytes.
//!
//! Covered here: the driver's connect closure, its cancellation, and the
//! `TransportState` transitions on a real [`SshConnection`]. Not covered:
//! `ConnectionLost` detection itself, which lives in `SSHTransport`'s
//! reader task (asserted by that lane's `tests/transport_sshd.rs`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use russh::client::{self, Config, Handle};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use wayterm::store::model::HostKeyPolicy;
use wayterm::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
use wayterm::transport::credentials::{CredentialManager, PromptSource};
use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
use wayterm::transport::reconnect::{
    ConnectFn, ConnectFuture, ReconnectDriver, ReconnectEvent, ReconnectOutcome, ReconnectPolicy,
};
use wayterm::transport::ssh::SSHTransport;
use wayterm::transport::types::{Transport, TransportError, TransportEvent, TransportState};
use zeroize::Zeroizing;

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
}

fn read_fixture() -> Option<Fixture> {
    let host = std::env::var("SSH_TEST_HOST").ok()?;
    let port = std::env::var("SSH_TEST_PORT").ok()?.parse().ok()?;
    let user = std::env::var("SSH_TEST_USER").ok()?;
    let password = std::env::var("SSH_TEST_PASSWORD").ok()?;
    Some(Fixture {
        host,
        port,
        user,
        password,
    })
}

fn skip() {
    println!(
        "Skipping reconnect sshd integration test: set SSH_TEST_HOST, SSH_TEST_PORT, \
         SSH_TEST_USER, and SSH_TEST_PASSWORD to run it."
    );
}

fn accept_all_verifier() -> HostKeyVerifier {
    HostKeyVerifier::new(
        HostKeyPolicy::Off,
        Arc::new(|_| HostKeyDecision::Accept),
        None,
    )
}

async fn connect_raw(host: &str, port: u16, username: &str) -> Handle<AppClientHandler> {
    let config = Arc::new(Config::default());
    let addr = SocketAddr::new(
        host.parse::<IpAddr>()
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        port,
    );
    let handler = AppClientHandler {
        host: host.into(),
        port,
        username: username.into(),
        host_key_verifier: accept_all_verifier(),
        state_tx: None,
    };
    client::connect(config, addr, handler)
        .await
        .expect("raw sshd connection failed")
}

fn password_cred_manager(password: &str) -> CredentialManager {
    let prompt_source = PromptSource::new(Arc::new({
        let password = password.to_string();
        move |_session_id, secret_type| match secret_type {
            "password" => Some(Zeroizing::new(password.clone().into_bytes())),
            _ => None,
        }
    }));
    CredentialManager::new(vec![Box::new(prompt_source)])
}

async fn authenticated_connection(f: &Fixture) -> Arc<SshConnection> {
    let handle = connect_raw(&f.host, f.port, &f.user).await;
    let connection = SshConnection::new(handle, f.host.clone(), f.port, f.user.clone());
    connection
        .authenticate(
            &AuthMethod::Password(f.password.clone()),
            &password_cred_manager(&f.password),
            None,
        )
        .await
        .expect("authentication failed");
    Arc::new(connection)
}

/// A reconnectable transport slot standing in for the supervisor: one
/// shared [`SshConnection`], a fresh [`SSHTransport`] per attempt, exactly
/// doc 04's "fresh `connect()` through the supervisor".
struct Supervisor {
    connection: Arc<SshConnection>,
    /// The currently installed transport; replaced on every connect.
    current: Mutex<Option<SSHTransport>>,
    /// Where each new transport pushes its events.
    events: Mutex<mpsc::Sender<TransportEvent>>,
    connects: AtomicU32,
    rows: u32,
    cols: u32,
}

impl Supervisor {
    fn new(connection: Arc<SshConnection>) -> (Self, mpsc::Receiver<TransportEvent>) {
        let (events, rx) = mpsc::channel(64);
        (
            Self {
                connection,
                current: Mutex::new(None),
                events: Mutex::new(events),
                connects: AtomicU32::new(0),
                rows: 24,
                cols: 80,
            },
            rx,
        )
    }

    /// Hand back a fresh receiver for the next attempt's events.
    async fn next_event_rx(&self) -> mpsc::Receiver<TransportEvent> {
        let (tx, rx) = mpsc::channel(64);
        *self.events.lock().await = tx;
        rx
    }

    async fn connect(&self) -> Result<(), TransportError> {
        let tx = self.events.lock().await.clone();
        let mut transport = SSHTransport::new(self.connection.clone(), tx, self.rows, self.cols);
        transport.connect().await?;
        *self.current.lock().await = Some(transport);
        self.connects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// The connect closure the driver calls — doc 04's supervisor seam.
    fn connect_fn(self: &Arc<Self>) -> ConnectFn {
        let supervisor = self.clone();
        Arc::new(move || {
            let supervisor = supervisor.clone();
            Box::pin(async move { supervisor.connect().await }) as ConnectFuture
        })
    }

    async fn write(&self, data: &[u8]) -> Result<(), TransportError> {
        let mut guard = self.current.lock().await;
        guard
            .as_mut()
            .ok_or(TransportError::Disconnected)?
            .write(data)
            .await
    }

    async fn disconnect(&self) -> Result<(), TransportError> {
        let mut guard = self.current.lock().await;
        match guard.as_mut() {
            Some(transport) => transport.disconnect().await,
            None => Ok(()),
        }
    }

    async fn state(&self) -> TransportState {
        self.current
            .lock()
            .await
            .as_ref()
            .map(|t| t.state())
            .unwrap_or(TransportState::Disconnected)
    }

    fn connects(&self) -> u32 {
        self.connects.load(Ordering::SeqCst)
    }
}

/// Wait for `needle` to appear on the transport's event channel.
async fn collect_echo(
    rx: &mut mpsc::Receiver<TransportEvent>,
    needle: &str,
    deadline: Duration,
) -> String {
    let result = timeout(deadline, async {
        let mut output = String::new();
        loop {
            match rx.recv().await {
                Some(TransportEvent::DataReceived(bytes)) => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                    if output.contains(needle) {
                        return output;
                    }
                }
                Some(TransportEvent::ConnectionLost) => {
                    panic!("connection lost before seeing '{needle}'")
                }
                Some(_) => {}
                None => panic!("event channel closed before seeing '{needle}'"),
            }
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("timed out waiting for '{needle}'"))
}

/// Drain the driver's progress channel into a vector.
fn drain_events(rx: &mut mpsc::UnboundedReceiver<ReconnectEvent>) -> Vec<ReconnectEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// Doc 04's curve, checked against a real reconnect: connect, drop, let
/// the driver reconnect through the supervisor, and assert the new
/// transport actually executes a remote command.
#[tokio::test]
async fn driver_reconnects_against_a_real_server_and_exec_works() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = authenticated_connection(&f).await;
    let (supervisor, mut rx) = Supervisor::new(connection);
    let supervisor = Arc::new(supervisor);
    let connect = supervisor.connect_fn();

    // First connection.
    connect().await.expect("initial connect failed");
    supervisor
        .write(b"echo 'before-drop'\n")
        .await
        .expect("write failed");
    collect_echo(&mut rx, "before-drop", Duration::from_secs(10)).await;

    // Drop it, the way a network fault looks from here.
    supervisor.disconnect().await.expect("disconnect failed");
    assert_eq!(supervisor.state().await, TransportState::Disconnected);

    // Drive the real reconnect.
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let mut rx = supervisor.next_event_rx().await;
    let outcome = driver
        .run(connect.clone(), &events, None, Some(&state_tx))
        .await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 1 });
    assert_eq!(supervisor.state().await, TransportState::Connected);
    assert_eq!(supervisor.connects(), 2, "a fresh transport per attempt");

    // The reconnected transport carries bytes.
    supervisor
        .write(b"echo 'after-reconnect'\n")
        .await
        .expect("write after reconnect failed");
    collect_echo(&mut rx, "after-reconnect", Duration::from_secs(10)).await;

    let log = drain_events(&mut event_rx);
    assert!(matches!(
        log.last(),
        Some(ReconnectEvent::Connected { attempts: 1 })
    ));

    let mut saw_reconnecting = false;
    let mut saw_connected = false;
    while let Ok(ev) = state_rx.try_recv() {
        if let TransportEvent::StateChanged(state) = ev {
            match state {
                TransportState::Reconnecting { attempt } => {
                    assert_eq!(attempt, 1);
                    saw_reconnecting = true;
                }
                TransportState::Connected => saw_connected = true,
                _ => {}
            }
        }
    }
    assert!(saw_reconnecting);
    assert!(saw_connected);

    supervisor
        .disconnect()
        .await
        .expect("final disconnect failed");
}

/// The driver retries against the real server: the first attempt is refused
/// (no transport installed yet), the second succeeds — the same loop the
/// auto-mode unit tests walk on virtual time, here on a live server.
#[tokio::test]
async fn driver_retries_a_failed_attempt_then_connects_for_real() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = authenticated_connection(&f).await;
    let (supervisor, _rx) = Supervisor::new(connection);
    let supervisor = Arc::new(supervisor);
    let connect = supervisor.connect_fn();

    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_seen = attempts.clone();
    // Fail attempt 1 only: the real failure mode after a half-open socket.
    let flaky: ConnectFn = Arc::new(move || {
        let supervisor_connect = connect.clone();
        let attempts = attempts_seen.clone();
        Box::pin(async move {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                return Err(TransportError::ConnectionFailed("simulated drop".into()));
            }
            supervisor_connect().await
        }) as ConnectFuture
    });

    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let outcome = driver.run(flaky, &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 2 });
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(supervisor.connects(), 1);

    let log = drain_events(&mut event_rx);
    assert!(log
        .iter()
        .any(|ev| matches!(ev, ReconnectEvent::AttemptFailed { attempt: 1, .. })));
    assert!(matches!(
        log.last(),
        Some(ReconnectEvent::Connected { attempts: 2 })
    ));

    supervisor.disconnect().await.expect("disconnect failed");
}

/// Disconnect during backoff stops the loop before it can open another
/// channel against the server — the same guard the paused-time unit tests
/// assert, here measured in real seconds.
#[tokio::test]
async fn cancel_mid_backoff_opens_no_further_channel() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = authenticated_connection(&f).await;
    let (supervisor, _rx) = Supervisor::new(connection);
    let supervisor = Arc::new(supervisor);
    let connect = supervisor.connect_fn();

    let cancel = CancellationToken::new();
    let driver = ReconnectDriver::with_cancel_token(ReconnectPolicy::auto(), cancel.clone());
    let (events, _rx) = mpsc::unbounded_channel();

    let run = tokio::spawn({
        let driver = driver.clone();
        let connect = connect.clone();
        let events = events.clone();
        async move { driver.run(connect, &events, None, None).await }
    });

    // Well inside doc 04's first 1s rung, then cancel: no attempt may run.
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();

    let outcome = timeout(Duration::from_secs(10), run)
        .await
        .expect("driver stopped promptly after cancel")
        .expect("driver task");
    assert_eq!(outcome, ReconnectOutcome::Cancelled);
    assert_eq!(supervisor.connects(), 0, "no channel may be opened");
    assert_eq!(supervisor.state().await, TransportState::Disconnected);
}

/// Prompt mode against the real server: with a declining responder the
/// driver must never touch the transport.
#[tokio::test]
async fn prompt_mode_declined_never_opens_a_channel() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = authenticated_connection(&f).await;
    let (supervisor, _rx) = Supervisor::new(connection);
    let supervisor = Arc::new(supervisor);
    let connect = supervisor.connect_fn();

    let asked = Arc::new(AtomicU32::new(0));
    let asked_inner = asked.clone();
    let decline: wayterm::transport::reconnect::ConfirmFn = Arc::new(move |_attempt| {
        asked_inner.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { false }) as wayterm::transport::reconnect::ConfirmFuture
    });

    let driver = ReconnectDriver::new(ReconnectPolicy::prompt());
    let (events, _rx) = mpsc::unbounded_channel();
    let outcome = driver.run(connect, &events, Some(decline), None).await;

    assert_eq!(outcome, ReconnectOutcome::Declined { attempt: 1 });
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(supervisor.connects(), 0);
    assert_eq!(supervisor.state().await, TransportState::Disconnected);
}

/// The real transport emits `Reconnecting`-able states: after a drop the
/// old transport reads `Disconnected`, which is exactly the state doc 04's
/// close guard treats as "nothing to confirm" and the driver treats as
/// "needs a fresh connect".
#[tokio::test]
async fn a_dropped_real_transport_reads_disconnected_and_needs_a_fresh_connect() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = authenticated_connection(&f).await;
    let (supervisor, mut rx) = Supervisor::new(connection);
    let supervisor = Arc::new(supervisor);
    let connect = supervisor.connect_fn();

    connect().await.expect("initial connect failed");
    assert_eq!(supervisor.state().await, TransportState::Connected);
    supervisor
        .write(b"echo 'marker-1'\n")
        .await
        .expect("write failed");
    collect_echo(&mut rx, "marker-1", Duration::from_secs(10)).await;

    supervisor.disconnect().await.expect("disconnect failed");
    assert_eq!(supervisor.state().await, TransportState::Disconnected);

    // Writing to the dropped transport fails rather than silently
    // succeeding — the condition that triggers a reconnect.
    assert!(supervisor.write(b"echo 'marker-2'\n").await.is_err());

    // And a fresh connect through the same supervisor works again. The
    // receiver must be swapped *before* connecting: each transport captures
    // the sender at construction time.
    let mut rx = supervisor.next_event_rx().await;
    connect().await.expect("reconnect failed");
    assert_eq!(supervisor.state().await, TransportState::Connected);
    supervisor
        .write(b"echo 'marker-3'\n")
        .await
        .expect("write after reconnect failed");
    collect_echo(&mut rx, "marker-3", Duration::from_secs(10)).await;

    supervisor
        .disconnect()
        .await
        .expect("final disconnect failed");
}

/// Reconnect preserves the PTY size, so a reattached tmux session comes
/// back with the right dimensions (doc 04 §Tmux Session Persistence: the
/// reconnect restores full state).
#[tokio::test]
async fn reconnected_transport_keeps_its_pty_size() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = authenticated_connection(&f).await;
    let (mut raw_supervisor, mut rx) = Supervisor::new(connection);
    raw_supervisor.rows = 42;
    raw_supervisor.cols = 137;
    let supervisor = Arc::new(raw_supervisor);
    let connect = supervisor.connect_fn();

    connect().await.expect("initial connect failed");
    supervisor
        .write(b"stty size\n")
        .await
        .expect("write failed");
    collect_echo(&mut rx, "42 137", Duration::from_secs(10)).await;

    supervisor.disconnect().await.expect("disconnect failed");

    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = mpsc::unbounded_channel();
    let mut rx = supervisor.next_event_rx().await;
    assert!(driver
        .run(connect.clone(), &events, None, None)
        .await
        .is_connected());

    supervisor
        .write(b"stty size\n")
        .await
        .expect("write after reconnect failed");
    collect_echo(&mut rx, "42 137", Duration::from_secs(10)).await;

    supervisor
        .disconnect()
        .await
        .expect("final disconnect failed");
}
