use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::Error as KeysError;
use russh::keys::PublicKey;

use crate::store::model::HostKeyPolicy;

fn known_hosts_default_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("known_hosts")
}

/// Outcome of a host-key prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyDecision {
    Accept,
    AcceptAndSave,
    Reject,
}

/// OpenSSH-style facts shown to the user when prompting about an unknown host.
#[derive(Clone, Debug)]
pub struct HostKeyFacts {
    pub host: String,
    pub port: u16,
    pub key_type: String,
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

/// Host-key verification logic per doc 11.
///
/// `check_known_hosts_path` does the actual file check. This type maps the
/// result to a decision, applying the configured policy for unknown hosts.
pub struct HostKeyVerifier {
    policy: HostKeyPolicy,
    prompt: Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>,
    known_hosts_path: Option<Arc<Path>>,
}

impl HostKeyVerifier {
    /// Build a verifier.
    ///
    /// * `policy` — global or per-session policy.
    /// * `prompt` — closure bridging to the GTK dialog. Must return
    ///   `AcceptAndSave` when the user confirms, so the key is recorded.
    /// * `known_hosts_path` — `None` means the default `~/.ssh/known_hosts`
    ///   is used by `russh::keys`. Tests always provide an explicit path.
    pub fn new(
        policy: HostKeyPolicy,
        prompt: Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>,
        known_hosts_path: Option<Arc<Path>>,
    ) -> Self {
        Self {
            policy,
            prompt,
            known_hosts_path,
        }
    }

    /// Decide whether to trust a server key.
    ///
    /// Mapping per russh 0.64.1 known_hosts semantics:
    /// * `Ok(true)`    → known + matching  → `Accept`.
    /// * `Ok(false)`   → unknown host       → apply `policy`.
    /// * `Err(KeyChanged { line })` → known + mismatched → hard `Reject`,
    ///   no prompt.
    pub fn verify(&self, facts: &HostKeyFacts, key: &PublicKey) -> HostKeyDecision {
        let path = self.known_hosts_path.as_deref();
        let check_result = if let Some(p) = path {
            check_known_hosts_path(&facts.host, facts.port, key, p)
        } else {
            check_known_hosts_path(&facts.host, facts.port, key, known_hosts_default_path())
        };

        match check_result {
            Ok(true) => HostKeyDecision::Accept,
            Ok(false) => match self.policy {
                HostKeyPolicy::Strict => {
                    let decision = (self.prompt)(facts);
                    if decision == HostKeyDecision::AcceptAndSave {
                        let learn_path =
                            path.map_or_else(known_hosts_default_path, |p| p.to_path_buf());
                        if learn_known_hosts_path(&facts.host, facts.port, key, learn_path).is_err()
                        {
                            // If we cannot persist the key, reject rather than silently
                            // accepting an unrecorded host under a strict policy.
                            return HostKeyDecision::Reject;
                        }
                    }
                    decision
                }
                HostKeyPolicy::AcceptNew => {
                    let learn_path =
                        path.map_or_else(known_hosts_default_path, |p| p.to_path_buf());
                    if learn_known_hosts_path(&facts.host, facts.port, key, learn_path).is_err() {
                        return HostKeyDecision::Reject;
                    }
                    HostKeyDecision::AcceptAndSave
                }
                HostKeyPolicy::Off => HostKeyDecision::Accept,
            },
            Err(KeysError::KeyChanged { line }) => {
                let _ = line;
                HostKeyDecision::Reject
            }
            Err(_) => {
                // File/read errors other than KeyChanged are treated as an
                // unknown host so the policy can still decide. Under strict
                // this will prompt; under accept-new this will auto-save.
                self.verify_for_unknown(facts, key)
            }
        }
    }

    fn verify_for_unknown(&self, facts: &HostKeyFacts, key: &PublicKey) -> HostKeyDecision {
        let path = self.known_hosts_path.as_deref();
        match self.policy {
            HostKeyPolicy::Strict => {
                let decision = (self.prompt)(facts);
                if decision == HostKeyDecision::AcceptAndSave {
                    let learn_path =
                        path.map_or_else(known_hosts_default_path, |p| p.to_path_buf());
                    if learn_known_hosts_path(&facts.host, facts.port, key, learn_path).is_err() {
                        return HostKeyDecision::Reject;
                    }
                }
                decision
            }
            HostKeyPolicy::AcceptNew => {
                let learn_path = path.map_or_else(known_hosts_default_path, |p| p.to_path_buf());
                if learn_known_hosts_path(&facts.host, facts.port, key, learn_path).is_err() {
                    return HostKeyDecision::Reject;
                }
                HostKeyDecision::AcceptAndSave
            }
            HostKeyPolicy::Off => HostKeyDecision::Accept,
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
