//! Integration tests for the P3-3 connect flow against a real sshd.
//!
//! Skipped unless the environment variables are set:
//!   SSH_TEST_HOST, SSH_TEST_PORT, SSH_TEST_USER, SSH_TEST_PASSWORD,
//!   SSH_TEST_KEY_PATH
//!
//! Run through `scripts/run-sshd-tests.sh`, which provisions a throwaway
//! sshd and exports the fixture (see its TEST_CMD). The tests focus on the
//! supervisor + shared-connection semantics; channel/PTY behaviour itself is
//! covered by `transport_sshd.rs`.

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use wayterm::sftp::{commands::DirEntry, SftpClient, SftpCommand};
use wayterm::store::model::{HostKeyPolicy, SessionConfig, SessionType};
use wayterm::transport::auth::ChallengeResponder;
use wayterm::transport::connection::{AuthMethod, SshConnection};
use wayterm::transport::credentials::{CredentialManager, PromptSource};
use wayterm::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};
use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use wayterm::transport::tab::create_transport;
use wayterm::transport::types::{TransportError, TransportEvent};

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
    // Keep the exact wording `scripts/run-sshd-tests.sh` greps for.
    println!(
        "Skipping sshd integration test: set SSH_TEST_HOST, SSH_TEST_PORT, \
         SSH_TEST_USER, SSH_TEST_PASSWORD, and SSH_TEST_KEY_PATH to run it."
    );
}

fn accept_all_verifier() -> HostKeyVerifier {
    HostKeyVerifier::new(
        HostKeyPolicy::Off,
        Arc::new(|_| HostKeyDecision::Accept),
        None,
    )
}

fn password_cred_manager(password: &str) -> CredentialManager {
    let prompt_source = PromptSource::new(Arc::new({
        let password = password.to_string();
        move |_session_id, secret_type| match secret_type {
            "password" => Some(zeroize::Zeroizing::new(password.clone().into_bytes())),
            _ => None,
        }
    }));
    CredentialManager::new(vec![Box::new(prompt_source)])
}

fn password_responder(password: &str) -> ChallengeResponder {
    Arc::new({
        let password = password.to_string();
        move |prompts: &[wayterm::transport::auth::ChallengePrompt]| {
            Some(prompts.iter().map(|_| password.clone()).collect::<Vec<_>>())
        }
    })
}

fn connect_params(f: &Fixture) -> ConnectParams {
    ConnectParams {
        host: f.host.clone(),
        port: f.port,
        username: f.user.clone(),
    }
}

fn ssh_session(f: &Fixture) -> SessionConfig {
    SessionConfig {
        session_type: SessionType::SSH,
        host: Some(f.host.clone()),
        port: Some(f.port),
        username: Some(f.user.clone()),
        ..Default::default()
    }
}

async fn collect_echo(rx: &mut mpsc::Receiver<TransportEvent>, needle: &str, deadline: Duration) {
    let result = tokio::time::timeout(deadline, async {
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
                    panic!("connection lost before seeing '{}'", needle)
                }
                Some(_) => {}
                None => panic!("event channel closed before seeing '{}'", needle),
            }
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("timed out waiting for '{}'", needle));
}

async fn sftp_cmd<T>(
    client: &SftpClient,
    make: impl FnOnce(oneshot::Sender<Result<T, String>>) -> SftpCommand,
) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    client
        .request(make(tx))
        .await
        .expect("sftp task queue closed");
    tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .expect("sftp command timed out")
        .expect("sftp task ended before replying")
}

/// Poll until every strong reference to the released connection is gone.
///
/// The supervisor's release monitor polls every 250 ms (non-test build, which
/// is what integration tests see); a generous deadline asserts the semantics
/// (release happens), not the scheduling.
async fn assert_released(weak: &Weak<SshConnection>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if weak.upgrade().is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("connection was not released within 10s of its last consumer dropping");
}

