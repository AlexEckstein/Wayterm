use std::sync::Mutex;

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

    // The keyring refuses the id-less placeholder before reaching any
    // backend, so this holds with or without a live Secret Service.
    let none = CredentialManager::new(vec![
        Box::new(KeyringSource::default()) as Box<dyn CredentialSource>,
        Box::new(PromptSource::new(Arc::new(|_, _| None))),
    ]);
    assert!(matches!(
        none.store(LIVE_CONNECT_SESSION_ID, "S", "password", &secret),
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

// ---------------------------------------------------------------------------
// KeyringSource, against an in-memory keyring (no Secret Service needed)
// ---------------------------------------------------------------------------

/// Secret Service semantics: a search matches items carrying every
/// queried attribute with an equal value; creating with `replace` swaps out
/// an item with identical attributes.
#[derive(Default)]
struct FakeKeyring {
    items: Mutex<Vec<(String, Attributes, Vec<u8>)>>,
    /// Every call fails with this reason (backend unreachable).
    broken: Option<&'static str>,
}

impl FakeKeyring {
    fn broken(reason: &'static str) -> Arc<Self> {
        Arc::new(Self {
            broken: Some(reason),
            ..Self::default()
        })
    }

    /// An item as the real schema (or another libsecret client) writes it.
    fn seed(&self, attributes: &[(&str, &str)], secret: &[u8]) {
        let attributes = attributes
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let item = ("seeded".to_string(), attributes, secret.to_vec());
        self.items.lock().unwrap().push(item);
    }

    fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    fn check(&self) -> Result<(), String> {
        self.broken.map_or(Ok(()), |reason| Err(reason.to_string()))
    }
}

fn matches(item: &Attributes, query: &Attributes) -> bool {
    query.iter().all(|(k, v)| item.get(k) == Some(v))
}

impl KeyringBackend for FakeKeyring {
    fn probe(&self) -> Result<(), String> {
        self.check()
    }

    fn find(&self, attributes: &Attributes) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        self.check()?;
        let items = self.items.lock().unwrap();
        Ok(items
            .iter()
            .find(|(_, item, _)| matches(item, attributes))
            .map(|(_, _, secret)| Zeroizing::new(secret.clone())))
    }

    fn create(&self, label: &str, attributes: &Attributes, secret: &[u8]) -> Result<(), String> {
        self.check()?;
        let mut items = self.items.lock().unwrap();
        items.retain(|(_, item, _)| item != attributes);
        items.push((label.to_string(), attributes.clone(), secret.to_vec()));
        Ok(())
    }

    fn delete(&self, attributes: &Attributes) -> Result<usize, String> {
        self.check()?;
        let mut items = self.items.lock().unwrap();
        let before = items.len();
        items.retain(|(_, item, _)| !matches(item, attributes));
        Ok(before - items.len())
    }
}

const SCHEMA_ATTR: (&str, &str) = ("xdg:schema", "com.alex-eckstein.wayterm.Session");
const APP_ATTR: (&str, &str) = ("application", "com.alex-eckstein.wayterm");

fn keyring(fake: &Arc<FakeKeyring>) -> KeyringSource {
    KeyringSource::with_backend("42", "prod-db", Arc::clone(fake) as Arc<dyn KeyringBackend>)
}

fn secret(bytes: &[u8]) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(bytes.to_vec())
}

