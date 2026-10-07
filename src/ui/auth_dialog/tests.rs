//! Unit tests for the display-free logic of [`super::auth_dialog`].
//!
//! GTK dialog construction needs a display, so these tests exercise only the
//! prompt/field mapping, the response formatting, and the closure adapters
//! (with an injected fake runner instead of the real dialog).

use std::sync::Arc;

use crate::transport::auth::{ChallengePrompt, ChallengeResponder};

use super::*;

fn prompt(text: &str, echo: bool) -> ChallengePrompt {
    ChallengePrompt {
        prompt: text.to_string(),
        echo,
    }
}

#[test]
fn field_specs_preserve_order_and_echo_flags() {
    let prompts = [
        prompt("Password: ", false),
        prompt("Verification code: ", true),
    ];
    let specs = field_specs(&prompts);
    assert_eq!(specs.len(), 2);
    assert_eq!(
        specs,
        vec![
            ChallengeField {
                label: "Password:".to_string(),
                echo: false,
            },
            ChallengeField {
                label: "Verification code:".to_string(),
                echo: true,
            },
        ]
    );
}

#[test]
fn field_specs_relabel_empty_prompts() {
    let specs = field_specs(&[prompt("   ", false)]);
    assert_eq!(specs[0].label, "Response");
    assert!(!specs[0].echo);
}

#[test]
fn field_specs_of_empty_prompt_list_is_empty() {
    assert!(field_specs(&[]).is_empty());
}

#[test]
fn window_title_names_the_target() {
    let title = window_title("bastion");
    assert!(title.contains("bastion"), "{title}");
    assert!(title.contains("Authentication"), "{title}");
}

#[test]
fn instructions_label_passes_through_and_drops_blanks() {
    assert_eq!(
        instructions_label(Some("  Enter your OTP  ")),
        Some("Enter your OTP".to_string())
    );
    assert_eq!(instructions_label(Some("   ")), None);
    assert_eq!(instructions_label(None), None);
}

#[test]
fn responses_match_prompt_count_and_order() {
    let prompts = [prompt("Password: ", false), prompt("OTP: ", true)];
    let fields = vec!["hunter2".to_string(), "123456".to_string()];
    let responses = responses_from_fields(&prompts, &fields);
    assert_eq!(responses, vec!["hunter2".to_string(), "123456".to_string()]);
    assert_eq!(responses.len(), prompts.len());
}

#[test]
fn responses_pad_missing_entries() {
    let prompts = [prompt("a", false), prompt("b", true), prompt("c", false)];
    let fields = vec!["one".to_string()];
    assert_eq!(
        responses_from_fields(&prompts, &fields),
        vec!["one".to_string(), String::new(), String::new()]
    );
}

#[test]
fn responses_drop_extra_entries() {
    let prompts = [prompt("a", false)];
    let fields = vec!["one".to_string(), "two".to_string()];
    assert_eq!(
        responses_from_fields(&prompts, &fields),
        vec!["one".to_string()]
    );
}

#[test]
fn responses_of_no_prompts_is_empty() {
    assert!(responses_from_fields(&[], &["stray".to_string()]).is_empty());
}

#[test]
fn responses_keep_secrets_verbatim() {
    // Passwords must not be trimmed: a leading or trailing space is significant.
    let prompts = [prompt("Passphrase: ", false)];
    let fields = vec!["  pad d e  ".to_string()];
    assert_eq!(
        responses_from_fields(&prompts, &fields),
        vec!["  pad d e  ".to_string()]
    );
}

#[test]
fn challenge_responder_forwards_prompts_to_the_runner() {
    let responder = challenge_responder_with("bastion".to_string(), |name, instr, prompts| {
        assert_eq!(name, "bastion");
        assert_eq!(instr, None);
        Some(
            prompts
                .iter()
                .map(|p| if p.echo { "echo" } else { "secret" }.to_string())
                .collect(),
        )
    });

    let prompts = [prompt("Password: ", false), prompt("OTP: ", true)];
    let answers = responder(&prompts).expect("responder must answer");
    assert_eq!(answers, vec!["secret".to_string(), "echo".to_string()]);
}

#[test]
fn challenge_responder_propagates_cancellation() {
    let responder = challenge_responder_with("bastion".to_string(), |_, _, _| None);
    let prompts = [prompt("Password: ", false)];
    assert!(responder(&prompts).is_none());
}

#[test]
fn challenge_responder_cancellation_is_repeatable_across_rounds() {
    // The auth chain calls the responder once per InfoRequest; a cancelled
    // round must keep reporting cancellation rather than caching an answer.
    let responder = challenge_responder_with("bastion".to_string(), |_, _, _| None);
    let prompts = [prompt("Password: ", false)];
    for _ in 0..3 {
        assert!(responder(&prompts).is_none());
    }
}

#[test]
fn challenge_responder_is_send_and_sync() {
    fn assert_send_sync(_: &ChallengeResponder) {}
    let responder: ChallengeResponder = challenge_responder("bastion");
    assert_send_sync(&responder);
    // Also prove it can be handed to another thread, as the transport does.
    let moved = responder.clone();
    let handle = std::thread::spawn(move || {
        let _ = Arc::strong_count(&moved);
    });
    handle.join().expect("responder moved across threads");
}

#[test]
fn challenge_responder_handles_empty_prompt_set() {
    // The closure signature carries no opinion about an empty prompt set; the
    // dialog itself short-circuits to `None` (see `run_challenges`).
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_ok = seen.clone();
    let responder = challenge_responder_with("bastion".to_string(), move |_, _, prompts| {
        seen_ok.lock().expect("lock").push(prompts.len());
        Some(Vec::new())
    });
    assert_eq!(responder(&[]).unwrap_or_default(), Vec::<String>::new());
    assert_eq!(*seen.lock().expect("lock"), vec![0]);
}
