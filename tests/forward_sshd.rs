//! Local forwards against a real sshd (see `common` for the environment).
//!
//! Topology honesty: this rig has ONE sshd, so the "service on the SSH
//! host's loopback" is a throwaway echo server bound by the test process
//! itself — same host the CI sshd runs on, reached through the tunnel as
//! `127.0.0.1:<port>`. That proves the full path (listener →
//! direct-tcpip channel → bidirectional copy → sshd TCP forward to the
//! host's loopback), but it CANNOT express network isolation: a real
//! bastion test would put the service on an address unreachable except
//! through the jump host.

mod common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use wayterm::store::model::{ForwardKind, ForwardRule};

use common::*;
use wayterm::transport::forward::{ForwardEngine, ForwardEvent, ForwardState};

const WAIT: Duration = Duration::from_secs(10);

fn local_rule(bind_port: u16, remote_host: &str, remote_port: u16) -> ForwardRule {
    ForwardRule {
        kind: ForwardKind::Local,
        bind_addr: "127.0.0.1".to_string(),
        bind_port,
        remote_host: remote_host.to_string(),
        remote_port,
        enabled: true,
    }
}

/// A free loopback port (bind `:0`, read the port, release it).
async fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Throwaway echo service on the test host's loopback: accept and play
/// back everything, like a banner-less service behind the bastion.
async fn echo_service() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (mut read, mut write) = sock.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    port
}

async fn next_event(rx: &mut mpsc::Receiver<ForwardEvent>) -> ForwardEvent {
    timeout(WAIT, rx.recv())
        .await
        .expect("timed out waiting for a forward event")
        .expect("event channel closed")
}

#[tokio::test]
async fn local_forward_roundtrip_through_the_tunnel() {
    let Some(f) = fixture() else { return };

    let service = echo_service().await;
    let bind = free_port().await;
    let connection = connect_password(&f).await;
    let (tx, mut rx) = mpsc::channel(64);
    let engine = ForwardEngine::new(connection, tx);
    engine
        .start(vec![local_rule(bind, "127.0.0.1", service)])
        .await;
    assert!(
        matches!(next_event(&mut rx).await.state, ForwardState::Listening),
        "engine must report Listening"
    );

    // Through the tunnel to the host-loopback echo service and back.
    let tag = format!("wayterm-tunnel-{}\n", unique_tag());
    let mut sock = timeout(WAIT, TcpStream::connect(("127.0.0.1", bind)))
        .await
        .expect("tunnel connect timed out")
        .expect("tunnel listener refused");
    timeout(WAIT, sock.write_all(tag.as_bytes()))
        .await
        .expect("tunnel write timed out")
        .expect("tunnel write failed");
    let mut out = vec![0u8; tag.len()];
    timeout(WAIT, sock.read_exact(&mut out))
        .await
        .expect("tunnel echo timed out")
        .expect("tunnel echo failed");
    assert_eq!(out, tag.as_bytes(), "bidirectional copy must round-trip");

    engine.stop().await;
}

#[tokio::test]
async fn refused_remote_closes_cleanly_and_stop_closes_the_listener() {
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
    let (tx, mut rx) = mpsc::channel(64);
    let engine = ForwardEngine::new(connection, tx);
    let live = free_port().await;
    let dead_remote = free_port().await; // nothing listens here
    let service = echo_service().await;
    engine
        .start(vec![
            local_rule(live, "127.0.0.1", service),
            local_rule(dead_remote, "127.0.0.1", free_port().await),
        ])
        .await;
    // Rule 0 listens; rule 1's bind is its own listener (the refusal
    // happens per-connection, when the channel open fails).
    assert_eq!(next_event(&mut rx).await.rule_id, 0);
    assert_eq!(next_event(&mut rx).await.rule_id, 1);

    // Refused remote: the tunnel accepts, then closes our socket cleanly.
    let mut sock = TcpStream::connect(("127.0.0.1", dead_remote))
        .await
        .expect("refused-rule listener must still accept");
    let mut buf = [0u8; 16];
    let n = timeout(WAIT, sock.read(&mut buf))
        .await
        .expect("refused close timed out")
        .expect("refused read failed");
    assert_eq!(n, 0, "refused remote must EOF the accepted socket");
    let failed = next_event(&mut rx).await;
    assert!(
        matches!(failed.state, ForwardState::LastFailed { .. }),
        "refused remote must report LastFailed, got: {failed:?}"
    );

    engine.stop().await;
    assert!(
        TcpStream::connect(("127.0.0.1", live)).await.is_err(),
        "stop must close the listener"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", dead_remote))
            .await
            .is_err(),
        "stop must close every listener"
    );
}
