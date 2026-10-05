use std::fmt;
use std::path::Path;
use std::sync::Arc;

use russh::client::{self, Handle, Msg};
use russh::keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg};
use russh::{Channel, Disconnect};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};
use crate::transport::types::{TransportError, TransportState};

/// Authentication method for a session, including the secrets required at
/// connect time.
///
/// Doc 11 defines the full enum with agent and keyboard-interactive; this
/// Phase-2 subset implements password and private-key, stubs the rest.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum AuthMethod {
    Password(Zeroizing<Vec<u8>>),
    Key {
        path: String,
        passphrase: Option<Zeroizing<Vec<u8>>>,
    },
    /// Not implemented in Phase 2 — placeholder so the state machine stays
    /// compatible with doc 11.
    Agent,
    /// Not implemented in Phase 2 — placeholder.
    KeyboardInteractive,
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    /// Phase 2 stub; SFTP arrives in Phase 3.
    #[allow(dead_code)]
    pub async fn open_sftp_channel(&self) -> Result<Channel<Msg>, TransportError> {
        Err(TransportError::ChannelError(
            "open_sftp_channel not wired in Phase 2".into(),
        ))
    }

    /// Disconnect the underlying SSH session.
    #[allow(dead_code)]
    pub async fn disconnect(&self) -> Result<(), TransportError> {
        let handle = self.handle.lock().await;
        handle
            .disconnect(Disconnect::ByApplication, "", "English")
            .await
            .map_err(TransportError::from_russh)
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

/// Authenticate a freshly opened handle using the supplied method.
///
/// This is an internal helper used by the (future) connection supervisor and by
/// the SSH transport in this phase while the supervisor is not yet built.
#[allow(dead_code)]
pub async fn authenticate(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    method: &AuthMethod,
) -> Result<(), TransportError> {
    match method {
        AuthMethod::Password(password) => {
            let password = String::from_utf8_lossy(password).to_string();
            let res = handle
                .authenticate_password(username, password)
                .await
                .map_err(TransportError::from_russh)?;
            if !res.success() {
                return Err(TransportError::AuthFailed(
                    "password authentication failed".into(),
                ));
            }
        }
        AuthMethod::Key { path, passphrase } => {
            let passphrase = passphrase
                .as_ref()
                .map(|p| std::str::from_utf8(p).unwrap_or("").to_string());
            let key = load_secret_key(Path::new(path), passphrase.as_deref())
                .map_err(|e| TransportError::AuthFailed(e.to_string()))?;
            let hash_alg = handle
                .best_supported_rsa_hash()
                .await
                .map_err(TransportError::from_russh)?
                .flatten();
            let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            let res = handle
                .authenticate_publickey(username, key_with_alg)
                .await
                .map_err(TransportError::from_russh)?;
            if !res.success() {
                return Err(TransportError::AuthFailed(
                    "public-key authentication failed".into(),
                ));
            }
        }
        AuthMethod::Agent => {
            return Err(TransportError::AuthFailed(
                "agent authentication not implemented in Phase 2".into(),
            ));
        }
        AuthMethod::KeyboardInteractive => {
            return Err(TransportError::AuthFailed(
                "keyboard-interactive authentication not implemented in Phase 2".into(),
            ));
        }
        AuthMethod::None => {
            let res = handle
                .authenticate_none(username)
                .await
                .map_err(TransportError::from_russh)?;
            if !res.success() {
                return Err(TransportError::AuthFailed(
                    "none authentication failed".into(),
                ));
            }
        }
    }
    Ok(())
}

/// State carried by the russh `Handler` during connection setup.
#[allow(dead_code)]
pub struct AppClientHandler {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub host_key_verifier: HostKeyVerifier,
    pub state_tx: Option<tokio::sync::mpsc::Sender<TransportState>>,
}

impl AppClientHandler {
    #[allow(dead_code)]
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
