//! Host-key verification against `~/.ssh/known_hosts` and the keys the
//! session itself trusts (doc 04 §Host key verification).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use data_encoding::BASE64;
use russh::Preferred;
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{Algorithm, PublicKey};

use crate::store::model::{HostKeyPolicy, SessionHostKey};

mod known_hosts;
use known_hosts::{HostEntries, Standing, unpadded};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyDecision {
    Accept,
    /// Accept and record the key in the global known_hosts: the answer to
    /// the unknown-host question.
    AcceptAndSave,
    /// Accept and add the key to the session's own set, never to
    /// known_hosts: the answer to the changed-key question.
    AcceptAndSaveSession,
    Reject,
}

/// Which question a [`HostKeyPrompt`] is asked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyQuestion {
    /// Nothing records the host: trust the key and record it in
    /// known_hosts (`AcceptAndSave`), or reject.
    Unknown,
    /// known_hosts or the session records the host and none of its keys is
    /// this one: deny, or trust it for this session only
    /// (`AcceptAndSaveSession`).
    Changed,
}

/// What a host-key prompt shows (OpenSSH style).
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

/// Asks the user a [`HostKeyQuestion`] about a server's key. Only the
/// question's own accepting answer accepts: `AcceptAndSave` for an unknown
/// host, so the key gets recorded, `AcceptAndSaveSession` for a changed
/// key; anything else rejects.
pub type HostKeyPrompt =
    Arc<dyn Fn(HostKeyQuestion, &HostKeyFacts) -> HostKeyDecision + Send + Sync>;

/// A prompt that only answers the unknown-host question (see
/// [`HostKeyVerifier::new`]).
pub type UnknownHostPrompt = Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>;

/// The host keys one session trusts beside known_hosts (`sessions.host_keys`,
/// doc 04 §Host key verification), shared between the connect flow and the
/// verifier it hands to the dial: the verifier adds the key the user
/// accepts at the changed-key prompt, and the flow, back on the main
/// thread, persists what was [`added`](Self::added). Keys are held without
/// base64 padding, as known_hosts entries are compared.
#[derive(Clone, Default)]
pub struct SessionKeySet {
    inner: Arc<Mutex<KeySetInner>>,
}

#[derive(Default)]
struct KeySetInner {
    keys: Vec<SessionHostKey>,
    /// How many of `keys` the set started with; the rest were added.
    seeded: usize,
}

impl SessionKeySet {
    /// A set seeded with `keys`, the session's stored ones.
    pub fn new(keys: impl IntoIterator<Item = SessionHostKey>) -> Self {
        let mut seed: Vec<SessionHostKey> = Vec::new();
        for key in keys {
            let key = SessionHostKey {
                key: unpadded(&key.key).to_string(),
                ..key
            };
            if !seed.contains(&key) {
                seed.push(key);
            }
        }
        let seeded = seed.len();
        Self {
            inner: Arc::new(Mutex::new(KeySetInner { keys: seed, seeded })),
        }
    }

    /// Every key in the set, seeded ones first.
    pub fn keys(&self) -> Vec<SessionHostKey> {
        self.lock().keys.clone()
    }

    /// The keys added since the set was seeded: what the session's row
    /// still lacks.
    pub fn added(&self) -> Vec<SessionHostKey> {
        let inner = self.lock();
        inner.keys[inner.seeded..].to_vec()
    }

    /// The set as a value, for telling connections apart by the keys their
    /// sessions trust (the supervisor's pool key).
    pub fn identity(&self) -> BTreeSet<SessionHostKey> {
        self.lock().keys.iter().cloned().collect()
    }

    fn is_empty(&self) -> bool {
        self.lock().keys.is_empty()
    }

    /// Whether the set holds `key`, a key blob's base64 form.
    fn contains(&self, key: &str) -> bool {
        let key = unpadded(key);
        self.lock().keys.iter().any(|trusted| trusted.key == key)
    }