/// Doc 04 §Connection Supervisor: one dial per key, reuse while a consumer
/// holds the connection, disconnect + eviction once the last consumer drops
/// it, and a fresh dial afterwards.
#[tokio::test]
async fn supervisor_reuses_dial_and_releases_when_idle() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let supervisor = ConnectionSupervisor::new();
    let params = connect_params(&f);
    let method = AuthMethod::Password(f.password.clone());

    let first = supervisor
        .get_or_connect(
            &params,
            &method,
            &password_cred_manager(&f.password),
            accept_all_verifier(),
            Some(password_responder(&f.password)),
        )
        .await
        .expect("first connect failed");
    let second = supervisor
        .get_or_connect(
            &params,
            &method,
            &password_cred_manager(&f.password),
            accept_all_verifier(),
            Some(password_responder(&f.password)),
        )
        .await
        .expect("second connect failed");
    assert!(
        Arc::ptr_eq(&first, &second),
        "same (host, port, username) must reuse the pooled connection"
    );

    // Last consumers gone → the monitor disconnects and evicts the entry.
    let weak = Arc::downgrade(&first);
    drop(first);
    drop(second);
    assert_released(&weak).await;

    // The pool entry is gone, so this must dial a brand-new session.
    let fresh = supervisor
        .get_or_connect(
            &params,
            &method,
            &password_cred_manager(&f.password),
            accept_all_verifier(),
            None,
        )
        .await
        .expect("re-connect after release failed");
    let fresh_weak = Arc::downgrade(&fresh);
    drop(fresh);
    assert_released(&fresh_weak).await;
}

/// The P3-3 end-to-end shape: transport over the supervisor connection, tab
/// close (transport disconnect) does NOT kill the shared session, the SFTP
/// sidebar keeps working on it, and the supervisor disconnects only after
/// the sidebar releases it too (doc 04 + doc 09 §Connection Model).
#[tokio::test]
async fn shared_connection_survives_tab_close_for_sftp() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let supervisor = ConnectionSupervisor::new();
    let params = connect_params(&f);
    let conn = supervisor
        .get_or_connect(
            &params,
            &AuthMethod::Password(f.password.clone()),
            &password_cred_manager(&f.password),
            accept_all_verifier(),
            Some(password_responder(&f.password)),
        )
        .await
        .expect("supervisor connect failed");

    // Build the tab's transport over the shared connection.
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let mut transport = create_transport(&ssh_session(&f), Some(conn.clone()), event_tx, 24, 80)
        .expect("transport");
    transport.connect().await.expect("transport connect failed");
    transport
        .write(b"echo 'wayterm-connect-flow-echo'\n")
        .await
        .expect("write failed");
    collect_echo(
        &mut event_rx,
        "wayterm-connect-flow-echo",
        Duration::from_secs(10),
    )
    .await;

    // Tab close: the transport disconnects its channel — but the SshConnection
    // belongs to the supervisor and must survive for the sidebar.
    transport.disconnect().await.expect("disconnect failed");
    drop(transport);

    let client = SftpClient::new(conn.clone());
    let entries: Vec<DirEntry> = sftp_cmd(&client, |reply| SftpCommand::Cd {
        path: format!("/home/{}", f.user),
        reply,
    })
    .await
    .expect("sftp must keep working on the shared session after tab close");
    assert!(
        entries.iter().any(|e| e.name == ".ssh" && e.is_dir),
        "seeded home listing over the shared connection"
    );

    // Sidebar dropped (page closed) → last consumer gone → supervisor
    // disconnects the session.
    drop(client);
    let weak = Arc::downgrade(&conn);
    drop(conn);
    assert_released(&weak).await;
}

/// Host-key rejection under Strict policy surfaces as
/// [`TransportError::HostKeyRejected`] from the supervisor — the connect
/// flow maps that onto the error view (P3-3 3), never a panic.
#[tokio::test]
async fn strict_policy_reject_surfaces_host_key_rejected() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let supervisor = ConnectionSupervisor::new();
    let params = connect_params(&f);
    // Explicit, empty known_hosts file so "unknown host → prompt" is
    // deterministic regardless of the host machine's `~/.ssh/known_hosts`.
    let dir = tempfile::tempdir().expect("tempdir for known_hosts");
    let known_hosts: Arc<Path> = Arc::from(dir.path().join("known_hosts").as_path());
    let reject_verifier = HostKeyVerifier::new(
        HostKeyPolicy::Strict,
        Arc::new(|_: &HostKeyFacts| HostKeyDecision::Reject),
        Some(known_hosts),
    );

    let result = supervisor
        .get_or_connect(
            &params,
            &AuthMethod::Password(f.password.clone()),
            &password_cred_manager(&f.password),
            reject_verifier,
            None,
        )
        .await;

    match result {
        Err(TransportError::HostKeyRejected(host)) => assert_eq!(host, f.host),
        Err(other) => panic!("expected HostKeyRejected, got {other:?}"),
        Ok(_) => panic!("Strict policy with a Rejecting prompt must not connect"),
    }
}
