//! Host-key verification against `~/.ssh/known_hosts` (doc 04
//! §Host key verification).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{Error as KeysError, PublicKey};

use crate::store::model::HostKeyPolicy;

/// What to do with a server's host key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyDecision {
    /// Trust it for this connection.
    Accept,
    /// Accept and record the key in known_hosts.
    AcceptAndSave,
    /// Refuse it; the dial fails with `HostKeyRejected`.
    Reject,
}

/// What the unknown-host prompt shows (OpenSSH style).
#[derive(Clone, Debug)]
pub struct HostKeyFacts {
    /// Host as dialed.
    pub host: String,
    /// Port as dialed.
    pub port: u16,
    /// Key algorithm, e.g. `ssh-ed25519`.
    pub key_type: String,
    /// `SHA256:…`, as OpenSSH prints it.
    pub fingerprint_sha256: String,
}

impl fmt::Display for HostKeyFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} key fingerprint is {} for {}:{}",
            self.key_type, self.fingerprint_sha256, self.host, self.port
        )
    }
}

/// Asks the user about an unknown host; must answer `AcceptAndSave` to
/// accept, so the key gets recorded.
pub type HostKeyPrompt = Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>;

/// Maps a known_hosts check to a decision under a [`HostKeyPolicy`].
#[derive(Clone)]
pub struct HostKeyVerifier {
    /// Global or per-session policy.
    policy: HostKeyPolicy,
    /// Asks about unknown hosts under `strict`.
    prompt: HostKeyPrompt,
    /// `None` = `~/.ssh/known_hosts`; tests pass a temp file.
    known_hosts_path: Option<Arc<Path>>,
}

impl HostKeyVerifier {
    /// A verifier for `policy`; `known_hosts_path` `None` is `~/.ssh/known_hosts`.
    pub fn new(
        policy: HostKeyPolicy,
        prompt: HostKeyPrompt,
        known_hosts_path: Option<Arc<Path>>,
    ) -> Self {
        Self {
            policy,
            prompt,
            known_hosts_path,
        }
    }

    /// Decide whether to trust `key` (russh 0.64 semantics):
    ///
    /// * `Ok(true)` — known and matching → accept;
    /// * `Err(KeyChanged)` — known and **mismatched** → reject, no prompt
    ///   (the MITM signal);
    /// * `Ok(false)` or any other error — unknown → the policy decides
    ///   (`strict` prompts, `accept-new` records, `off` accepts).
    ///
    /// A key that should be recorded but cannot be is rejected rather than
    /// accepted unrecorded.
    pub fn verify(&self, facts: &HostKeyFacts, key: &PublicKey) -> HostKeyDecision {
        let path = self.known_hosts();
        let decision = match check_known_hosts_path(&facts.host, facts.port, key, &path) {
            Ok(true) => return HostKeyDecision::Accept,
            Err(KeysError::KeyChanged { .. }) => return HostKeyDecision::Reject,
            Ok(false) | Err(_) => match self.policy {
                HostKeyPolicy::Off => return HostKeyDecision::Accept,
                HostKeyPolicy::AcceptNew => HostKeyDecision::AcceptAndSave,
                HostKeyPolicy::Strict => (self.prompt)(facts),
            },
        };
        if decision == HostKeyDecision::AcceptAndSave
            && learn_known_hosts_path(&facts.host, facts.port, key, &path).is_err()
        {
            return HostKeyDecision::Reject;
        }
        decision
    }

    /// The known_hosts file in use.
    fn known_hosts(&self) -> PathBuf {
        match &self.known_hosts_path {
            Some(path) => path.to_path_buf(),
            None => std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".ssh/known_hosts"),
        }
    }
}

impl fmt::Debug for HostKeyVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostKeyVerifier")
            .field("policy", &self.policy)
            .field("known_hosts_path", &self.known_hosts_path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
