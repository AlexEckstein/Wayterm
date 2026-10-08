//! Credential lookup (doc 11 §CredentialManager): keyring → prompt →
//! store-if-checked. Secrets are `Zeroizing<Vec<u8>>` and are never logged,
//! `Debug`-printed, or written to SQLite.

use std::fmt;
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

/// The OS keyring (libsecret schema `com.alex-eckstein.wayterm.Session`,
/// doc 06). Not wired yet: lookups miss, so the prompt source answers.
#[derive(Debug, Default)]
pub struct KeyringSource;

impl CredentialSource for KeyringSource {
    fn lookup(&self, _session_id: &str, _secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        None
    }

    fn source_name(&self) -> &str {
        "keyring"
    }
}

/// Sources in priority order; the first hit wins.
pub struct CredentialManager {
    sources: Vec<Box<dyn CredentialSource>>,
}

impl CredentialManager {
    pub fn new(sources: Vec<Box<dyn CredentialSource>>) -> Self {
        Self { sources }
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
            .field("sources", &names)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSource {
        name: &'static str,
        secret: Option<&'static [u8]>,
        storable: bool,
    }

    impl MockSource {
        fn boxed(name: &'static str, secret: Option<&'static [u8]>) -> Box<dyn CredentialSource> {
            Box::new(Self {
                name,
                secret,
                storable: false,
            })
        }
    }

    impl CredentialSource for MockSource {
        fn lookup(&self, _: &str, _: &str) -> Option<Zeroizing<Vec<u8>>> {
            self.secret.map(|s| Zeroizing::new(s.to_vec()))
        }

        fn store(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &Zeroizing<Vec<u8>>,
        ) -> Result<(), CredentialError> {
            if self.storable {
                Ok(())
            } else {
                Err(CredentialError::Unavailable(self.name.into()))
            }
        }

        fn source_name(&self) -> &str {
            self.name
        }
    }

    #[test]
    fn lookup_returns_the_first_hit_in_priority_order() {
        let mgr = CredentialManager::new(vec![
            MockSource::boxed("empty", None),
            MockSource::boxed("keyring", Some(b"keyring-pw")),
            MockSource::boxed("prompt", Some(b"prompt-pw")),
        ]);
        assert_eq!(&*mgr.lookup("s", "password").unwrap(), b"keyring-pw");

        let empty = CredentialManager::new(vec![MockSource::boxed("a", None)]);
        assert!(empty.lookup("s", "password").is_none());
    }

    #[test]
    fn store_uses_the_first_source_that_can() {
        let secret = Zeroizing::new(b"x".to_vec());
        let storable = Box::new(MockSource {
            name: "storable",
            secret: None,
            storable: true,
        });
        let mgr = CredentialManager::new(vec![MockSource::boxed("prompt", None), storable]);
        assert!(mgr.store("s", "S", "password", &secret).is_ok());

        let none = CredentialManager::new(vec![
            Box::new(KeyringSource) as Box<dyn CredentialSource>,
            Box::new(PromptSource::new(Arc::new(|_, _| None))),
        ]);
        assert!(matches!(
            none.store("s", "S", "password", &secret),
            Err(CredentialError::Unavailable(_))
        ));
    }

    #[test]
    fn debug_names_sources_but_never_secrets() {
        let mgr = CredentialManager::new(vec![MockSource::boxed("keyring", Some(b"supersecret"))]);
        let debug = format!("{mgr:?}");
        assert!(debug.contains("keyring"));
        assert!(!debug.contains("supersecret"));
    }
}