#[test]
fn attribute_maps_follow_the_schema() {
    let owned = |pairs: &[(&str, &str)]| -> Attributes {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(
        store_attributes("42", "prod-db", "password"),
        owned(&[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_id", "42"),
            ("session_name", "prod-db"),
            ("secret_type", "password"),
        ])
    );
    assert_eq!(
        search_attributes(KeyedBy::Id("42"), "key_passphrase"),
        owned(&[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_id", "42"),
            ("secret_type", "key_passphrase"),
        ])
    );
    assert_eq!(
        search_attributes(KeyedBy::Name("prod-db"), "vnc_password"),
        owned(&[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_name", "prod-db"),
            ("secret_type", "vnc_password"),
        ])
    );
    assert_eq!(
        item_label("prod-db", "password"),
        "Wayterm — prod-db (password)"
    );
}

#[test]
fn lookup_finds_the_entry_stored_for_this_session_id() {
    let fake = Arc::new(FakeKeyring::default());
    fake.seed(
        &[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_id", "42"),
            ("session_name", "other-name"),
            ("secret_type", "password"),
        ],
        b"test-secret",
    );
    let source = keyring(&fake);
    assert_eq!(&*source.lookup("42", "password").unwrap(), b"test-secret");
    assert!(source.lookup("42", "key_passphrase").is_none());
    assert!(source.lookup("43", "password").is_none(), "other session");
}

#[test]
fn lookup_falls_back_to_the_exact_session_name() {
    let fake = Arc::new(FakeKeyring::default());
    fake.seed(
        &[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_name", "prod-db"),
            ("secret_type", "password"),
        ],
        b"by-name",
    );
    fake.seed(
        &[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_name", "prod"),
            ("secret_type", "key_passphrase"),
        ],
        b"substring",
    );
    let source = keyring(&fake);
    assert_eq!(&*source.lookup("42", "password").unwrap(), b"by-name");
    // "prod" is a prefix of "prod-db": equality only, never a substring.
    assert!(source.lookup("42", "key_passphrase").is_none());
    // The name belongs to session 42; another id does not inherit it.
    assert!(source.lookup("43", "password").is_none());
}

#[test]
fn the_session_id_entry_wins_over_the_name_entry() {
    let fake = Arc::new(FakeKeyring::default());
    let name_only = [
        SCHEMA_ATTR,
        APP_ATTR,
        ("session_name", "prod-db"),
        ("secret_type", "password"),
    ];
    fake.seed(&name_only, b"by-name");
    keyring(&fake)
        .store("42", "renamed", "password", &secret(b"by-id"))
        .unwrap();
    assert_eq!(&*keyring(&fake).lookup("42", "password").unwrap(), b"by-id");
}

#[test]
fn an_unreachable_keyring_is_a_miss_not_a_secret() {
    let fake = FakeKeyring::broken("no Secret Service on the bus");
    let source = keyring(&fake);
    assert!(source.lookup("42", "password").is_none());

    let mgr = CredentialManager::new(vec![
        Box::new(keyring(&fake)),
        MockSource::boxed("prompt", Some(b"prompted")),
    ]);
    assert_eq!(&*mgr.lookup("42", "password").unwrap(), b"prompted");
}

#[test]
fn the_placeholder_session_id_never_reaches_the_keyring() {
    let fake = Arc::new(FakeKeyring::default());
    let source = KeyringSource::with_backend(
        LIVE_CONNECT_SESSION_ID,
        "quick",
        Arc::clone(&fake) as Arc<dyn KeyringBackend>,
    );
    let stored = source.store(LIVE_CONNECT_SESSION_ID, "quick", "password", &secret(b"x"));
    assert!(matches!(stored, Err(CredentialError::Unavailable(_))));
    assert_eq!(fake.len(), 0);

    fake.seed(
        &[
            SCHEMA_ATTR,
            APP_ATTR,
            ("session_id", LIVE_CONNECT_SESSION_ID),
            ("session_name", "quick"),
            ("secret_type", "password"),
        ],
        b"shared",
    );
    assert!(source.lookup(LIVE_CONNECT_SESSION_ID, "password").is_none());
    assert!(source
        .delete_entry(LIVE_CONNECT_SESSION_ID, "password")
        .is_ok());
    assert_eq!(fake.len(), 1, "placeholder entries are left alone");
}

#[test]
fn store_writes_one_labelled_item_and_replaces_it() {
    let fake = Arc::new(FakeKeyring::default());
    let source = keyring(&fake);
    source
        .store("42", "prod-db", "password", &secret(b"first"))
        .unwrap();
    source
        .store("42", "prod-db", "password", &secret(b"second"))
        .unwrap();

    let items = fake.items.lock().unwrap();
    assert_eq!(items.len(), 1, "replace = true");
    let (label, attributes, stored) = &items[0];
    assert_eq!(label, "Wayterm — prod-db (password)");
    assert_eq!(attributes, &store_attributes("42", "prod-db", "password"));
    assert_eq!(stored, b"second");
}

#[test]
fn store_failures_carry_the_backend_reason() {
    let source = keyring(&FakeKeyring::broken("collection is locked"));
    match source.store("42", "prod-db", "password", &secret(b"x")) {
        Err(CredentialError::StoreError(reason)) => assert!(reason.contains("locked")),
        other => panic!("expected StoreError, got {other:?}"),
    }
    // A real error stops the manager's walk instead of skipping ahead.
    let storable = Box::new(MockSource {
        name: "storable",
        secret: None,
        storable: true,
    });
    let mgr = CredentialManager::new(vec![Box::new(source), storable]);
    let stored = mgr.store("42", "prod-db", "password", &secret(b"x"));
    assert!(matches!(stored, Err(CredentialError::StoreError(_))));
}

#[test]
fn delete_entry_removes_by_id_then_by_name() {
    let fake = Arc::new(FakeKeyring::default());
    let source = keyring(&fake);
    source
        .store("42", "prod-db", "password", &secret(b"mine"))
        .unwrap();
    source
        .store("43", "staging", "password", &secret(b"theirs"))
        .unwrap();
    let name_only = [
        SCHEMA_ATTR,
        APP_ATTR,
        ("session_name", "prod-db"),
        ("secret_type", "password"),
    ];
    fake.seed(&name_only, b"by-name");

    source.delete_entry("42", "password").unwrap();
    assert_eq!(fake.len(), 2, "the id entry goes; the name entry waits");
    assert_eq!(&*source.lookup("42", "password").unwrap(), b"by-name");

    source.delete_entry("42", "password").unwrap();
    assert!(source.lookup("42", "password").is_none());
    assert_eq!(&*source.lookup("43", "password").unwrap(), b"theirs");

    // Nothing left to delete is not an error.
    assert!(source.delete_entry("42", "password").is_ok());
}

#[test]
fn delete_entry_failures_are_store_errors() {
    let source = keyring(&FakeKeyring::broken("no Secret Service on the bus"));
    assert!(matches!(
        source.delete_entry("42", "password"),
        Err(CredentialError::StoreError(_))
    ));
}

#[test]
fn available_tells_unreachable_from_empty() {
    assert!(keyring(&Arc::new(FakeKeyring::default())).available());
    assert!(!keyring(&FakeKeyring::broken("no Secret Service on the bus")).available());
}

#[test]
fn for_session_carries_the_id_the_auth_chain_uses() {
    assert_eq!(
        CredentialManager::new(vec![]).session_id(),
        LIVE_CONNECT_SESSION_ID
    );
    assert_eq!(
        CredentialManager::for_session("42", vec![]).session_id(),
        "42"
    );
}

#[test]
fn keyring_debug_never_shows_stored_secrets() {
    let fake = Arc::new(FakeKeyring::default());
    let source = keyring(&fake);
    source
        .store("42", "prod-db", "password", &secret(b"test-secret"))
        .unwrap();
    let debug = format!("{source:?}");
    assert!(debug.contains("42") && debug.contains("prod-db"));
    assert!(!debug.contains("test-secret"));

    let mgr = CredentialManager::for_session("42", vec![Box::new(source)]);
    let debug = format!("{mgr:?}");
    assert!(debug.contains("keyring") && debug.contains("42"));
    assert!(!debug.contains("test-secret"));
}

// ---------------------------------------------------------------------------
// BitwardenSource, against the fake bw under tests/fixtures/ (no real CLI)
// ---------------------------------------------------------------------------

fn fake_bw() -> String {
    format!("{}/tests/fixtures/fake-bw", env!("CARGO_MANIFEST_DIR"))
}

fn bitwarden(session_name: &str, key: Option<&str>) -> BitwardenSource {
    BitwardenSource::with_session_key(
        &fake_bw(),
        session_name,
        key.map(|k| Zeroizing::new(k.to_string())),
    )
}

#[test]
fn bitwarden_item_is_prefixed_session_name() {
    assert_eq!(bitwarden_item_name("prod db"), "wayterm/prod db");
}

#[test]
fn bitwarden_lookup_returns_trimmed_stdout() {
    let secret = bitwarden("ok", None).lookup("1", "password").unwrap();
    assert_eq!(&*secret, b"test-secret-XYZ");
}

#[test]
fn bitwarden_passes_the_session_key_through_the_environment() {
    // The fixture exits non-zero if the key shows up in argv.
    let with_key = bitwarden("needs-session", Some("fake-session-key"));
    assert_eq!(
        &*with_key.lookup("1", "password").unwrap(),
        b"test-secret-XYZ"
    );
    assert!(bitwarden("needs-session", None)
        .lookup("1", "password")
        .is_none());
}

#[test]
fn bitwarden_missing_binary_is_a_miss() {
    let source = BitwardenSource::with_session_key(
        "/nonexistent/wayterm-test/bw",
        "ok",
        Some(Zeroizing::new("fake-session-key".into())),
    );
    assert!(source.lookup("1", "password").is_none());
}

#[test]
fn bitwarden_non_zero_exit_and_empty_output_are_misses() {
    assert!(bitwarden("missing-item", None)
        .lookup("1", "password")
        .is_none());
    assert!(bitwarden("empty", None).lookup("1", "password").is_none());
}

#[test]
fn bitwarden_timeout_kills_the_child_and_misses() {
    let source = bitwarden("slow", None).with_timeout(Duration::from_millis(300));
    let started = Instant::now();
    assert!(source.lookup("1", "password").is_none());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "child was killed"
    );
}

