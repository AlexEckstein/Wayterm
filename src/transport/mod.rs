//! SSH transport and the generic [`Transport`] trait (doc 04).
//!
//! v1 implements only the SSH byte-stream transport; local PTY and serial
//! transports are intentionally left as structural placeholders so the
//! module tree matches the target layout.

use tokio::sync::mpsc;

use crate::transport::credentials::CredentialSource;

pub mod auth;
pub mod connection;
pub mod credentials;
pub mod hostkey;
pub mod ssh;
pub mod supervisor;
pub mod tab;
pub mod types;

/// No-op that exercises the public transport API so the Phase 2 seam stays
/// compiled and lint-clean while the GTK connection dialog is wired in Phase
/// 3. This function does no I/O and is removed once the UI calls these items
/// directly.
pub fn touch_public_api() {
    use std::sync::Arc;

    // types
    let (tx, _rx) = mpsc::channel(8);
    let (state_tx, _state_rx) = mpsc::channel(1);
    for t in [
        types::TransportType::SSH,
        types::TransportType::LocalPTY,
        types::TransportType::Serial,
    ] {
        let _ = t.to_string();
    }
    for s in [
        types::TransportState::Disconnected,
        types::TransportState::Connecting,
        types::TransportState::VerifyingHostKey,
        types::TransportState::Authenticating,
        types::TransportState::Connected,
        types::TransportState::Reconnecting { attempt: 0 },
        types::TransportState::Error(String::new()),
    ] {
        s.emit(&tx);
    }
    for e in [
        types::TransportEvent::DataReceived(Vec::new()),
        types::TransportEvent::StateChanged(types::TransportState::Disconnected),
        types::TransportEvent::Error(String::new()),
        types::TransportEvent::ConnectionLost,
    ] {
        match e {
            types::TransportEvent::DataReceived(d) => {
                let _ = d.len();
            }
            types::TransportEvent::StateChanged(s) => {
                let _ = s.clone();
            }
            types::TransportEvent::Error(msg) => {
                let _ = msg.len();
            }
            types::TransportEvent::ConnectionLost => {}
        }
    }
    for err in [
        types::TransportError::ConnectionFailed(String::new()),
        types::TransportError::HostKeyRejected(String::new()),
        types::TransportError::AuthFailed(String::new()),
        types::TransportError::ChannelError(String::new()),
        types::TransportError::Disconnected,
        types::TransportError::Cancelled,
    ] {
        match err {
            types::TransportError::ConnectionFailed(s)
            | types::TransportError::HostKeyRejected(s)
            | types::TransportError::AuthFailed(s)
            | types::TransportError::ChannelError(s) => {
                let _ = s.len();
            }
            types::TransportError::IoError(_) => {}
            types::TransportError::Disconnected | types::TransportError::Cancelled => {}
        }
    }
    let _ = types::TransportError::from_russh(russh::Error::SendError).to_string();
    let _: Option<Box<dyn types::Transport>> = None;

    // tab wiring (TerminalTab + transport factory)
    let _ = tab::create_transport;
    let _ = tab::TerminalTab::new;
    let _ = tab::TerminalTab::close;
    let _ = tab::TerminalTab::backend;
    let _ = tab::TerminalTab::take_state_rx;

    // ssh transport constructor + trait methods
    let _ = ssh::SSHTransport::new;
    let _ = <ssh::SSHTransport as types::Transport>::connect;
    let _ = <ssh::SSHTransport as types::Transport>::disconnect;
    let _ = <ssh::SSHTransport as types::Transport>::state;
    let _ = <ssh::SSHTransport as types::Transport>::write;
    let _ = <ssh::SSHTransport as types::Transport>::resize;
    let _ = <ssh::SSHTransport as types::Transport>::set_tmux_enabled;
    let _ = <ssh::SSHTransport as types::Transport>::transport_type;
    let _ = <ssh::SSHTransport as types::Transport>::description;

    // connection + auth
    let _ = connection::SshConnection::new;
    let _ = connection::SshConnection::host;
    let _ = connection::SshConnection::port;
    let _ = connection::SshConnection::username;
    let _ = connection::SshConnection::open_session_channel;
    let _ = connection::SshConnection::open_direct_tcpip;
    let _ = connection::SshConnection::open_sftp_channel;
    let _ = connection::SshConnection::disconnect;
    let _ = connection::SshConnection::authenticate;
    let _ = connection::auth_config;
    let _ = connection::AppClientHandler::new;
    for m in [
        connection::AuthMethod::Password("p".into()),
        connection::AuthMethod::Key {
            path: "k".into(),
            passphrase: None,
        },
        connection::AuthMethod::Agent,
        connection::AuthMethod::KeyboardInteractive,
        connection::AuthMethod::None,
    ] {
        let _ = format!("{m:?}");
        match m {
            connection::AuthMethod::Password(s) => {
                let _ = s.len();
            }
            connection::AuthMethod::Key { path, passphrase } => {
                let _ = path.len();
                let _ = passphrase.is_none();
            }
            connection::AuthMethod::Agent
            | connection::AuthMethod::KeyboardInteractive
            | connection::AuthMethod::None => {}
        }
    }

    // connection supervisor (doc 04 §Connection Supervisor)
    let _ = supervisor::ConnectionSupervisor::<connection::SshConnection>::new;
    let _ = supervisor::ConnectionSupervisor::<connection::SshConnection>::get_or_connect;
    let pool_key = supervisor::ConnectionKey {
        host: String::from("h"),
        port: 22,
        username: String::from("u"),
    };
    let _ = pool_key.host.len();
    let _ = pool_key.port;
    let _ = pool_key.username.len();
    let params = supervisor::ConnectParams {
        host: String::from("h"),
        port: 22,
        username: String::from("u"),
    };
    let _ = params.host.len();
    let _ = params.port;
    let _ = params.username.len();
    let _ = <connection::SshConnection as supervisor::PooledConnection>::disconnect_now;

    // auth chain
    let _ = auth::authenticate_chain;
    let config = auth::AuthConfig {
        key_path: Some("/tmp/key".into()),
        key_passphrase: Some(zeroize::Zeroizing::new(b"p".to_vec())),
        password: Some(zeroize::Zeroizing::new(b"p".to_vec())),
        try_agent: true,
        try_keyboard_interactive: true,
        challenge_responder: None,
    };
    let _ = config.key_path;
    let _ = config.key_passphrase.as_ref().map(|p| p.len());
    let _ = config.password.as_ref().map(|p| p.len());
    let _ = config.try_agent;
    let _ = config.try_keyboard_interactive;
    let _ = config.challenge_responder.is_some();

    let outcome = auth::AuthOutcome::Failed {
        reason: String::from("r"),
        methods_remaining: vec![auth::AuthMethodKind::KeyboardInteractive],
    };
    match outcome {
        auth::AuthOutcome::Failed {
            reason,
            methods_remaining,
        } => {
            let _ = reason.len();
            let _ = methods_remaining.len();
        }
        auth::AuthOutcome::Success | auth::AuthOutcome::Cancelled => {}
    }

    let prompt = auth::ChallengePrompt {
        prompt: String::from("p"),
        echo: true,
    };
    let _ = prompt.prompt.len();
    let _ = prompt.echo;

    // credentials
    let prompt_fn: credentials::PromptFn = Arc::new(|_, _| None);
    let prompt_source = credentials::PromptSource::new(prompt_fn);
    let mgr = credentials::CredentialManager::new(vec![Box::new(prompt_source)]);
    let _ = mgr.lookup("touch", "password");
    let secret = zeroize::Zeroizing::new(b"x".to_vec());
    let _ = mgr.store("touch", "Touch", "password", &secret);
    let _ = mgr.source_count();
    for cred_err in [
        credentials::CredentialError::Unavailable(String::new()),
        credentials::CredentialError::StoreError(String::new()),
    ] {
        match cred_err {
            credentials::CredentialError::Unavailable(s)
            | credentials::CredentialError::StoreError(s) => {
                let _ = s.len();
            }
        }
    }

    #[cfg(feature = "keyring")]
    touch_keyring_api();

    // hostkey
    let verifier = hostkey::HostKeyVerifier::new(
        crate::store::model::HostKeyPolicy::Off,
        Arc::new(|_| hostkey::HostKeyDecision::Accept),
        None,
    );
    let handler = connection::AppClientHandler {
        host: String::from("h"),
        port: 22,
        username: String::from("u"),
        host_key_verifier: verifier,
        state_tx: Some(state_tx),
    };
    let _ = handler.host.len();
    let _ = handler.port;
    let _ = handler.username.len();
    let facts = hostkey::HostKeyFacts {
        host: String::from("h"),
        port: 22,
        key_type: String::from("t"),
        fingerprint_sha256: String::from("f"),
    };
    let _ = facts.host.len();
    let _ = facts.port;
    let _ = facts.key_type.len();
    let _ = facts.fingerprint_sha256.len();
}

#[cfg(feature = "keyring")]
fn touch_keyring_api() {
    let source = credentials::KeyringSource::new();
    let _ = credentials::KeyringSource::SCHEMA_ID;
    let secret = zeroize::Zeroizing::new(b"x".to_vec());
    let _ = CredentialSource::store(&source, "touch", "Touch", "password", &secret);
    let _ = CredentialSource::lookup(&source, "touch", "password");
}
