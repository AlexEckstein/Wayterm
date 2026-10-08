//! Server-gated auth-method fallback matrix — doc 11 §Testing Strategy item 3.
//!
//! Item 3 asks for "method fallback order and retry counts" asserted against a
//! real sshd whose `AuthenticationMethods` pins ONE method per instance. This
//! file is that matrix; `src/transport/auth.rs` is the code under test and is
//! NOT modified by it.
//!
//! WHY CI DOES NOT RUN THIS FILE: the matrix needs a different sshd per mode.
//! CI provisions exactly ONE all-methods sshd on port 22
//! (`.github/workflows/ci.yml`, "Provision test sshd") and sets no
//! `AuthenticationMethods`, so every method is offered and the matrix
//! degenerates into what `tests/transport_sshd.rs` already covers. CI therefore
//! never sets `SSH_MATRIX_*`, and every test here takes the skip path
//! (`println!` + early return) — `cargo test` stays green with no server.
//!
//! Run them for real with `scripts/auth-matrix-sshd.sh`, which provisions one
//! throwaway sshd per mode on its own port and exports `SSH_MATRIX_*`.
//!
//! Environment (all six required, else the test skips):
//!   SSH_MATRIX_HOST, SSH_MATRIX_PORT, SSH_MATRIX_USER, SSH_MATRIX_PASSWORD,
//!   SSH_MATRIX_KEY_PATH, SSH_MATRIX_MODE
//!
//! `SSH_MATRIX_MODE` is one of `password-only`, `kbd-interactive-only`,
//! `key-only`. Each test asserts its own mode and skips otherwise, so one
//! `cargo test --test auth_matrix` invocation against a single-mode server
//! runs exactly that mode's assertions. `--test-threads=1` is required by the
//! runner because every mode shares one test account's max-auth budget.
//!
//! HOW THE FALLBACK IS OBSERVED: `AuthOutcome` is `Success` without naming the
//! method that won, so each mode proves two halves separately —
//!
//! 1. the server really rejects the *other* methods (chain with only those
//!    configured → `Failed`), and
//! 2. the full chain then reaches `Success`.
//!
//! `Failed::methods_remaining` is the lever: it lists exactly the methods the
//! chain never ATTEMPTED, so a method's absence from that list proves it was
//! tried and failed. That makes per-method rejection assertable without adding
//! a hook to the chain, and it is why the "wrong password / wrong key" cases
//! assert an EMPTY list: every method was attempted and every one failed.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use russh::client::{self, Config, Handle};

use wayterm::store::model::HostKeyPolicy;
use wayterm::transport::auth::{
    authenticate_chain, AuthConfig, AuthMethodKind, AuthOutcome, ChallengePrompt,
    ChallengeResponder,
};
use wayterm::transport::connection::AppClientHandler;
use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
use zeroize::Zeroizing;

const MODE_PASSWORD: &str = "password-only";
const MODE_KBD_INTERACTIVE: &str = "kbd-interactive-only";
const MODE_KEY: &str = "key-only";

/// Never present on disk and never valid — the chain must treat it as a clean
/// "cannot load key" failure rather than a transport error.
const WRONG_KEY_PATH: &str = "/nonexistent/wayterm-auth-matrix-no-such-key";

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    key_path: String,
    mode: String,
}

fn read_fixture() -> Option<Fixture> {
    let host = std::env::var("SSH_MATRIX_HOST").ok()?;
    let port = std::env::var("SSH_MATRIX_PORT").ok()?.parse().ok()?;
    let user = std::env::var("SSH_MATRIX_USER").ok()?;
    let password = std::env::var("SSH_MATRIX_PASSWORD").ok()?;
    let key_path = std::env::var("SSH_MATRIX_KEY_PATH").ok()?;
    let mode = std::env::var("SSH_MATRIX_MODE").ok()?;
    Some(Fixture {
        host,
        port,
        user,
        password,
        key_path,
        mode,
    })
}

fn skip(reason: &str) {
    println!(
        "Skipping auth-matrix test: {reason}. Set SSH_MATRIX_HOST, \
         SSH_MATRIX_PORT, SSH_MATRIX_USER, SSH_MATRIX_PASSWORD, \
         SSH_MATRIX_KEY_PATH and SSH_MATRIX_MODE (see \
         scripts/auth-matrix-sshd.sh) to run it."
    );
}

