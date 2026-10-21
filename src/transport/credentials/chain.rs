//! A session's chain in priority order (doc 11 §CredentialManager) and the
//! prompt's wording. Display-free: the UI supplies the dialog that asks.

use std::sync::Arc;

use zeroize::Zeroizing;

use super::{BitwardenSource, CredentialSource, PromptSource};

/// What the prompt says when the keyring cannot be reached.
pub const KEYRING_UNAVAILABLE_HINT: &str = "Keyring unavailable — prompting for credentials.";

/// The prompt's instructions line: the keyring hint, or nothing.
pub fn prompt_instructions(keyring_unreachable: bool) -> Option<&'static str> {
    keyring_unreachable.then_some(KEYRING_UNAVAILABLE_HINT)
}

/// The chain in priority order: keyring, Bitwarden when enabled (the CLI
/// to run, `bitwarden_bin`), and the prompt always last.
pub fn credential_sources(
    bitwarden_bin: Option<&str>,
    session_name: &str,
    keyring: Box<dyn CredentialSource>,
    prompt: Box<dyn CredentialSource>,
) -> Vec<Box<dyn CredentialSource>> {
    let mut sources = vec![keyring];
    if let Some(bin) = bitwarden_bin {
        sources.push(Box::new(BitwardenSource::new(bin, session_name)));
    }
    sources.push(prompt);
    sources
}

/// The masked field's label for `secret_type`.
pub fn prompt_label(secret_type: &str) -> &'static str {
    match secret_type {
        "key_passphrase" => "Passphrase: ",
        _ => "Password: ",
    }
}

/// A prompt with one masked field: `ask` gets the field's label and
/// returns the answer, `None` when the user cancelled.
pub fn prompt_source(
    ask: impl Fn(&'static str) -> Option<String> + Send + Sync + 'static,
) -> PromptSource {
    PromptSource::new(Arc::new(move |_session_id, secret_type| {
        ask(prompt_label(secret_type)).map(|answer| Zeroizing::new(answer.into_bytes()))
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::transport::credentials::KeyringSource;

    fn names(bitwarden_bin: Option<&str>) -> Vec<String> {
        let prompt = PromptSource::new(Arc::new(|_, _| None));
        let keyring = KeyringSource::new("1", "prod");
        credential_sources(bitwarden_bin, "prod", Box::new(keyring), Box::new(prompt))
            .iter()
            .map(|source| source.source_name().to_string())
            .collect()
    }

    #[test]
    fn bitwarden_sits_between_keyring_and_prompt_when_enabled() {
        assert_eq!(names(Some("bw")), ["keyring", "bitwarden", "prompt"]);
    }

    #[test]
    fn bitwarden_is_absent_when_disabled() {
        assert_eq!(names(None), ["keyring", "prompt"]);
    }

    #[test]
    fn the_hint_shows_only_when_the_keyring_is_unreachable() {
        assert_eq!(prompt_instructions(false), None);
        assert_eq!(prompt_instructions(true), Some(KEYRING_UNAVAILABLE_HINT));
    }

    #[test]
    fn the_prompt_asks_by_secret_type_and_returns_the_answer() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let prompt = prompt_source(move |label| {
            log.lock().unwrap().push(label);
            Some("typed".to_string())
        });
        let answer = prompt.lookup("1", "key_passphrase").expect("answered");
        assert_eq!(answer.as_slice(), b"typed");
        prompt.lookup("1", "password").expect("answered");
        assert_eq!(*asked.lock().unwrap(), ["Passphrase: ", "Password: "]);
    }

    #[test]
    fn a_cancelled_prompt_is_a_miss() {
        let prompt = prompt_source(|_| None);
        assert!(prompt.lookup("1", "password").is_none());
    }
}
