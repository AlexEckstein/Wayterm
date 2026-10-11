//! Jump dials against a real sshd (see `common` for the environment).
//!
//! Topology honesty: this rig has ONE sshd, so jump and target resolve to
//! the same server. That cannot express bastion isolation — what it DOES
//! prove is the real jump plumbing: direct-tcpip channel open on the jump
//! hop, `connect_stream` with full host-key verification plus per-hop auth
//! on the inner stream, and the depth/self-jump guards. The depth tests
//! need no server at all (the guard fires before any dial).

mod common;

use std::time::Duration;

use russh::ChannelMsg;
use tokio::time::timeout;
use wayterm::store::model::SessionConfig;

use common::*;
use wayterm::transport::connection::{AuthMethod, SshConnection};
use wayterm::transport::credentials::CredentialManager;
use wayterm::transport::jump::{
    check_jump_chain, dial_via_jump, dial_with_jump, hop_params, HopInputs, JumpTarget,
};
use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use wayterm::transport::types::TransportError;

fn jump_session(id: i64, f: &Fixture) -> SessionConfig {
    SessionConfig {
        id,
        name: "jump".to_string(),
        host: Some(f.host.clone()),
        port: Some(f.port),
        username: Some(f.user.clone()),
        ..SessionConfig::default()
    }
}

fn target_session(id: i64, jump_id: i64, f: &Fixture) -> SessionConfig {
    SessionConfig {
        id,
        name: "target".to_string(),
        host: Some(f.host.clone()),
        port: Some(f.port),
        username: Some(f.user.clone()),
        jump_session_id: Some(jump_id),
        ..SessionConfig::default()
    }
}

/// Run `cmd` on a fresh exec channel and collect output until `needle`.
async fn exec_collect(conn: &SshConnection, cmd: &[u8], needle: &str) -> String {
    let mut channel = conn
        .open_session_channel()
        .await
        .expect("inner session channel failed");
    channel.exec(true, cmd).await.expect("exec failed");
    let mut out = String::new();
    timeout(Duration::from_secs(15), async {
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    out.push_str(&String::from_utf8_lossy(&data));
                    if out.contains(needle) {
                        break;
                    }
                }
                ChannelMsg::ExitStatus { .. } | ChannelMsg::Eof if out.contains(needle) => {
                    break;
                }
                ChannelMsg::ExitStatus { .. } | ChannelMsg::Eof => {}
                _ => {}
            }
        }
    })
    .await
    .expect("exec timed out");
    out
}

#[tokio::test]
async fn jump_path_runs_exec_over_the_inner_channel() {
    let Some(f) = fixture() else { return };

    // Hop 1: direct dial to the "bastion".
    let jump_conn = connect_password(&f).await;
    let jump_params = ConnectParams {
        host: f.host.clone(),
        port: f.port,
        username: f.user.clone(),
    };

    // Hop 2: the SAME sshd as the "target", reached through direct-tcpip.
    // Host-key verification and auth run for real on the inner stream.
    let creds = password_creds(&f.password);
    let target = JumpTarget {
        params: ConnectParams {
            host: f.host.clone(),
            port: f.port,
            username: f.user.clone(),
        },
        method: AuthMethod::Password(String::new()),
        creds: &creds,
        verifier: accept_all_verifier(),
        responder: Some(password_responder(&f.password)),
        ssh_w_fallback: false,
    };

    let inner = dial_via_jump(&jump_conn, &jump_params, target)
        .await
        .expect("jump dial failed");
    let tag = unique_tag();
    let out = exec_collect(
        &inner,
        format!("echo wayterm-jump-{tag}\n").as_bytes(),
        &format!("wayterm-jump-{tag}"),
    )
    .await;
    assert!(
        out.contains(&format!("wayterm-jump-{tag}")),
        "inner exec must echo through the jump channel, got: {out:?}"
    );
    inner.disconnect().await.expect("inner disconnect failed");
}

#[tokio::test]
async fn jump_depth_greater_than_one_fails_before_dialing() {
    let supervisor = ConnectionSupervisor::new();
    let creds = CredentialManager::new(vec![]);
    let jump_auth = HopInputs {
        creds: &creds,
        verifier: accept_all_verifier(),
        responder: None,
    };
    let target_auth = HopInputs {
        creds: &creds,
        verifier: accept_all_verifier(),
        responder: None,
    };
    // The jump row itself jumps: depth 2. No fixture needed — the guard
    // fires before any network dial.
    let jump = SessionConfig {
        id: 1,
        name: "outer".to_string(),
        host: Some("127.0.0.1".to_string()),
        username: Some("u".to_string()),
        jump_session_id: Some(99),
        ..SessionConfig::default()
    };
    let target = SessionConfig {
        id: 2,
        name: "inner".to_string(),
        host: Some("127.0.0.1".to_string()),
        username: Some("u".to_string()),
        jump_session_id: Some(1),
        ..SessionConfig::default()
    };
    let err = dial_with_jump(&supervisor, &jump, jump_auth, &target, target_auth)
        .await
        .expect_err("depth > 1 must fail");
    assert!(
        matches!(err, TransportError::ConnectionFailed(ref msg) if msg.contains("depth")),
        "must name depth, got: {err}"
    );
}

#[tokio::test]
async fn jump_to_self_fails_before_dialing() {
    let supervisor = ConnectionSupervisor::new();
    let creds = CredentialManager::new(vec![]);
    let jump_auth = HopInputs {
        creds: &creds,
        verifier: accept_all_verifier(),
        responder: None,
    };
    let target_auth = HopInputs {
        creds: &creds,
        verifier: accept_all_verifier(),
        responder: None,
    };
    let row = SessionConfig {
        id: 7,
        name: "itself".to_string(),
        host: Some("127.0.0.1".to_string()),
        username: Some("u".to_string()),
        ..SessionConfig::default()
    };
    let err = dial_with_jump(&supervisor, &row, jump_auth, &row, target_auth)
        .await
        .expect_err("self-jump must fail");
    assert!(
        matches!(err, TransportError::ConnectionFailed(ref msg) if msg.contains("itself")),
        "must refuse self-jump, got: {err}"
    );
}

#[tokio::test]
async fn jump_chain_checks_are_wired_to_the_configs() {
    let Some(f) = fixture() else { return };
    // The same guards through `check_jump_chain` on realistic rows: a
    // legal chain passes, so the integration path above is genuinely
    // guarded rather than accidentally unguarded.
    let jump = jump_session(1, &f);
    let target = target_session(2, 1, &f);
    check_jump_chain(&jump, &target).expect("legal single hop must pass");
    assert!(hop_params(&target).is_ok());
}