/// `None` → SSH_MATRIX_* absent, skip. `Some(false)` → wrong mode, skip.
fn gate(name: &str, mode: &str) -> Option<Fixture> {
    let Some(f) = read_fixture() else {
        skip("SSH_MATRIX_* is not set");
        return None;
    };
    if f.mode != mode {
        skip(&format!(
            "{name} needs mode '{want}', this sshd is mode '{have}'",
            want = mode,
            have = f.mode
        ));
        return None;
    }
    Some(f)
}

fn accept_all_verifier() -> HostKeyVerifier {
    HostKeyVerifier::new(
        HostKeyPolicy::Off,
        Arc::new(|_| HostKeyDecision::Accept),
        None,
    )
}

async fn connect_raw(host: &str, port: u16, username: &str) -> Handle<AppClientHandler> {
    let config = Arc::new(Config::default());
    let addr = SocketAddr::new(
        host.parse::<IpAddr>()
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        port,
    );
    let handler = AppClientHandler {
        host: host.into(),
        port,
        username: username.into(),
        host_key_verifier: accept_all_verifier(),
        state_tx: None,
    };
    client::connect(config, addr, handler)
        .await
        .expect("raw sshd connection failed")
}

/// Answers any password-shaped prompt with `answer`. The chain's synthetic
/// wrong-password re-prompt ("Password (retry): ") is password-shaped too, so
/// this doubles as the retry responder — exactly as the GTK dialog would.
fn password_responder(answer: &str, seen: Option<Arc<Mutex<Vec<String>>>>) -> ChallengeResponder {
    let answer = answer.to_string();
    Arc::new(move |prompts: &[ChallengePrompt]| {
        if let Some(seen) = &seen {
            if let Ok(mut log) = seen.lock() {
                log.extend(prompts.iter().map(|p| p.prompt.clone()));
            }
        }
        Some(
            prompts
                .iter()
                .map(|p| {
                    if p.prompt.to_ascii_lowercase().contains("password") {
                        answer.clone()
                    } else {
                        String::new()
                    }
                })
                .collect(),
        )
    })
}

fn auth_config(
    key_path: Option<&str>,
    password: Option<&str>,
    try_agent: bool,
    try_keyboard_interactive: bool,
    challenge_responder: Option<ChallengeResponder>,
) -> AuthConfig {
    AuthConfig {
        key_path: key_path.map(str::to_string),
        key_passphrase: None,
        password: password.map(|p| Zeroizing::new(p.as_bytes().to_vec())),
        try_agent,
        try_keyboard_interactive,
        challenge_responder,
    }
}

/// Assert `outcome` is `Failed` with exactly `expected` methods never tried.
fn assert_failed(
    outcome: AuthOutcome,
    context: &str,
    expected: &[AuthMethodKind],
) -> (String, Vec<AuthMethodKind>) {
    match outcome {
        AuthOutcome::Failed {
            reason,
            methods_remaining,
        } => {
            // The reason text is the only window into WHICH attempt ended the
            // chain (auth.rs keeps one rolling `last_reason`), so print it —
            // under `--nocapture` the runner log then shows, per mode, the real
            // last failure rather than only pass/fail.
            println!(
                "[auth-matrix] {context}: reason={reason:?} never_attempted={methods_remaining:?}"
            );
            assert_eq!(
                methods_remaining, expected,
                "{context}: methods_remaining mismatch (this list is exactly the \
                 methods the chain never attempted)"
            );
            (reason, methods_remaining)
        }
        AuthOutcome::Success => panic!("{context}: expected AuthOutcome::Failed, got Success"),
        AuthOutcome::Cancelled => panic!("{context}: expected AuthOutcome::Failed, got Cancelled"),
    }
}

fn assert_success(outcome: AuthOutcome, context: &str) {
    match outcome {
        AuthOutcome::Success => {}
        AuthOutcome::Failed {
            reason,
            methods_remaining,
        } => panic!(
            "{context}: expected AuthOutcome::Success, got Failed ({reason}) \
             with methods_remaining {methods_remaining:?}"
        ),
        AuthOutcome::Cancelled => panic!("{context}: expected AuthOutcome::Success, got Cancelled"),
    }
}

