//! Credential lookup (doc 11 §CredentialManager): keyring → prompt →
//! store-if-checked. Secrets are `Zeroizing<Vec<u8>>` and are never logged,
//! `Debug`-printed, or written to SQLite.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use zeroize::Zeroizing;

/// One place credentials can come from, consulted in priority order.
pub trait CredentialSource: Send + Sync {
    /// `None` when the source has nothing (or the user cancelled a prompt).
    fn lookup(&self, session_id: &str, secret_type: &str) -> Option<Zeroizing<Vec<u8>>>;

    /// Persist a credential the user chose to save. Sources that cannot
    /// store report [`CredentialError::Unavailable`].
    fn store(
        &self,
        _session_id: &str,
        _session_name: &str,
        _secret_type: &str,
        _secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(format!(
            "{} cannot store credentials",
            self.source_name()
        )))
    }

    /// Name for diagnostics — never a secret.
    fn source_name(&self) -> &str;
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential source unavailable: {0}")]
    Unavailable(String),
    #[error("credential store error: {0}")]
    StoreError(String),
}

/// `(session_id, secret_type) -> secret`; `None` = the user cancelled.
pub type PromptFn = Arc<dyn Fn(&str, &str) -> Option<Zeroizing<Vec<u8>>> + Send + Sync>;

/// Interactive source: the GTK dialog in the app, a closure in tests.
pub struct PromptSource {
    prompt: PromptFn,
}

impl PromptSource {
    pub fn new(prompt: PromptFn) -> Self {
        Self { prompt }
    }
}

impl CredentialSource for PromptSource {
    fn lookup(&self, session_id: &str, secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        (self.prompt)(session_id, secret_type)
    }

    fn source_name(&self) -> &str {
        "prompt"
    }
}

/// Session id where no saved session exists (the [`CredentialManager`]
/// default). The keyring never stores or serves secrets under it: every
/// id-less connect would otherwise share one entry.
pub const LIVE_CONNECT_SESSION_ID: &str = "_live_connect";

/// libsecret schema and application id (doc 06 §Credential Storage);
/// changing either orphans stored credentials.
const SCHEMA: &str = "com.alex-eckstein.wayterm.Session";
const APPLICATION: &str = "com.alex-eckstein.wayterm";

type Attributes = HashMap<String, String>;

/// The identity attribute a keyring search keys on.
enum KeyedBy<'a> {
    Id(&'a str),
    Name(&'a str),
}

/// Exact attribute equality, as Secret Service search is — never a
/// substring match.
fn search_attributes(key: KeyedBy<'_>, secret_type: &str) -> Attributes {
    let (name, value) = match key {
        KeyedBy::Id(id) => ("session_id", id),
        KeyedBy::Name(session_name) => ("session_name", session_name),
    };
    HashMap::from([
        (oo7::XDG_SCHEMA_ATTRIBUTE.to_string(), SCHEMA.to_string()),
        ("application".to_string(), APPLICATION.to_string()),
        (name.to_string(), value.to_string()),
        ("secret_type".to_string(), secret_type.to_string()),
    ])
}

/// Stored items carry both identities, so either search finds them.
fn store_attributes(session_id: &str, session_name: &str, secret_type: &str) -> Attributes {
    let mut attributes = search_attributes(KeyedBy::Id(session_id), secret_type);
    attributes.insert("session_name".to_string(), session_name.to_string());
    attributes
}

fn item_label(session_name: &str, secret_type: &str) -> String {
    format!("Wayterm — {session_name} ({secret_type})")
}

/// The keyring operations [`KeyringSource`] needs: oo7 in the app, an
/// in-memory fake in tests. An `Err` carries the backend's reason, never a
/// secret.
trait KeyringBackend: Send + Sync {
    /// `Err` when the keyring cannot be opened.
    fn probe(&self) -> Result<(), String>;
    /// The secret of the first item matching every attribute.
    fn find(&self, attributes: &Attributes) -> Result<Option<Zeroizing<Vec<u8>>>, String>;
    /// Create or replace the item with exactly these attributes.
    fn create(&self, label: &str, attributes: &Attributes, secret: &[u8]) -> Result<(), String>;
    /// Delete every item matching; returns how many there were.
    fn delete(&self, attributes: &Attributes) -> Result<usize, String>;
}

/// The Secret Service over D-Bus, or the portal-backed file keyring when
/// sandboxed (Flatpak) — `oo7::Keyring::new` picks.
struct Oo7Backend;

impl Oo7Backend {
    /// Drive one oo7 call to completion on this thread (callers run on the
    /// blocking pool). oo7's tokio backend spawns onto the ambient
    /// runtime, so a thread outside one reports an error instead of
    /// panicking.
    fn run<T, F>(op: impl FnOnce(oo7::Keyring) -> F) -> Result<T, String>
    where
        F: Future<Output = oo7::Result<T>>,
    {
        tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime on this thread".to_string())?;
        pollster::block_on(async { op(oo7::Keyring::new().await?).await })
            .map_err(|err| err.to_string())
    }
}

impl KeyringBackend for Oo7Backend {
    fn probe(&self) -> Result<(), String> {
        Self::run(|_| async { Ok(()) })
    }

    fn find(&self, attributes: &Attributes) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        Self::run(|keyring| async move {
            let Some(item) = keyring.search_items(attributes).await?.into_iter().next() else {
                return Ok(None);
            };
            // oo7 zeroizes its own buffer on drop; this is the one copy.
            let secret = item.secret().await?;
            Ok(Some(Zeroizing::new(secret.as_bytes().to_vec())))
        })
    }

    fn create(&self, label: &str, attributes: &Attributes, secret: &[u8]) -> Result<(), String> {
        Self::run(
            |keyring| async move { keyring.create_item(label, attributes, secret, true).await },
        )
    }

    fn delete(&self, attributes: &Attributes) -> Result<usize, String> {
        Self::run(|keyring| async move {
            let items = keyring.search_items(attributes).await?;
            for item in &items {
                item.delete().await?;
            }
            Ok(items.len())
        })
    }
}

