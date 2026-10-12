//! Credential lookup (doc 11 §CredentialManager): keyring → Bitwarden (when
//! enabled) → prompt → store-if-checked. Secrets are `Zeroizing<Vec<u8>>`
//! and are never logged, `Debug`-printed, or written to SQLite.

use std::fmt;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// How long one `bw` invocation may run before it is killed.
pub const BITWARDEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The Bitwarden item a session resolves: `wayterm/<session_name>`.
pub fn bitwarden_item_name(session_name: &str) -> String {
    format!("wayterm/{session_name}")
}

/// Read-only source backed by the Bitwarden CLI (doc 11 §Credential
/// sources): one `bw get password` per lookup, nothing cached. The vault
/// session key comes from `BW_SESSION` at construction and reaches the
/// child through its environment only — argv is world-readable in
/// `/proc/<pid>/cmdline`.
pub struct BitwardenSource {
    bin: String,
    item: String,
    session_key: Option<Zeroizing<String>>,
    timeout: Duration,
}

impl BitwardenSource {
    /// A source for `session_name`, running `bin`, with the user's
    /// `BW_SESSION` (if any).
    pub fn new(bin: &str, session_name: &str) -> Self {
        let session_key = std::env::var("BW_SESSION").ok().map(Zeroizing::new);
        Self::with_session_key(bin, session_name, session_key)
    }

    /// As [`Self::new`], with the session key given rather than read from
    /// the environment.
    pub fn with_session_key(
        bin: &str,
        session_name: &str,
        session_key: Option<Zeroizing<String>>,
    ) -> Self {
        Self {
            bin: bin.to_string(),
            item: bitwarden_item_name(session_name),
            session_key,
            timeout: BITWARDEN_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl CredentialSource for BitwardenSource {
    /// The item's password answers whatever secret the session's method
    /// asks for (a session has one method, so one secret).
    fn lookup(&self, _session_id: &str, _secret_type: &str) -> Option<Zeroizing<Vec<u8>>> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["get", "password", "--raw", &self.item, "--nointeraction"]);
        match &self.session_key {
            Some(key) => cmd.env("BW_SESSION", key.as_str()),
            None => cmd.env_remove("BW_SESSION"),
        };
        let (status, stdout) = match run_with_timeout(cmd, self.timeout) {
            Ok(done) => done,
            Err(err) => {
                eprintln!("Wayterm: bitwarden lookup for '{}': {err}", self.item);
                return None;
            }
        };
        if !status.success() {
            eprintln!(
                "Wayterm: bitwarden lookup for '{}' failed ({status})",
                self.item
            );
            return None;
        }
        let text = Zeroizing::new(String::from_utf8_lossy(&stdout).into_owned());
        drop(stdout);
        let secret = text.trim();
        if secret.is_empty() {
            eprintln!("Wayterm: bitwarden item '{}' has no password", self.item);
            return None;
        }
        Some(Zeroizing::new(secret.as_bytes().to_vec()))
    }

    fn store(
        &self,
        _session_id: &str,
        _session_name: &str,
        _secret_type: &str,
        _secret: &Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(
            "bitwarden is read-only".into(),
        ))
    }

    fn source_name(&self) -> &str {
        "bitwarden"
    }
}

/// What `bw status` says, reduced to what the Settings page shows. The
/// probe runs without `BW_SESSION`, so it never holds a secret, and only
/// the `status` field of its output is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitwardenStatus {
    /// The CLI ran; `state` is its `status` field (`unauthenticated`,
    /// `locked`, `unlocked`) when it printed one.
    Detected { state: Option<String> },
    /// The CLI is missing, failed, or timed out; the reason is one line.
    NotDetected(String),
}

/// Run `<bin> status --nointeraction` and classify the result.
pub fn bitwarden_status(bin: &str) -> BitwardenStatus {
    let mut cmd = Command::new(bin);
    cmd.args(["status", "--nointeraction"])
        .env_remove("BW_SESSION");
    match run_with_timeout(cmd, BITWARDEN_TIMEOUT) {
        Ok((status, stdout)) => classify_bitwarden_status(status.success(), &stdout),
        Err(err) => BitwardenStatus::NotDetected(err),
    }
}

/// Classify a finished `bw status` run.
pub fn classify_bitwarden_status(success: bool, stdout: &[u8]) -> BitwardenStatus {
    if !success {
        return BitwardenStatus::NotDetected("bw status failed".into());
    }
    let state = serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|json| json.get("status")?.as_str().map(str::to_string));
    BitwardenStatus::Detected { state }
}

/// Spawn `cmd` with stdout captured and wait up to `timeout`, polling;
/// a child still running then is killed. `Err` is a one-line reason with
/// no output in it.
fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> Result<(ExitStatus, Zeroizing<Vec<u8>>), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|err| format!("cannot run bw: {err}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill(&mut child);
                return Err(format!("bw timed out after {}s", timeout.as_secs_f32()));
            }
            Err(err) => {
                kill(&mut child);
                return Err(format!("waiting for bw failed: {err}"));
            }
        }
    };
    // Sized up front so a short secret is never left behind by a regrow.
    let mut stdout = Zeroizing::new(Vec::with_capacity(4096));
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut *stdout)
            .map_err(|err| format!("reading bw output failed: {err}"))?;
    }
    Ok((status, stdout))
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
}