// ---------------------------------------------------------------------------
// Mode (a): password-only — the server accepts NOTHING but password.
// ---------------------------------------------------------------------------

/// Server gate: the client's key is valid and IS in authorized_keys, yet the
/// server refuses it because `AuthenticationMethods password` is the only
/// acceptable method. Without this the "fell through to password" assertion in
/// `password_only_chain_falls_through_to_password` would be vacuous — the
/// key might have been what actually won.
#[tokio::test]
async fn password_only_rejects_key_even_when_installed() {
    let Some(f) = gate(
        "password_only_rejects_key_even_when_installed",
        MODE_PASSWORD,
    ) else {
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(Some(&f.key_path), None, false, false, None);

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion");

    let (reason, _) = assert_failed(
        outcome,
        "key-only attempt against a password-only server",
        &[
            AuthMethodKind::Agent,
            AuthMethodKind::Password,
            AuthMethodKind::KeyboardInteractive,
        ],
    );
    // The key is valid and installed, so the ONLY way this fails is the server
    // refusing it. Guard against the test passing for the wrong reason: a
    // client-side key-load failure would also say "publickey", so require the
    // key to have loaded and been OFFERED.
    assert!(
        reason.contains("publickey"),
        "reason should name the rejected publickey attempt: {reason}"
    );
    assert!(
        !reason.contains("load key"),
        "the key must have loaded and been offered for this to be a server \
         rejection, but the chain failed to load it: {reason}"
    );
}

/// The fallback this matrix exists for: pubkey is attempted and rejected, the
/// chain keeps going and the password link completes authentication.
#[tokio::test]
async fn password_only_chain_falls_through_to_password() {
    let Some(f) = gate(
        "password_only_chain_falls_through_to_password",
        MODE_PASSWORD,
    ) else {
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        Some(&f.key_path),
        Some(&f.password),
        false,
        true,
        Some(password_responder(&f.password, None)),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion");

    assert_success(outcome, "key + correct password against password-only sshd");
}

/// Retry accounting (doc 11 §Flow: "wrong password = retry, not an error
/// screen"): one wrong password triggers exactly one re-prompt, and when that
/// also fails the chain reports a CLEAN `Failed` — not a transport error and
/// not an error screen. Every method is configured here, so an empty
/// `methods_remaining` is the assertion that nothing was left untried.
#[tokio::test]
async fn password_only_wrong_password_fails_cleanly_after_one_retry() {
    let Some(f) = gate(
        "password_only_wrong_password_fails_cleanly_after_one_retry",
        MODE_PASSWORD,
    ) else {
        return;
    };

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        Some(&f.key_path),
        Some("definitely-wrong-password"),
        true,
        true,
        Some(password_responder(
            "definitely-wrong-password",
            Some(Arc::clone(&seen)),
        )),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion, not error");

    assert_failed(
        outcome,
        "all four methods attempted and all four rejected",
        &[],
    );

    let prompts = seen.lock().expect("prompt log poisoned").clone();
    let retries = prompts
        .iter()
        .filter(|p| p.to_ascii_lowercase().contains("retry"))
        .count();
    assert_eq!(
        retries, 1,
        "exactly one password re-prompt is allowed by doc 11 §Flow, saw {prompts:?}"
    );
}

// ---------------------------------------------------------------------------
// Mode (b): keyboard-interactive-only via PAM — the last link in the chain.
// ---------------------------------------------------------------------------

/// Server gate: the password is CORRECT and the server still refuses it
/// (`PasswordAuthentication no`). `KeyboardInteractive` is absent from
/// `methods_remaining` precisely because it was not attempted here — it is the
/// next link, which the next test proves succeeds.
#[tokio::test]
async fn kbd_interactive_only_rejects_password_even_when_correct() {
    let Some(f) = gate(
        "kbd_interactive_only_rejects_password_even_when_correct",
        MODE_KBD_INTERACTIVE,
    ) else {
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        None,
        Some(&f.password),
        false,
        false,
        Some(password_responder(&f.password, None)),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion");

    // PublicKey is in the list because no key is configured here — it is the
    // one method that genuinely was never attempted. Password's ABSENCE is the
    // assertion: it was attempted, and the server refused it anyway.
    let (reason, _) = assert_failed(
        outcome,
        "password attempt against a keyboard-interactive-only server",
        &[
            AuthMethodKind::Agent,
            AuthMethodKind::PublicKey,
            AuthMethodKind::KeyboardInteractive,
        ],
    );
    assert!(
        reason.contains("password"),
        "reason should name the rejected password attempt: {reason}"
    );
}

/// Proves the final link (agent → key → password → keyboard-interactive)
/// actually engages: key rejected, password rejected, then the PAM challenge
/// round is answered by the challenge closure and auth completes. Nothing but
/// keyboard-interactive can succeed on this server, so `Success` is
/// keyboard-interactive; the prompt log additionally proves the closure
/// answered a SERVER-SENT challenge rather than only the synthetic retry.
#[tokio::test]
async fn kbd_interactive_only_chain_reaches_keyboard_interactive() {
    let Some(f) = gate(
        "kbd_interactive_only_chain_reaches_keyboard_interactive",
        MODE_KBD_INTERACTIVE,
    ) else {
        return;
    };

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        Some(&f.key_path),
        Some(&f.password),
        false,
        true,
        Some(password_responder(&f.password, Some(Arc::clone(&seen)))),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion");

    assert_success(
        outcome,
        "key + password + keyboard-interactive against kbd-interactive-only sshd",
    );

    let prompts = seen.lock().expect("prompt log poisoned").clone();
    assert!(
        prompts.iter().any(|p| !p.contains("retry")),
        "expected the closure to answer a server-sent keyboard-interactive \
         challenge (PAM), saw only: {prompts:?}"
    );
}

// ---------------------------------------------------------------------------
// Mode (c): key-only — nothing but publickey is acceptable.
// ---------------------------------------------------------------------------

/// Server gate: BOTH of the trailing links are configured and attempted, yet
/// neither is in `methods_remaining`, so the failure can only mean "attempted
/// and rejected by the server". The chain must not skip them silently.
#[tokio::test]
async fn key_only_rejects_password_and_keyboard_interactive() {
    let Some(f) = gate(
        "key_only_rejects_password_and_keyboard_interactive",
        MODE_KEY,
    ) else {
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        None,
        Some(&f.password),
        false,
        true,
        Some(password_responder(&f.password, None)),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion");

    assert_failed(
        outcome,
        "password + keyboard-interactive against a key-only server",
        &[AuthMethodKind::Agent, AuthMethodKind::PublicKey],
    );
}

/// The key link wins immediately — first attempt, no fallback needed.
#[tokio::test]
async fn key_only_chain_succeeds_with_key() {
    let Some(f) = gate("key_only_chain_succeeds_with_key", MODE_KEY) else {
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        Some(&f.key_path),
        Some(&f.password),
        false,
        true,
        Some(password_responder(&f.password, None)),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("chain must run to completion");

    assert_success(outcome, "installed key against key-only sshd");
}

/// A wrong key path is a clean `Failed`, not a panic and not a transport
/// error, and the chain does NOT abort on it: `methods_remaining` is empty,
/// so password and keyboard-interactive were still attempted after the key
/// load failed. (The reason text is whatever the LAST attempt produced —
/// `authenticate_chain` keeps one rolling `last_reason` — so it is not
/// asserted here; over-fitting it would pin an implementation detail.)
#[tokio::test]
async fn key_only_wrong_key_path_reports_failed_cleanly() {
    let Some(f) = gate("key_only_wrong_key_path_reports_failed_cleanly", MODE_KEY) else {
        return;
    };

    let mut handle = connect_raw(&f.host, f.port, &f.user).await;
    let config = auth_config(
        Some(WRONG_KEY_PATH),
        Some(&f.password),
        true,
        true,
        Some(password_responder(&f.password, None)),
    );

    let outcome = authenticate_chain(&mut handle, &f.user, &config)
        .await
        .expect("a missing key file must not surface as a transport error");

    let (reason, _) = assert_failed(
        outcome,
        "nonexistent key path against a key-only server",
        &[],
    );
    assert!(!reason.is_empty(), "a Failed outcome must carry a reason");
}
