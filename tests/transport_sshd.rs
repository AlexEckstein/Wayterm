//! The SSH transport, auth chain, and SFTP task against a real sshd (see
//! `common` for the environment; CI runs this suite).

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use zeroize::Zeroizing;

use common::*;
use wayterm::sftp::{commands::DirEntry, SftpClient, SftpCommand};
use wayterm::transport::auth::{authenticate_chain, AuthConfig, AuthMethodKind, AuthOutcome};
use wayterm::transport::connection::{AuthMethod, SshConnection};
use wayterm::transport::credentials::CredentialManager;
use wayterm::transport::ssh::SSHTransport;
use wayterm::transport::types::{Transport, TransportEvent, TransportState};

fn ssh_transport(connection: Arc<SshConnection>) -> (SSHTransport, mpsc::Receiver<TransportEvent>) {
    let (tx, rx) = mpsc::channel(64);
    (SSHTransport::new(connection, tx, 24, 80), rx)
}

#[tokio::test]
async fn password_auth_exec_echo() {
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    let cmd = b"echo 'wayterm-password-echo'\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(&mut rx, "wayterm-password-echo").await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn key_auth_exec_echo() {
    let Some(f) = fixture() else { return };

    let Some(key_path) = f.key_path.clone() else {
        return println!("Skipping key auth: SSH_TEST_KEY_PATH is not set");
    };
    let connection = connect_with(
        &f,
        AuthMethod::Key {
            path: key_path,
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

    collect_echo(&mut rx, "wayterm-key-echo").await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn resize_propagation_reaches_remote_pty() {
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    // Resize to a distinctive size and query it via stty.
    transport.resize(42, 137).await.expect("resize failed");

    let cmd = b"stty size\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(&mut rx, "42 137").await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn disconnect_is_idempotent() {
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
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
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
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
    let Some(f) = fixture() else { return };

    let connection = connect_with(
        &f,
        AuthMethod::KeyboardInteractive,
        CredentialManager::new(vec![]),
        answer_all(&f.password),
    )
    .await;
    let (mut transport, mut rx) = ssh_transport(connection);

    transport.connect().await.expect("connect failed");
    let cmd = b"echo 'wayterm-kbd-interactive-echo'\n";
    transport.write(cmd).await.expect("write failed");

    collect_echo(&mut rx, "wayterm-kbd-interactive-echo").await;
    transport.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
async fn auth_failure_returns_failed_with_methods_remaining() {
    let Some(f) = fixture() else { return };

    let mut handle = connect_raw(&f).await;
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

// ---- SFTP (doc 09) ----

#[tokio::test]
async fn sftp_list_dir_returns_seeded_home() {
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
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
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
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
    let up_id = client.next_transfer_id();
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
    let dl_id = client.next_transfer_id();
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
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
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
    let Some(f) = fixture() else { return };

    let connection = connect_password(&f).await;
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
