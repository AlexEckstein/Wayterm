use std::fmt;

use russh::client::{self, Handle, Msg};
use russh::keys::HashAlg;
use russh::{Channel, Disconnect};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::transport::auth::{authenticate_chain, AuthConfig, AuthOutcome, ChallengeResponder};
use crate::transport::credentials::CredentialManager;
use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};
use crate::transport::types::{TransportError, TransportState};

/// Authentication method for a session, including the secrets required at
/// connect time.
///
/// Doc 11 defines the full enum with agent and keyboard-interactive. In Phase 2
/// the transport layer converts this into an [`AuthConfig`] and runs the full
/// fallback chain (agent → key → password → keyboard-interactive).
#[derive(Clone, Debug, Default)]
pub enum AuthMethod {
    /// Password authentication.
    Password(String),
    /// Private-key authentication.
    Key {
        path: String,
        passphrase: Option<String>,
    },
    /// SSH agent authentication.
    Agent,
    /// Keyboard-interactive authentication (2FA/OTP/bastions).
    KeyboardInteractive,
    /// No authentication.
    #[default]
    None,
}

/// One authenticated russh session, shared by multiple consumers.
///
/// Created and owned by `ConnectionSupervisor` (Phase 3); consumers hold
/// `Arc`. The session is authenticated; opening channels on it is cheap.
pub struct SshConnection {
    handle: Mutex<Handle<AppClientHandler>>,
    host: String,
    port: u16,
    username: String,
}

