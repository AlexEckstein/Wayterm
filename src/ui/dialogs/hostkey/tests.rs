//! Unit tests for the display-free logic of the host-key dialogs.
//!
//! GTK dialog construction needs a display, so these tests exercise only the
//! OpenSSH-style text builders, the decision mapping, and the prompt-closure
//! adapter (with an injected fake handler).

use super::*;
#[cfg(test)]
pub(crate) use crate::ui::dialogs::auth::tests::acquire_default_ctx;

// The two host_key_prompt_* tests below marshal onto the default
// MainContext through run_on_gtk_main_thread; `acquire_default_ctx()`
// (from the auth dialog's test module) pins context ownership
// process-wide so the marshalling path is deterministic under
// default test parallelism.

fn facts() -> HostKeyFacts {
    HostKeyFacts {
        host: "bastion.example.com".to_string(),
        port: 2222,
        key_type: "ssh-ed25519".to_string(),
        fingerprint_sha256: "SHA256:AnOThErFpRiNtExAMPLE0123456789=".to_string(),
    }
}

#[test]
fn unknown_headline_matches_openssh_wording() {
    assert_eq!(
        notice_headline(&facts(), HostKeyQuestion::Unknown),
        "The authenticity of host 'bastion.example.com' can't be established."
    );
}

#[test]
fn unknown_detail_shows_type_fingerprint_and_question() {
    let detail = notice_detail(&facts(), HostKeyQuestion::Unknown);
    assert!(detail.contains("ssh-ed25519"), "{detail}");
    assert!(
        detail.contains("SHA256:AnOThErFpRiNtExAMPLE0123456789="),
        "{detail}"
    );
    assert!(detail.contains("bastion.example.com:2222"), "{detail}");
    assert!(
        detail.contains("Are you sure you want to continue connecting?"),
        "{detail}"
    );
}

#[test]
fn changed_headline_names_the_change() {
    let headline = notice_headline(&facts(), HostKeyQuestion::Changed);
    assert!(headline.contains("has changed"), "{headline}");
    assert!(headline.contains("bastion.example.com"), "{headline}");
}

#[test]
fn changed_detail_shows_the_facts_and_the_mitm_warning() {
    let detail = notice_detail(&facts(), HostKeyQuestion::Changed);
    assert!(detail.contains("ssh-ed25519"), "{detail}");
    assert!(
        detail.contains("SHA256:AnOThErFpRiNtExAMPLE0123456789="),
        "{detail}"
    );
    assert!(detail.contains("bastion.example.com:2222"), "{detail}");
    assert!(detail.contains("man-in-the-middle"), "{detail}");
    assert!(detail.contains("out of band"), "{detail}");
}

/// The copy says what accepting does, and what it does not: the key is
/// this bookmark's alone, and known_hosts keeps its `ssh-keygen -R`
/// remedy.
#[test]
fn changed_detail_says_the_key_is_kept_for_this_session_only() {
    let detail = notice_detail(&facts(), HostKeyQuestion::Changed);
    assert!(
        detail.contains("for this session bookmark only"),
        "{detail}"
    );
    assert!(
        detail.contains("~/.ssh/known_hosts is not touched"),
        "{detail}"
    );
    assert!(
        detail.contains("ssh-keygen -R '[bastion.example.com]:2222'"),
        "{detail}"
    );
    assert!(!detail.contains("will not let you connect"), "{detail}");
    assert!(!detail.contains("Are you sure you want to continue connecting?"));

    let on_22 = HostKeyFacts {
        port: 22,
        ..facts()
    };
    let detail = notice_detail(&on_22, HostKeyQuestion::Changed);
    assert!(
        detail.contains("ssh-keygen -R 'bastion.example.com'"),
        "{detail}"
    );
}

#[test]
fn the_changed_dialog_offers_deny_first_then_accept_for_the_session() {
    assert_eq!(CHANGED_BUTTONS, ["Deny", "Accept and add to this session"]);
    assert_eq!(UNKNOWN_BUTTONS, ["Reject", "Accept"]);
}

#[test]
fn decision_from_response_maps_accept_to_accept_and_save() {
    assert_eq!(decision_from_response(true), HostKeyDecision::AcceptAndSave);
    assert_eq!(decision_from_response(false), HostKeyDecision::Reject);
}

/// Each question's accepting button gives its own decision: known_hosts
/// for an unknown host, the session's set for a changed key. Deny, Reject
/// and a closed window all reject.
#[test]
fn each_question_maps_its_answers_to_its_own_decisions() {
    let answer = |question, answer: Option<bool>| {
        ask_with(question, &facts(), |shown, asked| {
            assert_eq!(asked, question, "the dialog shows the question asked");
            assert_eq!(shown.host, "bastion.example.com");
            answer
        })
    };
    use HostKeyQuestion::{Changed, Unknown};
    assert_eq!(
        answer(Changed, Some(true)),
        HostKeyDecision::AcceptAndSaveSession
    );
    assert_eq!(answer(Changed, Some(false)), HostKeyDecision::Reject);
    assert_eq!(answer(Changed, None), HostKeyDecision::Reject);
    assert_eq!(answer(Unknown, Some(true)), HostKeyDecision::AcceptAndSave);
    assert_eq!(answer(Unknown, Some(false)), HostKeyDecision::Reject);
    assert_eq!(answer(Unknown, None), HostKeyDecision::Reject);
}

#[test]
fn host_key_prompt_forwards_the_question_and_facts_and_maps_decision() {
    let _ctx = acquire_default_ctx();
    let closure = host_key_prompt_with(|question, facts| {
        decision_for(question, Some(facts.key_type == "ssh-ed25519"))
    });
    assert_eq!(
        closure(HostKeyQuestion::Unknown, &facts()),
        HostKeyDecision::AcceptAndSave
    );
    assert_eq!(
        closure(HostKeyQuestion::Changed, &facts()),
        HostKeyDecision::AcceptAndSaveSession
    );

    let closure = host_key_prompt_with(|_, _| HostKeyDecision::Reject);
    assert_eq!(
        closure(HostKeyQuestion::Changed, &facts()),
        HostKeyDecision::Reject
    );
}

#[test]
fn host_key_prompt_never_returns_bare_accept() {
    let _ctx = acquire_default_ctx();
    // Bare Accept would skip learn_known_hosts_path and re-prompt on every
    // connect, so the accept path must always be AcceptAndSave.
    let closure = host_key_prompt_with(|question, _| decision_for(question, Some(true)));
    assert_ne!(
        closure(HostKeyQuestion::Unknown, &facts()),
        HostKeyDecision::Accept
    );
    assert_ne!(
        closure(HostKeyQuestion::Changed, &facts()),
        HostKeyDecision::Accept
    );
}
