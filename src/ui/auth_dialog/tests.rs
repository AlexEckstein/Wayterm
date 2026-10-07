//! Unit tests for the display-free logic of [`super::auth_dialog`].
//!
//! GTK dialog construction needs a display, so these tests exercise only the
//! prompt/field mapping, the response formatting, and the closure adapters
//! (with an injected fake runner instead of the real dialog).

use std::sync::Arc;

use crate::transport::auth::{ChallengePrompt, ChallengeResponder};

use super::*;

/// Make the default [`glib::MainContext`] owned by THIS thread so the
/// marshalling closures run inline deterministically.
///
/// `run_on_gtk_main_thread` has two paths: inline when the calling thread
/// owns the default context, otherwise `invoke` + blocking `recv()` against
/// a context whose delivery depends on glib's inline-run optimization —
/// i.e. on a race with any other test thread touching glib at the same
/// moment (observed: intermittent 60s+ hangs with default test
/// parallelism, never with `--test-threads=1`). Acquiring the context
/// removes the race: the guard pins ownership for the test's body and the
/// inline path is taken every time.
#[cfg(test)]
pub(crate) fn acquire_default_ctx() -> gtk4::glib::MainContextAcquireGuard<'static> {
    // AVOID the non-owner path of `run_on_gtk_main_thread` in tests.
    //
    // That path is `ctx.invoke(work)` + a blocking `recv()` on a channel the
    // invoked closure fills. Delivery relies on glib's inline-run
    // optimization: when the default context has NO owner and nobody is
    // iterating it, `invoke` runs the function on the calling thread
    // immediately. When some other thread transiently touches the context,
    // the closure is queued instead — and nothing ever iterates it, so the
    // test hangs forever (observed: intermittent 60s+ hangs at default
    // test parallelism; identical runs pass at `--test-threads=1`).
    //
    // Fix: each marshalling test holds the returned guard for its body and
    // releases it on drop. Do NOT mem::forget the guard: a leaked acquire
    // poisons the context for every other thread, because glib ownership is
    // thread-bound and nothing releases it. While one test holds the
    // context, callers retry briefly; test bodies are sub-millisecond.
    let ctx: &'static gtk4::glib::MainContext =
        Box::leak(Box::new(gtk4::glib::MainContext::default()));
    let mut attempts = 0;
    loop {
        match ctx.acquire() {
            Ok(guard) => return guard,
            Err(_) => {
                attempts += 1;
                assert!(attempts < 200, "default MainContext stuck owned");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

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
    let _ctx = acquire_default_ctx();
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
    let _ctx = acquire_default_ctx();
    let responder = challenge_responder_with("bastion".to_string(), |_, _, _| None);
    let prompts = [prompt("Password: ", false)];
    assert!(responder(&prompts).is_none());
}

#[test]
fn challenge_responder_cancellation_is_repeatable_across_rounds() {
    let _ctx = acquire_default_ctx();
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
    // No guard needed: never invokes a context, only moves the Arc across threads.
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
    let _ctx = acquire_default_ctx();
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
