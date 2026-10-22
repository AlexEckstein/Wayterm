//! The connect flow's supervisor semantics against a real sshd (see
//! `common`): one shared connection per identity, kept alive by any
//! consumer, released after the last.

mod common;

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use common::*;
use wayterm::sftp::{commands::DirEntry, SftpClient, SftpCommand};
use wayterm::store::model::{HostKeyPolicy, SessionConfig, SessionType};
use wayterm::transport::connection::SshConnection;
use wayterm::transport::factory::create_transport;
use wayterm::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};
use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth};
use wayterm::transport::types::TransportError;

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

/// Wait until every strong reference is gone (the release monitor polls
/// every 250 ms).
async fn assert_released(weak: &Weak<SshConnection>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "connection not released within 10s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Doc 04 §Connection supervisor: one dial per key, reuse while a consumer
/// holds the connection, disconnect + eviction once the last consumer drops
/// it, and a fresh dial afterwards.
#[tokio::test]
async fn supervisor_reuses_dial_and_releases_when_idle() {
    let Some(f) = fixture() else { return };

    let supervisor = ConnectionSupervisor::new();
    let params = connect_params(&f);

    let first = supervisor
        .get_or_connect(&params, &password_auth(&f.password))
        .await
        .expect("first connect failed");
    let second = supervisor
        .get_or_connect(&params, &password_auth(&f.password))
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
        .get_or_connect(&params, &password_auth(&f.password))
        .await
        .expect("re-connect after release failed");
    let fresh_weak = Arc::downgrade(&fresh);
    drop(fresh);
    assert_released(&fresh_weak).await;
}

/// The connect flow end to end: transport over the supervisor connection, tab
/// close (transport disconnect) does NOT kill the shared session, the SFTP
/// sidebar keeps working on it, and the supervisor disconnects only after
/// the sidebar releases it too (doc 03 + doc 07 §Connection model).
#[tokio::test]
async fn shared_connection_survives_tab_close_for_sftp() {
    let Some(f) = fixture() else { return };

    let supervisor = ConnectionSupervisor::new();
    let params = connect_params(&f);
    let conn = supervisor
        .get_or_connect(&params, &password_auth(&f.password))
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
    collect_echo(&mut event_rx, "wayterm-connect-flow-echo").await;

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
/// flow maps that onto the error view, never a panic.
#[tokio::test]
async fn strict_policy_reject_surfaces_host_key_rejected() {
    let Some(f) = fixture() else { return };

    let supervisor = ConnectionSupervisor::new();
    let params = connect_params(&f);
    // Explicit, empty known_hosts file so "unknown host → prompt" is
    // deterministic regardless of the host machine's `~/.ssh/known_hosts`.
    let dir = tempfile::tempdir().expect("tempdir for known_hosts");
    let known_hosts: Arc<Path> = Arc::from(dir.path().join("known_hosts").as_path());
    let auth = HopAuth {
        verifier: HostKeyVerifier::new(
            HostKeyPolicy::Strict,
            Arc::new(|_: &HostKeyFacts| HostKeyDecision::Reject),
            Some(known_hosts),
        ),
        responder: None,
        ..password_auth(&f.password)
    };

    let result = supervisor.get_or_connect(&params, &auth).await;

    match result {
        Err(TransportError::HostKeyRejected(host)) => assert_eq!(host, f.host),
        Err(other) => panic!("expected HostKeyRejected, got {other:?}"),
        Ok(_) => panic!("Strict policy with a Rejecting prompt must not connect"),
    }
}
