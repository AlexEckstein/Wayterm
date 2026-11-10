//! Host-key verification against `~/.ssh/known_hosts` (doc 04
//! §Host key verification).

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use data_encoding::BASE64;
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{Algorithm, PublicKey};
use russh::Preferred;

use crate::store::model::HostKeyPolicy;

mod known_hosts;
use known_hosts::{HostEntries, Standing};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyDecision {
    Accept,
    /// Accept and record the key in known_hosts.
    AcceptAndSave,
    Reject,
}

/// What the unknown-host prompt shows (OpenSSH style).
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

/// Asks the user about an unknown host; must answer `AcceptAndSave` to
/// accept, so the key gets recorded.
pub type HostKeyPrompt = Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>;

/// Decides whether to trust a server's key under a [`HostKeyPolicy`].
#[derive(Clone)]
pub struct HostKeyVerifier {
    policy: HostKeyPolicy,
    prompt: HostKeyPrompt,
    /// `None` = `~/.ssh/known_hosts`; tests pass a temp file.
    known_hosts_path: Option<Arc<Path>>,
}

impl HostKeyVerifier {
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

    /// Whether keys are checked at all: under `off` they are not.
    pub fn verifies(&self) -> bool {
        self.policy != HostKeyPolicy::Off
    }

    /// Decide whether to trust `key`, by what known_hosts holds for the
    /// host:
    ///
    /// * this key → accept;
    /// * other keys only, of any type, or this key revoked → reject, no
    ///   prompt (the MITM signal);
    /// * nothing → the policy decides (`strict` prompts, `accept-new`
    ///   records, `off` accepts).
    ///
    /// Whatever keeps that from being answered rejects: a file that cannot
    /// be read, a host name that cannot be written to it, a key that
    /// should be recorded and cannot be. Only `off` connects regardless.
    pub fn verify(&self, facts: &HostKeyFacts, key: &PublicKey) -> HostKeyDecision {
        let (host, port) = (facts.host.as_str(), facts.port);
        let unverified = |why: &dyn fmt::Display| {
            eprintln!("Wayterm: host key for {host}:{port} not verified: {why}");
            match self.policy {
                HostKeyPolicy::Off => HostKeyDecision::Accept,
                HostKeyPolicy::AcceptNew | HostKeyPolicy::Strict => HostKeyDecision::Reject,
            }
        };
        if !recordable(host) {
            return unverified(&"the host name cannot be recorded in known_hosts");
        }
        let entries = match self.entries(host, port) {
            Ok(entries) => entries,
            Err(err) => return unverified(&format_args!("cannot read known_hosts: {err}")),
        };
        let Ok(blob) = key.to_bytes() else {
            return unverified(&"the key cannot be encoded");
        };
        let decision = match entries.standing(&BASE64.encode(&blob)) {
            Standing::Trusted => return HostKeyDecision::Accept,
            Standing::Changed | Standing::Revoked => return HostKeyDecision::Reject,
            Standing::Unknown => match self.policy {
                HostKeyPolicy::Off => return HostKeyDecision::Accept,
                HostKeyPolicy::AcceptNew => HostKeyDecision::AcceptAndSave,
                HostKeyPolicy::Strict => (self.prompt)(facts),
            },
        };
        if decision == HostKeyDecision::AcceptAndSave && self.record(facts, key).is_err() {
            return HostKeyDecision::Reject;
        }
        decision
    }

    /// The host-key algorithms to offer `host`, those of its recorded keys
    /// first, as OpenSSH orders them. A host with a known key is then asked
    /// for that one, so any other key it shows is a changed key and never
    /// just a type known_hosts has not seen.
    pub fn key_preference(&self, host: &str, port: u16) -> Cow<'static, [Algorithm]> {
        let default = Preferred::DEFAULT.key;
        let Ok(entries) = self.entries(host, port) else {
            return default;
        };
        let (mut known, other): (Vec<_>, Vec<_>) = default
            .iter()
            .cloned()
            .partition(|algorithm| entries.has_key_type(key_type(algorithm)));
        if known.is_empty() {
            return default;
        }
        known.extend(other);
        Cow::Owned(known)
    }

    fn entries(&self, host: &str, port: u16) -> io::Result<HostEntries> {
        HostEntries::read(&self.known_hosts()?, host, port)
    }

    /// Append the key, creating `~/.ssh` private if it is missing.
    fn record(&self, facts: &HostKeyFacts, key: &PublicKey) -> io::Result<()> {
        let path = self.known_hosts()?;
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        learn_known_hosts_path(&facts.host, facts.port, key, &path).map_err(io::Error::other)
    }

    /// Without a home directory there is no file to trust: never one
    /// relative to wherever Wayterm was started.
    fn known_hosts(&self) -> io::Result<PathBuf> {
        match &self.known_hosts_path {
            Some(path) => Ok(path.to_path_buf()),
            None => std::env::home_dir()
                .map(|home| home.join(".ssh/known_hosts"))
                .ok_or_else(|| io::Error::other("no home directory")),
        }
    }
}

/// The type a key of `algorithm` is recorded under: the RSA signature
/// algorithms all use an `ssh-rsa` key.
fn key_type(algorithm: &Algorithm) -> &str {
    match algorithm {
        Algorithm::Rsa { .. } => "ssh-rsa",
        other => other.as_str(),
    }
}

/// Whether `host` can stand in a known_hosts line as one plain name. The
/// name is written as given, so blanks would start another field or line,
/// and `,` `*` `?` `!` `|` `#` `@` `[` `]` would make it a list, a pattern,
/// a hashed name, a comment, a marker or a port form: an entry for some
/// other host, or for every host.
fn recordable(host: &str) -> bool {
    !host.is_empty()
        && !host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || ",*?!|#@[]".contains(c))
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
