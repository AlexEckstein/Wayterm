use super::*;
use crate::store::model::HostKeyPolicy;
use crate::transport::hostkey::HostKeyDecision;

fn verifier() -> HostKeyVerifier {
    HostKeyVerifier::new(
        HostKeyPolicy::Off,
        Arc::new(|_| HostKeyDecision::Accept),
        None,
    )
}

fn session(id: i64, name: &str, jump: Option<i64>) -> SessionConfig {
    SessionConfig {
        id,
        name: name.to_string(),
        host: Some("example.test".to_string()),
        port: Some(22),
        username: Some("user".to_string()),
        jump_session_id: jump,
        ..SessionConfig::default()
    }
}

#[test]
fn chain_allows_a_single_hop() {
    let jump = session(1, "bastion", None);
    let target = SessionConfig {
        jump_session_id: Some(1),
        ..session(2, "inner", None)
    };
    check_jump_chain(&jump, &target).unwrap();
}

#[test]
fn chain_rejects_depth_greater_than_one() {
    let jump = SessionConfig {
        jump_session_id: Some(9),
        ..session(1, "bastion", None)
    };
    let target = session(2, "inner", None);
    let err = check_jump_chain(&jump, &target).unwrap_err();
    assert!(
        matches!(err, TransportError::ConnectionFailed(ref msg) if msg.contains("depth")),
        "must name depth, got: {err}"
    );
}

#[test]
fn chain_rejects_self_jump() {
    let row = session(1, "itself", None);
    let err = check_jump_chain(&row, &row).unwrap_err();
    assert!(
        matches!(err, TransportError::ConnectionFailed(ref msg) if msg.contains("itself")),
        "must refuse self-jump, got: {err}"
    );
}

#[test]
fn hop_params_defaults_to_port_22_and_rejects_blanks() {
    let mut cfg = session(1, "s", None);
    cfg.port = None;
    let params = hop_params(&cfg).unwrap();
    assert_eq!(params.port, 22);

    cfg.host = Some("   ".to_string());
    assert!(matches!(
        hop_params(&cfg),
        Err(TransportError::ConnectionFailed(_))
    ));
    cfg.host = Some("h".to_string());
    cfg.username = None;
    assert!(matches!(
        hop_params(&cfg),
        Err(TransportError::ConnectionFailed(_))
    ));
}

#[test]
fn hop_auth_method_mirrors_the_connect_flow_mapping() {
    let mut cfg = session(1, "s", None);
    assert!(matches!(hop_auth_method(&cfg), AuthMethod::Password(_)));
    cfg.auth_method = Some(StoredMethod::Key);
    cfg.key_path = Some("/keys/k".to_string());
    assert!(matches!(
        hop_auth_method(&cfg),
        AuthMethod::Key { path, .. } if path == "/keys/k"
    ));
    cfg.auth_method = Some(StoredMethod::Agent);
    assert!(matches!(hop_auth_method(&cfg), AuthMethod::Agent));
    cfg.auth_method = Some(StoredMethod::KeyboardInteractive);
    assert!(matches!(
        hop_auth_method(&cfg),
        AuthMethod::KeyboardInteractive
    ));
    cfg.auth_method = Some(StoredMethod::None);
    assert!(matches!(hop_auth_method(&cfg), AuthMethod::None));
}

#[test]
fn ssh_w_argv_carries_no_secrets() {
    let jump = ConnectParams {
        host: "bastion.test".to_string(),
        port: 2222,
        username: "jumpuser".to_string(),
    };
    let argv = ssh_w_argv(&jump, "inner.test", 2202);
    assert_eq!(
        argv,
        vec![
            "-W",
            "inner.test:2202",
            "-o",
            "BatchMode=yes",
            "-p",
            "2222",
            "jumpuser@bastion.test"
        ]
    );
    // Default port omits -p; either way no credential material appears.
    let default_port = ConnectParams {
        port: 22,
        ..jump.clone()
    };
    let argv = ssh_w_argv(&default_port, "inner.test", 22);
    assert_eq!(
        argv,
        vec![
            "-W",
            "inner.test:22",
            "-o",
            "BatchMode=yes",
            "jumpuser@bastion.test"
        ]
    );
    for secret in ["s3cr3t-password", "passphrase"] {
        assert!(
            !argv.iter().any(|a| a.contains(secret)),
            "secret material must never reach the child argv"
        );
    }
    // BatchMode=yes is positional after -W in both shapes above; assert
    // it outright so a reorder cannot silently drop the no-prompt guard.
    for argv in [
        ssh_w_argv(&jump, "inner.test", 2202),
        ssh_w_argv(&default_port, "inner.test", 22),
    ] {
        assert!(
            argv.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]),
            "ssh -W must never prompt on a tty Wayterm does not own: {argv:?}"
        );
    }
}

/// The `ssh -W` spawn path with the real `ssh` binary: spawning must
/// succeed (TCP-refused comes later, through the stream) and surface as
/// a clean `ConnectionFailed`, never a hang. Skips when `ssh` is absent.
#[tokio::test]
async fn ssh_w_spawn_path_against_a_closed_port() {
    if Command::new("ssh").arg("-V").output().await.is_err() {
        return println!("Skipping ssh -W spawn test: no ssh binary");
    }
    let jump = ConnectParams {
        host: "127.0.0.1".to_string(),
        port: 9,
        username: "nobody".to_string(),
    };
    // Port 9 on loopback is (almost) always closed: `ssh` exits
    // promptly with "Connection refused" on stderr (nulled here).
    let target = JumpTarget {
        params: ConnectParams {
            host: "inner.test".to_string(),
            port: 22,
            username: "user".to_string(),
        },
        method: AuthMethod::None,
        creds: &CredentialManager::new(vec![]),
        verifier: verifier(),
        responder: None,
        ssh_w_fallback: true,
    };
    let stream = SshWStream::spawn(&jump, &target.params.host, target.params.port)
        .await
        .expect("spawning ssh must succeed; the failure comes through the stream");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        connect_inner(stream, &target),
    )
    .await;
    match result {
        Err(_) => panic!("ssh -W to a closed port must fail fast, not hang"),
        Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("a closed port must not authenticate"),
    }
}