#[test]
fn bitwarden_is_read_only() {
    let secret = Zeroizing::new(b"x".to_vec());
    assert!(matches!(
        bitwarden("ok", None).store("1", "ok", "password", &secret),
        Err(CredentialError::Unavailable(_))
    ));
}

#[test]
fn bitwarden_status_probe_never_gets_the_session_key() {
    // `bitwarden_status` strips BW_SESSION, so the fixture's guard
    // (exit 91 when it is set) never trips even if the parent has it.
    assert_eq!(
        bitwarden_status(&fake_bw()),
        BitwardenStatus::Detected {
            state: Some("locked".into())
        }
    );
    assert!(matches!(
        bitwarden_status("/nonexistent/wayterm-test/bw"),
        BitwardenStatus::NotDetected(_)
    ));
}

#[test]
fn bitwarden_status_classification() {
    assert_eq!(
        classify_bitwarden_status(true, br#"{"status":"unauthenticated"}"#),
        BitwardenStatus::Detected {
            state: Some("unauthenticated".into())
        }
    );
    assert_eq!(
        classify_bitwarden_status(true, b"not json"),
        BitwardenStatus::Detected { state: None }
    );
    assert!(matches!(
        classify_bitwarden_status(false, b""),
        BitwardenStatus::NotDetected(_)
    ));
}
