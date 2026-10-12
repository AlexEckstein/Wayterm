use super::*;
use crate::store::model::ForwardKind;

#[test]
fn the_tab_title_prefers_the_terminal_and_never_goes_blank() {
    assert_eq!(tab_title("db-prod", "root@db-prod:~"), "root@db-prod:~");
    assert_eq!(tab_title("db-prod", ""), "db-prod");
    assert_eq!(tab_title("db-prod", "   \n"), "db-prod");
}

#[test]
fn status_endpoint_uses_the_description_only_without_a_host() {
    let ssh = SessionConfig {
        host: Some("db".into()),
        port: Some(2222),
        ..Default::default()
    };
    assert_eq!(status_endpoint(&ssh, "ignored"), ("db".into(), 2222));
    for session_type in [SessionType::LocalShell, SessionType::Serial] {
        let config = SessionConfig {
            session_type,
            ..Default::default()
        };
        assert_eq!(
            status_endpoint(&config, "/dev/ttyUSB0@115200"),
            ("/dev/ttyUSB0@115200".into(), 0)
        );
    }
}

#[test]
fn auth_methods_map_without_secrets() {
    let mut config = SessionConfig {
        key_path: Some("/k/id_ed25519".into()),
        ..Default::default()
    };
    for (stored, expected) in [
        (None, "Password(\"\")"),
        (Some(AuthMethod::Password), "Password(\"\")"),
        (
            Some(AuthMethod::Key),
            "Key { path: \"/k/id_ed25519\", passphrase: None }",
        ),
        (Some(AuthMethod::Agent), "Agent"),
        (Some(AuthMethod::KeyboardInteractive), "KeyboardInteractive"),
        (Some(AuthMethod::None), "None"),
    ] {
        config.auth_method = stored;
        assert_eq!(format!("{:?}", transport_auth_method(&config)), expected);
    }
}

#[test]
fn jump_dial_needs_ssh_with_a_jump_host() {
    let ssh_jumped = SessionConfig {
        session_type: SessionType::SSH,
        jump_session_id: Some(7),
        ..Default::default()
    };
    assert!(
        uses_jump_dial(&ssh_jumped),
        "an SSH session with a jump host dials through the bastion"
    );
    assert!(
        !uses_jump_dial(&SessionConfig {
            jump_session_id: None,
            ..ssh_jumped.clone()
        }),
        "a direct SSH session keeps the supervisor dial"
    );
    for session_type in [
        SessionType::RDP,
        SessionType::VNC,
        SessionType::Serial,
        SessionType::LocalShell,
    ] {
        assert!(
            !uses_jump_dial(&SessionConfig {
                session_type,
                jump_session_id: Some(7),
                ..Default::default()
            }),
            "only SSH reaches the jump branch (other types return earlier)"
        );
    }
}

#[test]
fn forward_engine_arms_the_sessions_current_rules() {
    assert!(
        forward_rules_for(&SessionConfig::default()).is_empty(),
        "no stored rules is an empty engine set, not a missing one"
    );
    let rules = vec![ForwardRule {
        kind: ForwardKind::Local,
        bind_addr: "127.0.0.1".to_string(),
        bind_port: 8080,
        remote_host: "web1.lan".to_string(),
        remote_port: 80,
        enabled: true,
    }];
    let session = SessionConfig {
        forward_rules: Some(rules.clone()),
        ..Default::default()
    };
    assert_eq!(forward_rules_for(&session), rules);
}

#[test]
fn jump_resolution_reports_a_deleted_jump_host() {
    let store = SessionStore::in_memory().expect("in-memory store");
    let jump_id = store
        .insert_session(&SessionConfig {
            name: "bastion".to_string(),
            host: Some("bastion.test".to_string()),
            username: Some("jump".to_string()),
            ..Default::default()
        })
        .expect("insert jump host");
    let resolved = resolve_jump_session(&store, jump_id).expect("jump host exists");
    assert_eq!(resolved.name, "bastion");

    // A deleted jump host is a connect error naming the loss, never a
    // dial to the wrong place.
    store.delete_session(jump_id).expect("delete jump host");
    let err = resolve_jump_session(&store, jump_id).unwrap_err();
    assert!(
        err.contains("no longer exists"),
        "must name the loss, got: {err}"
    );
    assert!(err.contains(&jump_id.to_string()), "must name the id");
}
