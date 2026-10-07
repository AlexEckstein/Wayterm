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
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use wayterm::sftp::{commands::DirEntry, SftpClient, SftpCommand};

mod app_support {
    pub use wayterm::store::model::HostKeyPolicy;
    pub use wayterm::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
    pub use wayterm::transport::credentials::{CredentialManager, PromptSource};
    pub use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
    pub use wayterm::transport::ssh::SSHTransport;
    pub use wayterm::transport::types::{Transport, TransportEvent, TransportState};
}
use app_support::*;
use wayterm::transport::auth::{
    authenticate_chain, AuthConfig, AuthMethodKind, AuthOutcome, ChallengePrompt,
    ChallengeResponder,
};
use zeroize::Zeroizing;

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
        move |prompts: &[ChallengePrompt]| {
            Some(
                prompts
                    .iter()
                    .map(|p| {
                        if p.prompt.to_ascii_lowercase().contains("password") {
                            password.clone()
                        } else {
                            String::new()
                        }
                    })
                    .collect(),
            )
        }
    })
}

fn keyboard_interactive_responder(password: &str) -> ChallengeResponder {
    // CI sshd uses keyboard-interactive PAM; answer every prompt with the
    // test password. Some prompts are echoes, but the test password is
    // intentionally not secret in this context.
    Arc::new({
        let password = password.to_string();
        move |prompts: &[ChallengePrompt]| Some(vec![password.clone(); prompts.len()])
    })
}

async fn connect_authenticated(
    method: AuthMethod,
    creds: CredentialManager,
    responder: ChallengeResponder,
) -> Arc<SshConnection> {
    let f = read_fixture().expect("fixture");
    let handle = connect_raw(&f.host, f.port, &f.user).await;
    let connection = SshConnection::new(handle, f.host, f.port, f.user);
    connection
        .authenticate(&method, &creds, Some(responder))
        .await
        .expect("authentication failed");
    Arc::new(connection)
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

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
    let (mut transport, mut rx) = ssh_transport(connection);

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

    let connection = connect_authenticated(
        AuthMethod::Key {
            path: f.key_path,
            passphrase: None,
        },
        CredentialManager::new(vec![]),
        password_responder(&f.password),
    )
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

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
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

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
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

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
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

#[tokio::test]
async fn keyboard_interactive_auth_exec_echo() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = connect_authenticated(
        AuthMethod::KeyboardInteractive,
        CredentialManager::new(vec![]),
        keyboard_interactive_responder(&f.password),
    )
    .await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    let cmd = b"echo 'wayterm-kbd-interactive-echo'\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(
        &mut rx,
        "wayterm-kbd-interactive-echo",
        Duration::from_secs(10),
    )
    .await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn auth_failure_returns_failed_with_methods_remaining() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = AuthConfig {
        key_path: None,
        key_passphrase: None,
        password: Some(Zeroizing::new(b"definitely-wrong-password".to_vec())),
        try_agent: false,
        try_keyboard_interactive: false,
        challenge_responder: None,
    };

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("auth chain should run to completion");

    match outcome {
        AuthOutcome::Failed {
            reason,
            methods_remaining,
        } => {
            assert!(
                reason.to_ascii_lowercase().contains("password")
                    || reason.to_ascii_lowercase().contains("keyboard-interactive"),
                "failure reason should mention an auth method: {reason}"
            );
            assert!(
                methods_remaining.contains(&AuthMethodKind::KeyboardInteractive)
                    || methods_remaining.contains(&AuthMethodKind::Agent),
                "expected at least one untried method in remaining list: {:?}",
                methods_remaining
            );
        }
        other => panic!("expected AuthOutcome::Failed, got {other:?}"),
    }
}

// ---- SFTP integration (doc 09) -------------------------------------------------

fn unique_tag() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis()
        .to_string()
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
    timeout(Duration::from_secs(30), rx)
        .await
        .expect("sftp command timed out")
        .expect("sftp task ended before replying")
}

fn transfer_id(client: &SftpClient) -> u64 {
    client.next_transfer_id()
}

