//! Credential management — CredentialManager per doc 11 §CredentialManager.
//!
//! Lookup order: keyring → prompt → store-if-checked.
//!
//! Design contracts:
//!   - docs/11-ssh-auth-security.md §CredentialManager
//!   - docs/06-session-store.md §Credential Storage
//!
//! Secrets are `Zeroizing<Vec<u8>>` everywhere; never logged, never in SQLite,
//! never Debug-printed.

use std::fmt;
use std::sync::Arc;

use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// CredentialSource trait
// ---------------------------------------------------------------------------

/// A source of credentials, checked in priority order.
///
/// Two built-in implementations:
/// - `KeyringSource` — wraps libsecret (feature-gated behind `keyring`).
/// - `PromptSource` — wraps a closure for interactive prompting (or tests).
pub trait CredentialSource: Send + Sync {
    /// Look up a credential. Returns `None` if not found or unavailable.
    fn lookup(&self, session_id: &str, secret_type: &str) -> Option<Zeroizing<Vec<u8>>>;

    /// Store a credential (called after successful auth if user opted to save).
    /// Default: no-op (not all sources support storing).
    fn store(
        &self,
        _session_id: &str,
        _session_name: &str,
        _secret_type: &str,
        _secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        Ok(())
    }

    /// Human-readable name for logging (never logs secrets).
    fn source_name(&self) -> &str;
}

/// Errors from credential operations.
#[derive(Debug)]
pub enum CredentialError {
    /// The underlying store is unavailable (e.g. no secret service on D-Bus).
    Unavailable(String),
    /// A store/delete operation failed.
    StoreError(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(msg) => write!(f, "credential source unavailable: {msg}"),
            Self::StoreError(msg) => write!(f, "credential store error: {msg}"),
        }
    }
}

impl std::error::Error for CredentialError {}

// ---------------------------------------------------------------------------
// PromptSource
// ---------------------------------------------------------------------------

/// Closure signature for prompt-based credential sources.
///
/// `(session_id, secret_type) -> Option<secret_bytes>`. Returns `None` if the
/// user cancels.
pub type PromptFn = Arc<dyn Fn(&str, &str) -> Option<Zeroizing<Vec<u8>>> + Send + Sync>;

/// Prompt-based credential source. The closure is injected by the UI layer
/// (GTK dialog in Phase 3) or by tests.
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

impl fmt::Debug for PromptSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptSource")
            .field("prompt", &"<closure>")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// KeyringSource (feature-gated behind `keyring`)
// ---------------------------------------------------------------------------

/// Keyring-based credential source using libsecret.
///
/// Schema: `com.alex-eckstein.wayterm.Session`
/// Attributes: `session_id`, `session_name`, `secret_type`
///     (password | key_passphrase | vnc_password | rdp_password)
///
/// If the secret service is not running on D-Bus, `lookup()` returns `None`
/// silently. This ensures code compiles and runs without a secret service.
#[cfg(feature = "keyring")]
pub struct KeyringSource {
    // Schema ID per doc 06.
    _schema_id: &'static str,
}

#[cfg(feature = "keyring")]
impl KeyringSource {
    /// Schema ID from doc 06.
    pub const SCHEMA_ID: &'static str = "com.alex-eckstein.wayterm.Session";

    pub fn new() -> Self {
        Self {
            _schema_id: Self::SCHEMA_ID,
        }
    }
}

#[cfg(feature = "keyring")]
impl Default for KeyringSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "keyring")]
impl CredentialSource for KeyringSource {
    fn lookup(&self, _session_id: &str, _secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        // TODO: Phase 3 — wire libsecret::password_lookup_sync with the schema.
        // For now, return None to fall through to the prompt source. The
        // feature gate ensures we compile with or without libsecret; the
        // runtime check (secret service on D-Bus) is also a Phase 3 item.
        None
    }

    fn store(
        &self,
        _session_id: &str,
        _session_name: &str,
        _secret_type: &str,
        _secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        // TODO: Phase 3 — wire libsecret::password_store_sync.
        Err(CredentialError::Unavailable(
            "keyring store not wired in Phase 2".into(),
        ))
    }

    fn source_name(&self) -> &str {
        "keyring"
    }
}

#[cfg(feature = "keyring")]
impl fmt::Debug for KeyringSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyringSource")
            .field("schema_id", &Self::SCHEMA_ID)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// CredentialManager
// ---------------------------------------------------------------------------

/// Manages credential sources in priority order per doc 11 §CredentialManager.
///
/// Lookup walks sources in order; first hit wins. `store()` delegates to the
/// first source that supports storing (typically the keyring).
pub struct CredentialManager {
    sources: Vec<Box<dyn CredentialSource>>,
}

impl CredentialManager {
    /// Create a new CredentialManager with the given sources in priority order.
    ///
    /// Typical order: `[KeyringSource, PromptSource]`.
    pub fn new(sources: Vec<Box<dyn CredentialSource>>) -> Self {
        Self { sources }
    }

    /// Look up a credential from the first source that has it.
    pub fn lookup(&self, session_id: &str, secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        for source in &self.sources {
            if let Some(secret) = source.lookup(session_id, secret_type) {
                return Some(secret);
            }
        }
        None
    }