    fn has_key_type(&self, key_type: &str) -> bool {
        self.lock()
            .keys
            .iter()
            .any(|trusted| trusted.key_type == key_type)
    }

    /// Trust `key` for the session: what the verifier does with the key
    /// the user accepts at the changed-key prompt. A key already held is
    /// not added twice.
    pub fn add(&self, key: SessionHostKey) {
        let key = SessionHostKey {
            key: unpadded(&key.key).to_string(),
            ..key
        };
        let mut inner = self.lock();
        if !inner.keys.contains(&key) {
            inner.keys.push(key);
        }
    }

    fn lock(&self) -> MutexGuard<'_, KeySetInner> {
        // A poisoned lock still guards a consistent list.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl fmt::Debug for SessionKeySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Key types only: the keys are public, but logs never carry key
        // material.
        let inner = self.lock();
        f.debug_struct("SessionKeySet")
            .field(
                "key_types",
                &inner.keys.iter().map(|k| &k.key_type).collect::<Vec<_>>(),
            )
            .field("added", &(inner.keys.len() - inner.seeded))
            .finish()
    }
}

/// Decides whether to trust a server's key under a [`HostKeyPolicy`].
#[derive(Clone)]
pub struct HostKeyVerifier {
    policy: HostKeyPolicy,
    prompt: HostKeyPrompt,
    /// `None` = `~/.ssh/known_hosts`; tests pass a temp file.
    known_hosts_path: Option<Arc<Path>>,
    session_keys: SessionKeySet,
}

impl HostKeyVerifier {
    /// A verifier whose `prompt` answers only the unknown-host question: a
    /// changed key is denied without asking. The app asks both with
    /// [`Self::asking`].
    pub fn new(
        policy: HostKeyPolicy,
        prompt: UnknownHostPrompt,
        known_hosts_path: Option<Arc<Path>>,
    ) -> Self {
        let prompt: HostKeyPrompt = Arc::new(move |question, facts| match question {
            HostKeyQuestion::Unknown => prompt(facts),
            HostKeyQuestion::Changed => HostKeyDecision::Reject,
        });
        Self::asking(policy, prompt, known_hosts_path)
    }

    /// A verifier asking `prompt` about unknown hosts and changed keys,
    /// with an empty session key set.
    pub fn asking(
        policy: HostKeyPolicy,
        prompt: HostKeyPrompt,
        known_hosts_path: Option<Arc<Path>>,
    ) -> Self {
        Self {
            policy,
            prompt,
            known_hosts_path,
            session_keys: SessionKeySet::default(),
        }
    }

    /// Trust the keys of `keys` beside known_hosts, and add to it the
    /// changed key the user accepts for this session.
    pub fn with_session_keys(mut self, keys: SessionKeySet) -> Self {
        self.session_keys = keys;
        self
    }

    /// The session's key set, which a changed key accepted for the session
    /// joins.
    pub fn session_keys(&self) -> &SessionKeySet {
        &self.session_keys
    }

    /// Whether keys are checked at all: under `off` they are not.
    pub fn verifies(&self) -> bool {
        self.policy != HostKeyPolicy::Off
    }

