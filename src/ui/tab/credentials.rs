//! The session's credential chain (doc 05) with the
//! challenge dialog as its prompt. The chain's order and wording are
//! display-free, in `transport::credentials`.

use crate::store::SessionStore;
use crate::transport::credentials::{
    credential_sources, prompt_instructions, prompt_source, CredentialManager, KeyringSource,
    PromptSource,
};
use crate::ui::auth_dialog::{self, ChallengePrompt};
use crate::ui::settings::credentials::CredentialSettings;

/// Keyring first, then Bitwarden when enabled, then a one-field challenge
/// dialog (doc 05), for saved session `session_id`. The
/// settings come from `store`; without one (a page opened with no store)
/// Bitwarden stays off.
// used by ui::vnc_tab, ui::rdp_tab and ui::session_tree
pub(crate) fn credential_manager(
    session_id: i64,
    session_name: &str,
    store: Option<&SessionStore>,
) -> CredentialManager {
    let session_id = session_id.to_string();
    let settings = store.map(CredentialSettings::load).unwrap_or_default();
    let keyring = KeyringSource::new(&session_id, session_name);
    let keyring_unreachable = !keyring.available();
    let prompt = dialog_prompt(session_name, prompt_instructions(keyring_unreachable));
    let bitwarden_bin = settings
        .bitwarden_enabled
        .then_some(settings.bitwarden_bin.as_str());
    CredentialManager::for_session(
        &session_id,
        credential_sources(
            bitwarden_bin,
            session_name,
            Box::new(keyring),
            Box::new(prompt),
        ),
    )
}

/// The challenge dialog with one masked field.
fn dialog_prompt(session_name: &str, instructions: Option<&'static str>) -> PromptSource {
    let name = session_name.to_string();
    prompt_source(move |label| {
        let prompts = [ChallengePrompt {
            prompt: label.into(),
            echo: false,
        }];
        let name = name.clone();
        auth_dialog::run_on_gtk_main_thread(move || {
            auth_dialog::run_challenges(&name, instructions, &prompts)
        })
        .flatten()
        .and_then(|answers| answers.into_iter().next())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manager_reads_the_store_and_ends_with_the_prompt() {
        let store = SessionStore::in_memory().unwrap();
        let debug = |mgr: CredentialManager| format!("{mgr:?}");
        assert!(debug(credential_manager(42, "prod", Some(&store)))
            .contains(r#"["keyring", "prompt"]"#));
        crate::ui::settings::credentials::save_enabled(&store, true).unwrap();
        assert!(debug(credential_manager(42, "prod", Some(&store)))
            .contains(r#"["keyring", "bitwarden", "prompt"]"#));
        assert!(debug(credential_manager(42, "prod", None)).contains(r#"["keyring", "prompt"]"#));
    }
}
