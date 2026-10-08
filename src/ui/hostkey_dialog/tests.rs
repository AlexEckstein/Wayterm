//! Unit tests for the display-free logic of [`super::hostkey_dialog`].
//!
//! GTK dialog construction needs a display, so these tests exercise only the
//! OpenSSH-style text builders, the decision mapping, and the prompt-closure
//! adapter (with an injected fake handler).

use super::*;
#[cfg(test)]
pub(crate) use crate::ui::auth_dialog::tests::acquire_default_ctx;

// The two host_key_prompt_* tests below marshal onto the default
// MainContext through run_on_gtk_main_thread; `acquire_default_ctx()`
// (from auth_dialog's test module) pins context ownership
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
        notice_headline(&facts(), HostKeyNotice::Unknown),
        "The authenticity of host 'bastion.example.com' can't be established."
    );
}

#[test]
fn unknown_detail_shows_type_fingerprint_and_question() {
    let detail = notice_detail(&facts(), HostKeyNotice::Unknown);
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
    let headline = notice_headline(&facts(), HostKeyNotice::Changed);
    assert!(headline.contains("has changed"), "{headline}");
    assert!(headline.contains("bastion.example.com"), "{headline}");
}

#[test]
fn changed_detail_is_a_mitm_warning_with_remedy() {
    let detail = notice_detail(&facts(), HostKeyNotice::Changed);
    assert!(detail.contains("man-in-the-middle"), "{detail}");
    assert!(
        detail.contains("SHA256:AnOThErFpRiNtExAMPLE0123456789="),
        "{detail}"
    );
    assert!(
        detail.contains("ssh-keygen -R 'bastion.example.com:2222'"),
        "{detail}"
    );
    assert!(detail.contains("will not let you connect"), "{detail}");
}

#[test]
fn changed_detail_offers_no_acceptance_question() {
    let detail = notice_detail(&facts(), HostKeyNotice::Changed);
    assert!(!detail.contains("Are you sure you want to continue connecting?"));
    assert!(!detail.to_lowercase().contains("do you want to continue"));
}

#[test]
fn changed_dialog_has_only_the_reject_button() {
    assert_eq!(CHANGED_BUTTONS, ["Reject"]);
    assert!(UNKNOWN_BUTTONS.contains(&"Accept"));
    assert_eq!(UNKNOWN_BUTTONS[0], "Reject");
}

#[test]
fn decision_from_response_maps_accept_to_accept_and_save() {
    assert_eq!(decision_from_response(true), HostKeyDecision::AcceptAndSave);
    assert_eq!(decision_from_response(false), HostKeyDecision::Reject);
}

#[test]
fn host_key_prompt_forwards_facts_and_maps_decision() {
    let _ctx = acquire_default_ctx();
    let closure = host_key_prompt_with(|notice, facts| {
        assert_eq!(notice, HostKeyNotice::Unknown);
        decision_from_response(facts.key_type == "ssh-ed25519")
    });
    assert_eq!(closure(&facts()), HostKeyDecision::AcceptAndSave);

    let closure = host_key_prompt_with(|_, _| HostKeyDecision::Reject);
    assert_eq!(closure(&facts()), HostKeyDecision::Reject);
}

#[test]
fn host_key_prompt_never_returns_bare_accept() {
    let _ctx = acquire_default_ctx();
    // Bare Accept would skip learn_known_hosts_path and re-prompt on every
    // connect, so the accept path must always be AcceptAndSave.
    let closure = host_key_prompt_with(|_, _| decision_from_response(true));
    assert_ne!(closure(&facts()), HostKeyDecision::Accept);
}