#[tokio::test]
async fn sftp_list_dir_returns_seeded_home() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
    let client = SftpClient::new(connection.clone());

    let entries: Vec<DirEntry> = sftp_cmd(&client, |reply| SftpCommand::Cd {
        path: format!("/home/{}", f.user),
        reply,
    })
    .await
    .expect("cd into home failed");

    assert!(!entries.is_empty(), "home listing must not be empty");
    assert!(
        entries.iter().any(|e| e.name == ".ssh" && e.is_dir),
        "seeded .ssh directory must be listed"
    );
    for pair in entries.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.is_dir != b.is_dir {
            assert!(a.is_dir, "directories must sort before files");
        } else {
            let (la, lb) = (a.name.to_ascii_lowercase(), b.name.to_ascii_lowercase());
            assert!(
                la <= lb,
                "entries of the same kind must sort case-insensitively"
            );
        }
    }

    drop(client);
    connection.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn sftp_upload_download_round_trip_reports_progress() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
    let client = SftpClient::new(connection.clone());
    let tag = unique_tag();

    let size = 100 * 1024u64;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let directory = tempfile::tempdir().expect("create tempdir");
    let local_upload = directory.path().join("upload.bin");
    let local_download = directory.path().join("download.bin");
    std::fs::write(&local_upload, &payload).expect("seed upload file");

    // Upload a 100 KiB payload and watch cumulative progress.
    let (reply_tx, reply_rx) = oneshot::channel();
    let (progress_tx, mut progress_rx) = mpsc::channel(32);
    let up_id = transfer_id(&client);
    let remote_upload = format!("/home/{}/wayterm-sftp-up-{tag}.bin", f.user);
    client
        .request(SftpCommand::Upload {
            transfer_id: up_id,
            local_path: local_upload.to_string_lossy().into_owned(),
            remote_path: remote_upload.clone(),
            progress: progress_tx,
            reply: reply_tx,
        })
        .await
        .expect("sftp task queue closed");
    let up_reply: Result<u64, String> = timeout(Duration::from_secs(60), reply_rx)
        .await
        .expect("upload timed out")
        .expect("upload task ended before replying");
    let mut up_events = Vec::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(5), progress_rx.recv()).await {
        up_events.push(event);
    }
    assert_eq!(up_reply.expect("upload failed"), size);
    let last = up_events.last().expect("upload must report progress");
    assert_eq!((last.transferred, last.total), (size, size));

    // Download it back and check byte-for-byte equality.
    let (reply_tx, reply_rx) = oneshot::channel();
    let (progress_tx, mut progress_rx) = mpsc::channel(32);
    let dl_id = transfer_id(&client);
    client
        .request(SftpCommand::Download {
            transfer_id: dl_id,
            remote_path: remote_upload.clone(),
            local_path: local_download.to_string_lossy().into_owned(),
            progress: progress_tx,
            reply: reply_tx,
        })
        .await
        .expect("sftp task queue closed");
    let dl_reply: Result<u64, String> = timeout(Duration::from_secs(60), reply_rx)
        .await
        .expect("download timed out")
        .expect("download task ended before replying");
    let mut dl_events = Vec::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(5), progress_rx.recv()).await {
        dl_events.push(event);
    }
    assert_eq!(dl_reply.expect("download failed"), size);
    let last = dl_events.last().expect("download must report progress");
    assert_eq!((last.transferred, last.total), (size, size));

    let downloaded = std::fs::read(&local_download).expect("read downloaded file");
    assert_eq!(
        downloaded, payload,
        "downloaded bytes must match uploaded bytes"
    );

    // Cancelling a finished transfer is a harmless no-op.
    client
        .request(SftpCommand::Cancel { transfer_id: dl_id })
        .await
        .expect("sftp task queue closed");

    sftp_cmd(&client, |reply| SftpCommand::Delete {
        path: remote_upload.clone(),
        reply,
    })
    .await
    .expect("cleanup delete failed");
    drop(client);
    connection.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn sftp_mkdir_rename_delete_round_trip() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
    let client = SftpClient::new(connection.clone());
    let tag = unique_tag();
    let base = format!("/home/{}", f.user);
    let dir_a = format!("{base}/wayterm-dir-{tag}");
    let dir_b = format!("{base}/wayterm-dir-renamed-{tag}");

    sftp_cmd(&client, |reply| SftpCommand::Mkdir {
        path: dir_a.clone(),
        reply,
    })
    .await
    .expect("mkdir failed");

    sftp_cmd(&client, |reply| SftpCommand::Rename {
        from: dir_a.clone(),
        to: dir_b.clone(),
        reply,
    })
    .await
    .expect("rename failed");

    let entries: Vec<DirEntry> = sftp_cmd(&client, |reply| SftpCommand::Cd {
        path: base.clone(),
        reply,
    })
    .await
    .expect("list home failed");
    assert!(
        entries
            .iter()
            .any(|e| e.name == format!("wayterm-dir-renamed-{tag}")),
        "renamed directory must be visible"
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.name == format!("wayterm-dir-{tag}")),
        "old name must no longer exist after rename"
    );

    sftp_cmd(&client, |reply| SftpCommand::Delete {
        path: dir_b.clone(),
        reply,
    })
    .await
    .expect("delete directory failed");

    let entries: Vec<DirEntry> = sftp_cmd(&client, |reply| SftpCommand::Cd { path: base, reply })
        .await
        .expect("list home failed");
    assert!(
        !entries
            .iter()
            .any(|e| e.name == format!("wayterm-dir-renamed-{tag}")),
        "deleted directory must be gone"
    );

    // Deleting a path that no longer exists must error, not hang.
    assert!(
        sftp_cmd(&client, |reply| SftpCommand::Delete { path: dir_b, reply })
            .await
            .is_err(),
        "deleting a missing path should error"
    );

    drop(client);
    connection.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn sftp_command_errors_after_connection_disconnect() {
    let Some(f) = read_fixture() else {
        skip();
        return;
    };

    let connection = connect_authenticated(
        AuthMethod::Password(f.password.clone()),
        password_cred_manager(&f.password),
        password_responder(&f.password),
    )
    .await;
    let client = SftpClient::new(connection.clone());

    let _baseline: Vec<DirEntry> = sftp_cmd(&client, |reply| SftpCommand::Cd {
        path: format!("/home/{}", f.user),
        reply,
    })
    .await
    .expect("baseline listing should succeed");

    connection.disconnect().await.expect("disconnect failed");

    let after: Result<Vec<DirEntry>, String> = sftp_cmd(&client, |reply| SftpCommand::Cd {
        path: "/".into(),
        reply,
    })
    .await;
    assert!(
        after.is_err(),
        "a command on the dead session must surface an error"
    );

    let reopened: Result<Vec<DirEntry>, String> = sftp_cmd(&client, |reply| SftpCommand::Cd {
        path: "/".into(),
        reply,
    })
    .await;
    assert!(
        reopened.is_err(),
        "a reattempt (fresh session attempt) must also error"
    );

    drop(client);
}
