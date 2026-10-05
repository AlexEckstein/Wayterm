//! Integration tests against a real sshd.
//!
//! These tests are skipped unless the environment variables are set:
//!   SSH_TEST_HOST, SSH_TEST_PORT, SSH_TEST_USER, SSH_TEST_PASSWORD,
//!   SSH_TEST_KEY_PATH
//!
//! CI is expected to spin up a local sshd container with a known user/key.
//! Locally (without those variables) the tests print a skip message and return.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Config, Handle};
use tokio::sync::mpsc;
use tokio::time::timeout;

mod app_support {
    pub use wayterm::store::model::HostKeyPolicy;
    pub use wayterm::transport::connection::{
        authenticate, AppClientHandler, AuthMethod, SshConnection,
    };
    pub use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
    pub use wayterm::transport::types::{Transport, TransportEvent, TransportState};
    pub use wayterm::transport::SSHTransport;
}
use app_support::*;

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    key_path: String,
}

fn read_fixture() -> Option<Fixture> {
    let host = std::env::var("SSH_TEST_HOST").ok()?;
    let port = std::env::var("SSH_TEST_PORT").ok()?.parse().ok()?;
    let user = std::env::var("SSH_TEST_USER").ok()?;
    let password = std::env::var("SSH_TEST_PASSWORD").ok()?;
    let key_path = std::env::var("SSH_TEST_KEY_PATH").ok()?;
    Some(Fixture {
        host,
        port,
        user,
        password,
        key_path,
    })
}

fn skip() {
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

async fn connect_authenticated(method: AuthMethod) -> Arc<SshConnection> {
    let f = read_fixture().expect("fixture");
    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    authenticate(&mut handle, &f.user, &method)
        .await
        .expect("authentication failed");
    Arc::new(SshConnection::new(handle, f.host, f.port, f.user))
}

fn ssh_transport(connection: Arc<SshConnection>) -> (SSHTransport, mpsc::Receiver<TransportEvent>) {
    let (tx, rx) = mpsc::channel(64);
    (SSHTransport::new(connection, tx, 24, 80), rx)
}

async fn collect_echo(rx: &mut mpsc::Receiver<TransportEvent>, needle: &str, deadline: Duration) {
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

#[tokio::test]
async fn password_auth_exec_echo() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection =
        connect_authenticated(AuthMethod::Password(f.password.into_bytes().into())).await;
    let (mut transport, mut rx) = ssh_transport(connection);

    // Request exec instead of shell by abusing the shared channel. For this test
    // we enable tmux with a command that echoes and exits — the public trait
    // does not expose exec, so we close the shell and rely on the raw channel
    // not being reused. Simpler: write an echo command and read its output.
    transport.connect().await.expect("connect failed");
    let cmd = b"echo 'wayterm-password-echo'\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(&mut rx, "wayterm-password-echo", Duration::from_secs(10)).await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn key_auth_exec_echo() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = connect_authenticated(AuthMethod::Key {
        path: f.key_path,
        passphrase: None,
    })
    .await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    let cmd = b"echo 'wayterm-key-echo'\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(&mut rx, "wayterm-key-echo", Duration::from_secs(10)).await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn resize_propagation_reaches_remote_pty() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection =
        connect_authenticated(AuthMethod::Password(f.password.into_bytes().into())).await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    // Resize to a distinctive size and query it via stty.
    transport.resize(42, 137).await.expect("resize failed");

    let cmd = b"stty size\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(&mut rx, "42 137", Duration::from_secs(10)).await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn disconnect_is_idempotent() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection =
        connect_authenticated(AuthMethod::Password(f.password.into_bytes().into())).await;
    let (mut transport, _rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    transport.disconnect().await.expect("disconnect failed");
    transport
        .disconnect()
        .await
        .expect("second disconnect failed");
    assert_eq!(transport.state(), TransportState::Disconnected);
}

#[tokio::test]
async fn reader_pushes_data_received_events() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection =
        connect_authenticated(AuthMethod::Password(f.password.into_bytes().into())).await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    transport
        .write(b"echo 'data-event-marker'\n")
        .await
        .expect("write failed");

    let saw_data = timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(TransportEvent::DataReceived(bytes)) => {
                    if String::from_utf8_lossy(&bytes).contains("data-event-marker") {
                        return true;
                    }
                }
                Some(TransportEvent::ConnectionLost) => return false,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(saw_data, "reader task must push DataReceived events");
    transport.disconnect().await.expect("disconnect failed");
}