    /// Decide whether to trust `key`, by what known_hosts and the
    /// session's key set hold for the host:
    ///
    /// * this key under `@revoked` in known_hosts → reject, every policy;
    /// * this key in known_hosts or in the session's set → accept;
    /// * other keys only, of any type, in either → a changed key, the MITM
    ///   signal: `strict` and `accept-new` ask whether to deny or to trust
    ///   it for this session (`AcceptAndSaveSession`, which adds it to the
    ///   set and never writes known_hosts); `off` rejects;
    /// * nothing → the policy decides (`strict` prompts, `accept-new`
    ///   records, `off` accepts); an accepted key is recorded in
    ///   known_hosts.
    ///
    /// Whatever keeps that from being answered rejects: a file that cannot
    /// be read, a host name that cannot be written to it, a key that
    /// cannot be encoded or should be recorded and cannot be. Only `off`
    /// connects regardless.
    pub fn verify(&self, facts: &HostKeyFacts, key: &PublicKey) -> HostKeyDecision {
        let (host, port) = (facts.host.as_str(), facts.port);
        let rejected = |why: &dyn fmt::Display| {
            // Every reason is a static text or an io error: no key material.
            tracing::warn!(host = %host, port, reason = %why, "host key rejected");
            HostKeyDecision::Reject
        };
        let unverified = |why: &dyn fmt::Display| match self.policy {
            HostKeyPolicy::Off => HostKeyDecision::Accept,
            HostKeyPolicy::AcceptNew | HostKeyPolicy::Strict => rejected(why),
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
        let encoded = BASE64.encode(&blob);
        let presented = unpadded(&encoded);
        let changed = match entries.standing(presented) {
            Standing::Revoked => return rejected(&"known_hosts revokes this key"),
            Standing::Trusted => return HostKeyDecision::Accept,
            _ if self.session_keys.contains(presented) => return HostKeyDecision::Accept,
            Standing::Changed => "known_hosts holds another key for this host",
            Standing::Unknown if !self.session_keys.is_empty() => {
                "the session trusts other keys for this host"
            }
            Standing::Unknown => return self.unknown(facts, key, &rejected),
        };
        match self.policy {
            HostKeyPolicy::Off => rejected(&changed),
            HostKeyPolicy::AcceptNew | HostKeyPolicy::Strict => {
                match (self.prompt)(HostKeyQuestion::Changed, facts) {
                    HostKeyDecision::AcceptAndSaveSession => {
                        self.session_keys.add(SessionHostKey {
                            key_type: key_type(&key.algorithm()).to_string(),
                            key: presented.to_string(),
                        });
                        HostKeyDecision::AcceptAndSaveSession
                    }
                    _ => rejected(&format_args!("{changed}, and it was not accepted")),
                }
            }
        }
    }

    /// Nothing records the host: the policy decides, and an accepted key is
    /// recorded in known_hosts.
    fn unknown(
        &self,
        facts: &HostKeyFacts,
        key: &PublicKey,
        rejected: &dyn Fn(&dyn fmt::Display) -> HostKeyDecision,
    ) -> HostKeyDecision {
        let decision = match self.policy {
            HostKeyPolicy::Off => return HostKeyDecision::Accept,
            HostKeyPolicy::AcceptNew => HostKeyDecision::AcceptAndSave,
            HostKeyPolicy::Strict => match (self.prompt)(HostKeyQuestion::Unknown, facts) {
                // Not an answer to this question.
                HostKeyDecision::AcceptAndSaveSession => {
                    return rejected(&"the unknown-host prompt answered another question");
                }
                decision => decision,
            },
        };
        if decision == HostKeyDecision::AcceptAndSave {
            if let Err(err) = self.record(facts, key) {
                return rejected(&format_args!("cannot record it in known_hosts: {err}"));
            }
        }
        decision
    }

    /// The host-key algorithms to offer `host`, those of its recorded keys
    /// — in known_hosts or the session's set — first, as OpenSSH orders
    /// them. A host with a known key is then asked for that one, so any
    /// other key it shows is a changed key and never just a type neither
    /// has seen.
    pub fn key_preference(&self, host: &str, port: u16) -> Cow<'static, [Algorithm]> {
        let default = Preferred::DEFAULT.key;
        let entries = self.entries(host, port).ok();
        let recorded = |key_type: &str| {
            entries
                .as_ref()
                .is_some_and(|entries| entries.has_key_type(key_type))
                || self.session_keys.has_key_type(key_type)
        };
        let (mut known, other): (Vec<_>, Vec<_>) = default
            .iter()
            .cloned()
            .partition(|algorithm| recorded(key_type(algorithm)));
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
            .field("session_keys", &self.session_keys)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