/// The OS keyring (libsecret schema `com.alex-eckstein.wayterm.Session`,
/// doc 06) for one saved session. Lookups key on the session id, then fall
/// back to the session name (items made by other tools); an unreachable
/// keyring is a miss, so the prompt source answers.
pub struct KeyringSource {
    session_id: String,
    session_name: String,
    backend: Arc<dyn KeyringBackend>,
}

impl KeyringSource {
    pub fn new(session_id: &str, session_name: &str) -> Self {
        Self::with_backend(session_id, session_name, Arc::new(Oo7Backend))
    }

    fn with_backend(
        session_id: &str,
        session_name: &str,
        backend: Arc<dyn KeyringBackend>,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            session_name: session_name.to_string(),
            backend,
        }
    }

    /// Whether the keyring can be opened: tells "nothing stored" (a miss
    /// while available) from "backend unreachable".
    pub fn available(&self) -> bool {
        self.backend.probe().is_ok()
    }

    /// Delete the session's `secret_type` entries: those keyed on the id,
    /// else those keyed on the session name.
    pub fn delete_entry(&self, session_id: &str, secret_type: &str) -> Result<(), CredentialError> {
        if session_id == LIVE_CONNECT_SESSION_ID {
            return Ok(());
        }
        let by_id = search_attributes(KeyedBy::Id(session_id), secret_type);
        let deleted = self
            .backend
            .delete(&by_id)
            .map_err(CredentialError::StoreError)?;
        if let (0, Some(name)) = (deleted, self.name_fallback(session_id)) {
            let by_name = search_attributes(KeyedBy::Name(name), secret_type);
            self.backend
                .delete(&by_name)
                .map_err(CredentialError::StoreError)?;
        }
        Ok(())
    }

    /// The name only stands in for this source's own session.
    fn name_fallback(&self, session_id: &str) -> Option<&str> {
        (session_id == self.session_id && !self.session_name.is_empty())
            .then_some(self.session_name.as_str())
    }
}

impl Default for KeyringSource {
    fn default() -> Self {
        Self::new(LIVE_CONNECT_SESSION_ID, "")
    }
}

impl fmt::Debug for KeyringSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyringSource")
            .field("session_id", &self.session_id)
            .field("session_name", &self.session_name)
            .finish_non_exhaustive()
    }
}

impl CredentialSource for KeyringSource {
    fn lookup(&self, session_id: &str, secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        if session_id == LIVE_CONNECT_SESSION_ID {
            return None;
        }
        let name = self.name_fallback(session_id).map(KeyedBy::Name);
        for key in std::iter::once(KeyedBy::Id(session_id)).chain(name) {
            match self.backend.find(&search_attributes(key, secret_type)) {
                Ok(Some(secret)) => return Some(secret),
                Ok(None) => {}
                Err(reason) => {
                    eprintln!("Wayterm: keyring: unavailable ({reason}); prompting instead");
                    return None;
                }
            }
        }
        None
    }

    fn store(
        &self,
        session_id: &str,
        session_name: &str,
        secret_type: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        if session_id == LIVE_CONNECT_SESSION_ID {
            return Err(CredentialError::Unavailable(
                "keyring: no saved session to store under".into(),
            ));
        }
        self.backend
            .create(
                &item_label(session_name, secret_type),
                &store_attributes(session_id, session_name, secret_type),
                secret,
            )
            .map_err(CredentialError::StoreError)
    }

    fn source_name(&self) -> &str {
        "keyring"
    }
}

/// Sources in priority order; the first hit wins.
pub struct CredentialManager {
    sources: Vec<Box<dyn CredentialSource>>,
    session_id: String,
}

impl CredentialManager {
    /// A manager for no saved session ([`LIVE_CONNECT_SESSION_ID`]).
    pub fn new(sources: Vec<Box<dyn CredentialSource>>) -> Self {
        Self::for_session(LIVE_CONNECT_SESSION_ID, sources)
    }

    /// A manager for saved session `session_id`; the SSH auth chain looks
    /// secrets up under [`Self::session_id`].
    pub fn for_session(session_id: &str, sources: Vec<Box<dyn CredentialSource>>) -> Self {
        Self {
            sources,
            session_id: session_id.to_string(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn lookup(&self, session_id: &str, secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        self.sources
            .iter()
            .find_map(|source| source.lookup(session_id, secret_type))
    }

    /// Store in the first source able to; an error other than
    /// `Unavailable` stops the walk.
    pub fn store(
        &self,
        session_id: &str,
        session_name: &str,
        secret_type: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        for source in &self.sources {
            match source.store(session_id, session_name, secret_type, secret) {
                Err(CredentialError::Unavailable(_)) => continue,
                result => return result,
            }
        }
        Err(CredentialError::Unavailable(
            "no credential source supports storing".into(),
        ))
    }
}

impl fmt::Debug for CredentialManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.sources.iter().map(|s| s.source_name()).collect();
        f.debug_struct("CredentialManager")
            .field("session_id", &self.session_id)
            .field("sources", &names)
            .finish()
    }
}

#[cfg(test)]
mod tests;