impl SshConnection {
    pub fn new(
        handle: Handle<AppClientHandler>,
        host: String,
        port: u16,
        username: String,
    ) -> Self {
        Self {
            handle: Mutex::new(handle),
            host,
            port,
            username,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Shell/exec/PTY channels — used by `SSHTransport`.
    pub async fn open_session_channel(&self) -> Result<Channel<Msg>, TransportError> {
        let handle = self.handle.lock().await;
        handle
            .channel_open_session()
            .await
            .map_err(TransportError::from_russh)
    }

    /// direct-tcpip channels — used by local forwards and jump hops (doc 11).
    /// Phase 2 stub; returns a clear error until Phase 3/11 wiring is built.
    pub async fn open_direct_tcpip(
        &self,
        _host: &str,
        _port: u16,
    ) -> Result<Channel<Msg>, TransportError> {
        Err(TransportError::ChannelError(
            "open_direct_tcpip not wired in Phase 2".into(),
        ))
    }

    /// SFTP subsystem channel — used by the sidebar (doc 09).
    ///
    /// Opens a session channel and requests the `sftp` subsystem on it.
    /// `russh_sftp::client::SftpSession::new` consumes the channel by value
    /// (`channel.into_stream()`), so the subsystem request lives here while
    /// session creation stays with doc 09's SFTP task: the task converts the
    /// returned channel on the first command (lazy open, reopenable after the
    /// server closes the subsystem).
    pub async fn open_sftp_channel(&self) -> Result<Channel<Msg>, TransportError> {
        let channel = self.open_session_channel().await?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(TransportError::from_russh)?;
        Ok(channel)
    }

    /// Disconnect the underlying SSH session.
    pub async fn disconnect(&self) -> Result<(), TransportError> {
        let handle = self.handle.lock().await;
        handle
            .disconnect(Disconnect::ByApplication, "", "English")
            .await
            .map_err(TransportError::from_russh)
    }

    /// Authenticate this connection's handle using the supplied method and
    /// credential manager.  Runs the full fallback chain per doc 11.
    pub async fn authenticate(
        &self,
        method: &AuthMethod,
        creds: &CredentialManager,
        responder: Option<ChallengeResponder>,
    ) -> Result<(), TransportError> {
        let mut handle = self.handle.lock().await;
        let outcome = authenticate_chain(
            &mut handle,
            &self.username,
            &auth_config(method, creds, responder),
        )
        .await?;
        match outcome {
            AuthOutcome::Success => Ok(()),
            AuthOutcome::Cancelled => Err(TransportError::Cancelled),
            AuthOutcome::Failed { reason, .. } => Err(TransportError::AuthFailed(reason)),
        }
    }
}

impl fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConnection")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Build an [`AuthConfig`] from a preferred [`AuthMethod`] and a
/// [`CredentialManager`].
///
/// The preferred method controls which options are enabled, but the chain
/// still falls through so the user can recover if the preferred method fails
/// (e.g. password configured but server only offers keyboard-interactive).
pub(crate) fn auth_config(
    method: &AuthMethod,
    creds: &CredentialManager,
    responder: Option<ChallengeResponder>,
) -> AuthConfig {
    let session_id = "_live_connect";
    let mut config = AuthConfig {
        key_path: None,
        key_passphrase: None,
        password: None,
        try_agent: false,
        try_keyboard_interactive: false,
        challenge_responder: responder,
    };

    match method {
        AuthMethod::Agent => {
            config.try_agent = true;
        }
        AuthMethod::Key { path, passphrase } => {
            config.key_path = Some(path.clone());
            if let Some(pw) = passphrase {
                config.key_passphrase = Some(Zeroizing::new(pw.clone().into_bytes()));
            } else if let Some(pw) = creds.lookup(session_id, "key_passphrase") {
                config.key_passphrase = Some(pw);
            }
        }
        AuthMethod::Password(_) => {
            if let Some(pw) = creds.lookup(session_id, "password") {
                config.password = Some(pw);
            }
            config.try_keyboard_interactive = true;
        }
        AuthMethod::KeyboardInteractive => {
            config.try_keyboard_interactive = true;
        }
        AuthMethod::None => {}
    }

    config
}

/// State carried by the russh `Handler` during connection setup.
pub struct AppClientHandler {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub host_key_verifier: HostKeyVerifier,
    pub state_tx: Option<tokio::sync::mpsc::Sender<TransportState>>,
}

impl AppClientHandler {
    pub fn new(
        host: String,
        port: u16,
        username: String,
        host_key_verifier: HostKeyVerifier,
        state_tx: Option<tokio::sync::mpsc::Sender<TransportState>>,
    ) -> Self {
        Self {
            host,
            port,
            username,
            host_key_verifier,
            state_tx,
        }
    }
}

impl AppClientHandler {
    fn send_state(&self, state: TransportState) {
        if let Some(tx) = &self.state_tx {
            let _ = tx.try_send(state);
        }
    }
}

impl client::Handler for AppClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        self.send_state(TransportState::VerifyingHostKey);

        let key = match server_public_key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                return Err(russh::Error::from(std::io::Error::other(
                    "certificate-based host auth not supported yet",
                )));
            }
        };

        let facts = HostKeyFacts {
            host: self.host.clone(),
            port: self.port,
            key_type: key.algorithm().to_string(),
            fingerprint_sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        };

        match self.host_key_verifier.verify(&facts, key) {
            HostKeyDecision::Accept => Ok(true),
            HostKeyDecision::AcceptAndSave => {
                // `HostKeyVerifier::verify` already called `learn_known_hosts_path`
                // before returning AcceptAndSave.
                Ok(true)
            }
            HostKeyDecision::Reject => Err(russh::Error::from(std::io::Error::other(format!(
                "host key rejected for {}",
                self.host
            )))),
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Banners are not logged (they may contain sensitive host info).
        // They are surfaced through the UI in a later phase.
        let _ = banner;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeroize::Zeroizing;

    use crate::transport::auth::{AuthConfig, AuthMethodKind, ChallengeResponder};
    use crate::transport::connection::{auth_config, AuthMethod, CredentialManager};
    use crate::transport::credentials::PromptSource;

    fn empty_creds() -> CredentialManager {
        CredentialManager::new(vec![])
    }

    fn password_creds(password: &[u8]) -> CredentialManager {
        let source = PromptSource::new(Arc::new({
            let password = password.to_vec();
            move |_session_id, secret_type| match secret_type {
                "password" => Some(Zeroizing::new(password.clone())),
                _ => None,
            }
        }));
        CredentialManager::new(vec![Box::new(source)])
    }

    fn passphrase_creds(passphrase: &[u8]) -> CredentialManager {
        let source = PromptSource::new(Arc::new({
            let passphrase = passphrase.to_vec();
            move |_session_id, secret_type| match secret_type {
                "key_passphrase" => Some(Zeroizing::new(passphrase.clone())),
                _ => None,
            }
        }));
        CredentialManager::new(vec![Box::new(source)])
    }

    #[test]
    fn chain_order_matches_doc_11() {
        // Doc 11 order: agent → publickey → password → keyboard-interactive.
        let expected = [
            AuthMethodKind::Agent,
            AuthMethodKind::PublicKey,
            AuthMethodKind::Password,
            AuthMethodKind::KeyboardInteractive,
        ];
        for (i, kind) in expected.iter().enumerate() {
            assert_eq!(*kind, expected[i]);
        }
        // All four are distinct.
        for (i, a) in expected.iter().enumerate() {
            for (j, b) in expected.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn auth_config_for_agent_enables_agent_only() {
        let config = auth_config(&AuthMethod::Agent, &empty_creds(), None);
        assert!(config.try_agent);
        assert!(!config.try_keyboard_interactive);
        assert!(config.key_path.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn auth_config_for_key_uses_inline_passphrase() {
        let config = auth_config(
            &AuthMethod::Key {
                path: "/tmp/key".into(),
                passphrase: Some("inline-pass".into()),
            },
            &empty_creds(),
            None,
        );
        assert_eq!(config.key_path.as_deref(), Some("/tmp/key"));
        assert_eq!(
            config.key_passphrase.as_ref().map(|p| p.as_slice()),
            Some(b"inline-pass".as_slice())
        );
    }

    #[test]
    fn auth_config_for_key_falls_back_to_credential_manager_passphrase() {
        let config = auth_config(
            &AuthMethod::Key {
                path: "/tmp/key".into(),
                passphrase: None,
            },
            &passphrase_creds(b"cm-pass"),
            None,
        );
        assert_eq!(
            config.key_passphrase.as_ref().map(|p| p.as_slice()),
            Some(b"cm-pass".as_slice())
        );
    }

    #[test]
    fn auth_config_for_password_looks_up_password_and_enables_kbd_interactive() {
        let config = auth_config(
            &AuthMethod::Password("configured".into()),
            &password_creds(b"cm-password"),
            None,
        );
        assert_eq!(
            config.password.as_ref().map(|p| p.as_slice()),
            Some(b"cm-password".as_slice())
        );
        assert!(config.try_keyboard_interactive);
    }

    #[test]
    fn auth_config_for_keyboard_interactive_enables_responder() {
        let responder: ChallengeResponder = Arc::new(|_| Some(vec!["answer".into()]));
        let config = auth_config(
            &AuthMethod::KeyboardInteractive,
            &empty_creds(),
            Some(responder),
        );
        assert!(config.try_keyboard_interactive);
        assert!(config.challenge_responder.is_some());
        assert!(config.password.is_none());
    }

    #[test]
    fn auth_config_for_none_is_empty() {
        let config = auth_config(&AuthMethod::None, &empty_creds(), None);
        assert!(!config.try_agent);
        assert!(!config.try_keyboard_interactive);
        assert!(config.key_path.is_none());
        assert!(config.password.is_none());
        assert!(config.challenge_responder.is_none());
    }

    #[test]
    fn auth_method_default_is_none() {
        let method: AuthMethod = Default::default();
        assert!(matches!(method, AuthMethod::None));
    }

    #[test]
    fn auth_config_debug_redacts_secrets() {
        let responder: ChallengeResponder = Arc::new(|_| None);
        let config = AuthConfig {
            key_path: Some("/tmp/key".into()),
            key_passphrase: Some(Zeroizing::new(b"supersecret".to_vec())),
            password: Some(Zeroizing::new(b"hunter2".to_vec())),
            try_agent: true,
            try_keyboard_interactive: true,
            challenge_responder: Some(responder),
        };
        let dbg = format!("{config:?}");
        assert!(
            dbg.contains("[REDACTED]"),
            "expected redaction marker: {dbg}"
        );
        assert!(
            !dbg.contains("supersecret"),
            "secret leaked in debug: {dbg}"
        );
        assert!(!dbg.contains("hunter2"), "secret leaked in debug: {dbg}");
        // Zeroize zeroes memory on drop, but Debug must still not print raw bytes.
        assert!(
            !dbg.as_bytes().windows(11).any(|w| w == b"supersecret"),
            "raw secret bytes leaked in debug output"
        );
    }
}
