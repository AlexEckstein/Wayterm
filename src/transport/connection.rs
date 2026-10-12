//! [`SshConnection`]: one authenticated russh session shared by every
//! consumer — terminal channels, the SFTP sidebar, future tunnels (doc 04
//! §Connection Supervisor).

use std::fmt;
use std::path::Path;

use russh::client::{self, Handle, Msg};
use russh::keys::{load_secret_key, HashAlg, PublicKeyOrCertificate};
use russh::{Channel, Disconnect};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::transport::auth::{authenticate_chain, AuthConfig, AuthOutcome, ChallengeResponder};
use crate::transport::credentials::CredentialManager;
use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};
use crate::transport::types::{TransportError, TransportState};

/// The method a session prefers. Secrets are looked up through the
/// [`CredentialManager`] at connect time; the chain still falls through to
/// the other methods (doc 11 §Flow).
#[derive(Clone, Debug, Default)]
pub enum AuthMethod {
    /// The payload is not used: the password comes from the credential
    /// lookup, never from the session row.
    Password(String),
    Key {
        path: String,
        passphrase: Option<String>,
    },
    Agent,
    KeyboardInteractive,
    #[default]
    None,
}

/// One authenticated SSH session. Consumers hold an `Arc`; the supervisor
/// disconnects it after the last one lets go.
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

    /// Shell/exec/PTY channels (`SSHTransport`, the SFTP sidebar's shell
    /// probes).
    pub async fn open_session_channel(&self) -> Result<Channel<Msg>, TransportError> {
        self.handle
            .lock()
            .await
            .channel_open_session()
            .await
            .map_err(TransportError::from_russh)
    }

    /// direct-tcpip channels for local forwards and jump hosts (doc 11
    /// §Jump hosts and port forwarding). Originator address/port are
    /// client-side metadata, so the loopback/0 convention (like OpenSSH
    /// `-W`) is used; callers only name the destination.
    pub async fn open_direct_tcpip(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Channel<Msg>, TransportError> {
        self.handle
            .lock()
            .await
            .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
            .await
            .map_err(TransportError::from_russh)
    }

    /// A session channel with the `sftp` subsystem requested; the SFTP task
    /// turns it into a session with `into_stream()` (doc 09).
    pub async fn open_sftp_channel(&self) -> Result<Channel<Msg>, TransportError> {
        let channel = self.open_session_channel().await?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(TransportError::from_russh)?;
        Ok(channel)
    }

    pub async fn disconnect(&self) -> Result<(), TransportError> {
        self.handle
            .lock()
            .await
            .disconnect(Disconnect::ByApplication, "", "English")
            .await
            .map_err(TransportError::from_russh)
    }

    /// Run the auth chain on this session (doc 11 §Flow).
    pub async fn authenticate(
        &self,
        method: &AuthMethod,
        creds: &CredentialManager,
        responder: Option<ChallengeResponder>,
    ) -> Result<(), TransportError> {
        let config = auth_config(method, creds.session_id(), creds, responder)?;
        let mut handle = self.handle.lock().await;
        match authenticate_chain(&mut handle, &self.username, &config).await? {
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

/// Turn the preferred method into the chain's inputs, looking secrets up
/// under `session_id`. A credential prompt the user cancels aborts the
/// connect with [`TransportError::Cancelled`] instead of falling through to
/// a failure screen.
pub(crate) fn auth_config(
    method: &AuthMethod,
    session_id: &str,
    creds: &CredentialManager,
    responder: Option<ChallengeResponder>,
) -> Result<AuthConfig, TransportError> {
    let mut config = AuthConfig {
        key_path: None,
        key_passphrase: None,
        password: None,
        try_agent: false,
        try_keyboard_interactive: false,
        challenge_responder: responder,
    };
    match method {
        AuthMethod::Agent => config.try_agent = true,
        AuthMethod::Key { path, passphrase } => {
            config.key_path = Some(path.clone());
            config.key_passphrase = match passphrase {
                Some(pw) => Some(Zeroizing::new(pw.clone().into_bytes())),
                // Only an encrypted key prompts; nothing can rescue a
                // missing or corrupt one.
                None if key_needs_passphrase(path) => Some(
                    creds
                        .lookup(session_id, "key_passphrase")
                        .ok_or(TransportError::Cancelled)?,
                ),
                None => None,
            };
        }
        AuthMethod::Password(_) => {
            config.password = Some(
                creds
                    .lookup(session_id, "password")
                    .ok_or(TransportError::Cancelled)?,
            );
            config.try_keyboard_interactive = true;
        }
        AuthMethod::KeyboardInteractive => config.try_keyboard_interactive = true,
        AuthMethod::None => {}
    }
    Ok(config)
}

fn key_needs_passphrase(path: &str) -> bool {
    matches!(
        load_secret_key(Path::new(path), None),
        Err(russh::keys::Error::KeyIsEncrypted)
    )
}

/// The russh client handler: host-key verification happens here, during
/// key exchange and before authentication.
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

impl client::Handler for AppClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        if let Some(tx) = &self.state_tx {
            let _ = tx.try_send(TransportState::VerifyingHostKey);
        }
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => {
                return Err(
                    std::io::Error::other("certificate-based host auth not supported yet").into(),
                );
            }
        };
        let facts = HostKeyFacts {
            host: self.host.clone(),
            port: self.port,
            key_type: key.algorithm().to_string(),
            fingerprint_sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        };
        match self.host_key_verifier.verify(&facts, key) {
            HostKeyDecision::Accept | HostKeyDecision::AcceptAndSave => Ok(true),
            // The supervisor recognises this message as HostKeyRejected.
            HostKeyDecision::Reject => {
                Err(std::io::Error::other(format!("host key rejected for {}", self.host)).into())
            }
        }
    }

    /// Banners may carry sensitive host info, so they are not logged; the UI
    /// does not show them yet.
    async fn auth_banner(
        &mut self,
        _banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;
    use crate::transport::credentials::PromptSource;

    const SESSION: &str = "42";

    /// Answers only `secret_type` prompts for [`SESSION`], with `secret`.
    fn creds_for(secret_type: &'static str, secret: &'static [u8]) -> CredentialManager {
        CredentialManager::new(vec![Box::new(PromptSource::new(Arc::new(
            move |session_id, asked| {
                (session_id == SESSION && asked == secret_type)
                    .then(|| Zeroizing::new(secret.to_vec()))
            },
        )))])
    }

    fn no_creds() -> CredentialManager {
        CredentialManager::new(vec![])
    }

    fn cancelling_creds() -> CredentialManager {
        CredentialManager::new(vec![Box::new(PromptSource::new(Arc::new(|_, _| None)))])
    }

    fn key(path: impl Into<String>) -> AuthMethod {
        AuthMethod::Key {
            path: path.into(),
            passphrase: None,
        }
    }

    /// A throwaway ed25519 key generated at runtime (no key material is
    /// committed); `None` when `ssh-keygen` is unavailable.
    fn generate_test_key(dir: &Path, passphrase: &str) -> Option<PathBuf> {
        let path = dir.join("id_ed25519");
        std::process::Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-q", "-f"])
            .arg(&path)
            .args(["-N", passphrase])
            .status()
            .ok()?
            .success()
            .then_some(path)
    }

    #[test]
    fn agent_and_none_enable_only_what_they_name() {
        let agent = auth_config(&AuthMethod::Agent, SESSION, &no_creds(), None).unwrap();
        assert!(agent.try_agent && !agent.try_keyboard_interactive);
        assert!(agent.key_path.is_none() && agent.password.is_none());

        let none = auth_config(&AuthMethod::None, SESSION, &no_creds(), None).unwrap();
        assert!(!none.try_agent && !none.try_keyboard_interactive);
        assert!(none.key_path.is_none() && none.password.is_none());
    }

    #[test]
    fn key_uses_an_inline_passphrase() {
        let method = AuthMethod::Key {
            path: "/tmp/key".into(),
            passphrase: Some("inline-pass".into()),
        };
        let config = auth_config(&method, SESSION, &no_creds(), None).unwrap();
        assert_eq!(config.key_path.as_deref(), Some("/tmp/key"));
        assert_eq!(
            config.key_passphrase.as_deref(),
            Some(&b"inline-pass".to_vec())
        );
    }

    #[test]
    fn only_an_encrypted_key_asks_for_a_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let Some(encrypted) = generate_test_key(dir.path(), "key-secret") else {
            eprintln!("ssh-keygen unavailable; skipping");
            return;
        };
        let path = encrypted.to_string_lossy().into_owned();
        let config = auth_config(
            &key(&path),
            SESSION,
            &creds_for("key_passphrase", b"cm-pass"),
            None,
        );
        assert_eq!(
            config.unwrap().key_passphrase.as_deref(),
            Some(&b"cm-pass".to_vec())
        );
        let cancelled = auth_config(&key(&path), SESSION, &cancelling_creds(), None);
        assert!(matches!(cancelled, Err(TransportError::Cancelled)));

        let plain_dir = tempfile::tempdir().unwrap();
        let plain = generate_test_key(plain_dir.path(), "").unwrap();
        let creds = creds_for("key_passphrase", b"cm-pass");
        let config = auth_config(&key(plain.to_string_lossy()), SESSION, &creds, None).unwrap();
        assert!(
            config.key_passphrase.is_none(),
            "unencrypted key: no prompt"
        );

        let config = auth_config(&key("/nonexistent-wayterm-key"), SESSION, &creds, None).unwrap();
        assert!(config.key_passphrase.is_none(), "unreadable key: no prompt");
    }

    #[test]
    fn password_comes_from_the_credential_lookup() {
        let method = AuthMethod::Password("ignored".into());
        let config = auth_config(
            &method,
            SESSION,
            &creds_for("password", b"cm-password"),
            None,
        )
        .unwrap();
        assert_eq!(config.password.as_deref(), Some(&b"cm-password".to_vec()));
        assert!(config.try_keyboard_interactive);

        let cancelled = auth_config(&method, SESSION, &cancelling_creds(), None);
        assert!(matches!(cancelled, Err(TransportError::Cancelled)));
    }

    #[test]
    fn lookups_key_on_the_given_session_id() {
        let method = AuthMethod::Password(String::new());
        let creds = creds_for("password", b"cm-password");
        let other = auth_config(&method, "43", &creds, None);
        assert!(matches!(other, Err(TransportError::Cancelled)));
    }

    #[test]
    fn keyboard_interactive_keeps_the_responder() {
        let responder: ChallengeResponder = Arc::new(|_| Some(vec!["answer".into()]));
        let config = auth_config(
            &AuthMethod::KeyboardInteractive,
            SESSION,
            &no_creds(),
            Some(responder),
        )
        .unwrap();
        assert!(config.try_keyboard_interactive && config.challenge_responder.is_some());
        assert!(config.password.is_none());
    }
}