    /// Store a credential in the first source that supports it.
    pub fn store(
        &self,
        session_id: &str,
        session_name: &str,
        secret_type: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        for source in &self.sources {
            match source.store(session_id, session_name, secret_type, secret) {
                Ok(()) => return Ok(()),
                Err(CredentialError::Unavailable(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(CredentialError::Unavailable(
            "no credential source supports storing".into(),
        ))
    }

    /// Number of configured sources.
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }
}

// Debug impl: never print secrets, just source names.
impl fmt::Debug for CredentialManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.sources.iter().map(|s| s.source_name()).collect();
        f.debug_struct("CredentialManager")
            .field("sources", &names)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock credential source for testing.
    struct MockSource {
        name: &'static str,
        secret: Option<Zeroizing<Vec<u8>>>,
        supports_store: bool,
    }

    impl MockSource {
        fn with_secret(name: &'static str, secret: &[u8]) -> Self {
            Self {
                name,
                secret: Some(Zeroizing::new(secret.to_vec())),
                supports_store: false,
            }
        }

        fn empty(name: &'static str) -> Self {
            Self {
                name,
                secret: None,
                supports_store: false,
            }
        }

        fn storable(name: &'static str) -> Self {
            Self {
                name,
                secret: None,
                supports_store: true,
            }
        }
    }

    impl CredentialSource for MockSource {
        fn lookup(&self, _session_id: &str, _secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
            self.secret.clone()
        }

        fn store(
            &self,
            _session_id: &str,
            _session_name: &str,
            _secret_type: &str,
            _secret: &Zeroizing<Vec<u8>>,
        ) -> Result<(), CredentialError> {
            if self.supports_store {
                Ok(())
            } else {
                Err(CredentialError::Unavailable(format!(
                    "{} does not support store",
                    self.name
                )))
            }
        }

        fn source_name(&self) -> &str {
            self.name
        }
    }

    #[test]
    fn lookup_returns_first_hit() {
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::empty("source-1")),
            Box::new(MockSource::with_secret("source-2", b"secret2")),
            Box::new(MockSource::with_secret("source-3", b"secret3")),
        ]);

        let result = mgr.lookup("sess-1", "password").unwrap();
        assert_eq!(&*result, b"secret2");
    }

    #[test]
    fn lookup_falls_through_when_all_empty() {
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::empty("a")),
            Box::new(MockSource::empty("b")),
        ]);

        assert!(mgr.lookup("sess-1", "password").is_none());
    }

    #[test]
    fn lookup_priority_order() {
        // First source has the secret → should win.
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::with_secret("keyring", b"keyring-pw")),
            Box::new(MockSource::with_secret("prompt", b"prompt-pw")),
        ]);

        let result = mgr.lookup("sess-1", "password").unwrap();
        assert_eq!(&*result, b"keyring-pw");
    }

    #[test]
    fn store_delegates_to_first_storable() {
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::empty("non-storable")),
            Box::new(MockSource::storable("storable")),
        ]);

        let secret = Zeroizing::new(b"test-secret".to_vec());
        let result = mgr.store("sess-1", "My Session", "password", &secret);
        assert!(result.is_ok());
    }

    #[test]
    fn store_fails_when_no_source_supports_it() {
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::empty("a")),
            Box::new(MockSource::empty("b")),
        ]);

        let secret = Zeroizing::new(b"test".to_vec());
        let result = mgr.store("sess-1", "My Session", "password", &secret);
        assert!(result.is_err());
    }

    #[test]
    fn prompt_source_works() {
        let source = PromptSource::new(Arc::new(|_session_id, _secret_type| {
            Some(Zeroizing::new(b"prompted-password".to_vec()))
        }));

        let result = source.lookup("sess-1", "password").unwrap();
        assert_eq!(&*result, b"prompted-password");
    }

    #[test]
    fn prompt_source_can_cancel() {
        let source = PromptSource::new(Arc::new(|_, _| None));
        assert!(source.lookup("sess-1", "password").is_none());
    }

    #[test]
    fn debug_impl_redacts_secrets() {
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::with_secret("keyring", b"supersecret")),
            Box::new(MockSource::empty("prompt")),
        ]);

        let dbg = format!("{mgr:?}");
        assert!(dbg.contains("keyring"));
        assert!(dbg.contains("prompt"));
        assert!(!dbg.contains("supersecret"));
    }

    #[test]
    fn source_count() {
        let mgr = CredentialManager::new(vec![
            Box::new(MockSource::empty("a")),
            Box::new(MockSource::empty("b")),
            Box::new(MockSource::empty("c")),
        ]);
        assert_eq!(mgr.source_count(), 3);
    }

    #[test]
    fn zeroizing_secret_is_zeroed_on_drop() {
        // This test verifies the API contract: Zeroizing<Vec<u8>> zeroes on
        // drop. We can't inspect freed memory, but we verify the type
        // compiles correctly and the Zeroizing wrapper works as expected.
        let secret = Zeroizing::new(b"temporary-secret".to_vec());
        assert_eq!(&*secret, b"temporary-secret");
        // When `secret` goes out of scope, Zeroizing::drop() zeroes the Vec.
        // The zeroize crate guarantees this behaviour; we trust the crate
        // and verify the API contract compiles.
        drop(secret);
    }

    #[cfg(feature = "keyring")]
    #[test]
    fn keyring_source_creation() {
        let source = KeyringSource::new();
        assert_eq!(source.source_name(), "keyring");
        assert_eq!(
            KeyringSource::SCHEMA_ID,
            "com.alex-eckstein.wayterm.Session"
        );
    }

    #[cfg(feature = "keyring")]
    #[test]
    fn keyring_source_lookup_returns_none_when_not_wired() {
        // Phase 2: keyring lookup is stubbed → returns None.
        let source = KeyringSource::new();
        assert!(source.lookup("sess-1", "password").is_none());
    }

    #[cfg(feature = "keyring")]
    #[test]
    fn keyring_source_debug() {
        let source = KeyringSource::new();
        let dbg = format!("{source:?}");
        assert!(dbg.contains("com.alex-eckstein.wayterm.Session"));
    }
}
