use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use russh::keys::{load_public_key, Algorithm, EcdsaCurve, PublicKey};
use russh::Preferred;

use crate::store::model::{HostKeyPolicy, SessionHostKey};
use crate::transport::hostkey::{
    HostKeyDecision, HostKeyFacts, HostKeyQuestion, HostKeyVerifier, SessionKeySet,
};

const POLICIES: [HostKeyPolicy; 3] = [
    HostKeyPolicy::Strict,
    HostKeyPolicy::AcceptNew,
    HostKeyPolicy::Off,
];

const ED25519_KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
const ED25519_KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
const ECDSA_KEY: &str = "AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBE+KdDWUZt8XbzY8cEXhri1wgMtIDN53rQwh9dilljdUu5AvA88iV+cGfUNs7+yjPIMCDXcm2Bqf4dS0a9mnp7k=";

fn make_key(blob: &str) -> PublicKey {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("key.pub");
    // No comment: a key off the wire has none, and equality compares it.
    std::fs::write(&path, format!("ssh-ed25519 {blob}\n")).unwrap();
    load_public_key(&path).expect("valid test key")
}

fn make_facts() -> HostKeyFacts {
    HostKeyFacts {
        host: "localhost".into(),
        port: 13265,
        key_type: "ssh-ed25519".into(),
        fingerprint_sha256: "SHA256:test".into(),
    }
}

fn write_known_hosts(path: &std::path::Path, host: &str, port: u16, key: &PublicKey) {
    use std::io::Write;
    let mut f = std::fs::File::create(path).unwrap();
    if port != 22 {
        write!(f, "[{}]:{} ", host, port).unwrap();
    } else {
        write!(f, "{} ", host).unwrap();
    }
    f.write_all(key.to_openssh().unwrap().as_bytes()).unwrap();
    f.write_all(b"\n").unwrap();
}

fn verifier(
    policy: HostKeyPolicy,
    decision: HostKeyDecision,
    path: Option<PathBuf>,
) -> HostKeyVerifier {
    HostKeyVerifier::new(
        policy,
        Arc::new(move |_facts| decision),
        path.map(Arc::from),
    )
}

fn verifier_with_prompt(
    policy: HostKeyPolicy,
    path: Option<PathBuf>,
) -> (HostKeyVerifier, Arc<AtomicBool>) {
    let called = Arc::new(AtomicBool::new(false));
    let called_clone = called.clone();
    let v = HostKeyVerifier::new(
        policy,
        Arc::new(move |_facts| {
            called_clone.store(true, Ordering::Relaxed);
            // A real prompt returns AcceptAndSave when the user confirms.
            HostKeyDecision::AcceptAndSave
        }),
        path.map(Arc::from),
    );
    (v, called)
}

#[test]
fn unknown_host_under_strict_prompts_and_accepts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key(ED25519_KEY_A);
    let (v, called) = verifier_with_prompt(HostKeyPolicy::Strict, Some(path.clone()));

    assert_eq!(
        v.verify(&make_facts(), &key),
        HostKeyDecision::AcceptAndSave
    );
    assert!(
        called.load(Ordering::Relaxed),
        "strict policy must prompt for an unknown host"
    );

    // Key must now be recorded.
    let v2 = verifier(HostKeyPolicy::Strict, HostKeyDecision::Reject, Some(path));
    assert_eq!(v2.verify(&make_facts(), &key), HostKeyDecision::Accept);
}

#[test]
fn unknown_host_under_accept_new_auto_saves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key(ED25519_KEY_A);
    let v = verifier(
        HostKeyPolicy::AcceptNew,
        HostKeyDecision::Reject,
        Some(path.clone()),
    );

    assert_eq!(
        v.verify(&make_facts(), &key),
        HostKeyDecision::AcceptAndSave
    );
    // Second check should match now.
    let v2 = verifier(HostKeyPolicy::Strict, HostKeyDecision::Reject, Some(path));
    assert_eq!(v2.verify(&make_facts(), &key), HostKeyDecision::Accept);
}

#[test]
fn matching_key_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key(ED25519_KEY_A);
    write_known_hosts(&path, "localhost", 13265, &key);

    let (v, called) = verifier_with_prompt(HostKeyPolicy::Strict, Some(path));
    assert_eq!(v.verify(&make_facts(), &key), HostKeyDecision::Accept);
    assert!(
        !called.load(Ordering::Relaxed),
        "matching key must not prompt"
    );
}

/// [`HostKeyVerifier::new`]'s prompt answers only the unknown-host
/// question: a changed key is denied without calling it.
#[test]
fn an_unknown_host_prompt_is_never_asked_about_a_changed_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let recorded = make_key(ED25519_KEY_A);
    write_known_hosts(&path, "localhost", 13265, &recorded);

    let presented = make_key(ED25519_KEY_B);
    let (v, called) = verifier_with_prompt(HostKeyPolicy::Strict, Some(path));
    assert_eq!(v.verify(&make_facts(), &presented), HostKeyDecision::Reject);
    assert!(
        !called.load(Ordering::Relaxed),
        "mismatched key must not prompt"
    );
}

#[test]
fn off_policy_auto_accepts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key(ED25519_KEY_A);
    let (v, called) = verifier_with_prompt(HostKeyPolicy::Off, Some(path));
    assert_eq!(v.verify(&make_facts(), &key), HostKeyDecision::Accept);
    assert!(
        !called.load(Ordering::Relaxed),
        "off policy must not prompt"
    );
}

#[test]
fn only_the_off_policy_does_not_verify() {
    let verifies = |policy| verifier(policy, HostKeyDecision::Reject, None::<PathBuf>).verifies();
    assert!(verifies(HostKeyPolicy::Strict));
    assert!(verifies(HostKeyPolicy::AcceptNew));
    assert!(!verifies(HostKeyPolicy::Off));
}

#[test]
fn hashed_entries_are_supported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key("AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF");
    std::fs::write(
        &path,
        b"|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF\n",
    )
    .unwrap();

    let facts = HostKeyFacts {
        host: "example.com".into(),
        port: 22,
        key_type: "ssh-ed25519".into(),
        fingerprint_sha256: "SHA256:test".into(),
    };
    let v = verifier(HostKeyPolicy::Strict, HostKeyDecision::Reject, Some(path));
    assert_eq!(v.verify(&facts, &key), HostKeyDecision::Accept);
}

#[test]
fn strict_unknown_prompt_reject_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key(ED25519_KEY_A);
    let v = verifier(HostKeyPolicy::Strict, HostKeyDecision::Reject, Some(path));
    assert_eq!(v.verify(&make_facts(), &key), HostKeyDecision::Reject);
}

/// A known host showing a key of a type its entries do not hold has
/// changed its key: `strict` and `accept-new` ask the changed-key
/// question, never the unknown-host one, and a denial records nothing;
/// `off` rejects without asking.
#[test]
fn a_key_of_another_type_is_a_changed_key_for_a_known_host() {
    for policy in POLICIES {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let recorded = format!("[localhost]:13265 ecdsa-sha2-nistp256 {ECDSA_KEY}\n");
        std::fs::write(&path, &recorded).unwrap();

        let (v, asked) = asking(policy.clone(), &path, HostKeyDecision::Reject);
        let presented = make_key(ED25519_KEY_A);
        assert_eq!(
            v.verify(&make_facts(), &presented),
            HostKeyDecision::Reject,
            "{policy:?}"
        );
        let expected: &[HostKeyQuestion] = match policy {
            HostKeyPolicy::Off => &[],
            _ => &[HostKeyQuestion::Changed],
        };
        assert_eq!(*asked.lock().unwrap(), expected, "{policy:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), recorded);
        assert!(v.session_keys().keys().is_empty());
    }
}

#[test]
fn a_revoked_key_is_rejected_without_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    std::fs::write(
        &path,
        format!(
            "[localhost]:13265 ssh-ed25519 {ED25519_KEY_A}\n@revoked * ssh-ed25519 {ED25519_KEY_A}\n"
        ),
    )
    .unwrap();
    let (v, called) = verifier_with_prompt(HostKeyPolicy::Strict, Some(path));
    let key = make_key(ED25519_KEY_A);
    assert_eq!(v.verify(&make_facts(), &key), HostKeyDecision::Reject);
    assert!(!called.load(Ordering::Relaxed));
}

/// A known_hosts that cannot be read may hold the host's key, so the host
/// is not unknown: only `off` connects.
#[test]
fn an_unreadable_known_hosts_rejects_unless_verification_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_file = dir.path().to_path_buf();
    let key = make_key(ED25519_KEY_A);
    for (policy, expected) in [
        (HostKeyPolicy::Strict, HostKeyDecision::Reject),
        (HostKeyPolicy::AcceptNew, HostKeyDecision::Reject),
        (HostKeyPolicy::Off, HostKeyDecision::Accept),
    ] {
        let (v, called) = verifier_with_prompt(policy.clone(), Some(not_a_file.clone()));
        assert_eq!(v.verify(&make_facts(), &key), expected, "{policy:?}");
        assert!(
            !called.load(Ordering::Relaxed),
            "{policy:?} must not prompt"
        );
    }
}

/// The name is written to known_hosts as given: one that reads there as a
/// pattern, a list or a second line would vouch for other hosts.
#[test]
fn a_host_name_with_known_hosts_syntax_is_never_recorded() {
    let key = make_key(ED25519_KEY_A);
    for host in [
        "",
        "*",
        "web?.example",
        "a,b",
        "a b",
        "a\tb",
        "a\ngithub.com ssh-ed25519 AAAA",
        "!a",
        "|1|salt|hash",
        "#a",
        "@revoked",
        "[a]",
    ] {
        let facts = HostKeyFacts {
            host: host.into(),
            ..make_facts()
        };
        for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("known_hosts");
            let (v, called) = verifier_with_prompt(policy, Some(path.clone()));
            assert_eq!(v.verify(&facts, &key), HostKeyDecision::Reject, "{host:?}");
            assert!(!called.load(Ordering::Relaxed), "{host:?} must not prompt");
            assert!(!path.exists(), "{host:?} was recorded");
        }
    }
}

#[test]
fn recording_creates_a_private_ssh_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ssh_dir = dir.path().join(".ssh");
    let path = ssh_dir.join("known_hosts");
    let key = make_key(ED25519_KEY_A);
    let v = verifier(
        HostKeyPolicy::AcceptNew,
        HostKeyDecision::Reject,
        Some(path.clone()),
    );
    assert_eq!(
        v.verify(&make_facts(), &key),
        HostKeyDecision::AcceptAndSave
    );
    let mode = std::fs::metadata(&ssh_dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert!(path.is_file());
}

#[test]
fn the_types_of_recorded_keys_are_offered_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let v = verifier(
        HostKeyPolicy::Strict,
        HostKeyDecision::Reject,
        Some(path.clone()),
    );
    let default = Preferred::DEFAULT.key;
    assert_eq!(v.key_preference("localhost", 13265), default, "no file");

    std::fs::write(
        &path,
        format!("[localhost]:13265 ecdsa-sha2-nistp256 {ECDSA_KEY}\n"),
    )
    .unwrap();
    let order = v.key_preference("localhost", 13265);
    assert_eq!(
        order[0],
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256
        }
    );
    assert_eq!(order[1], Algorithm::Ed25519, "the rest keep their order");
    assert_eq!(order.len(), default.len());
    assert_eq!(v.key_preference("elsewhere", 22), default, "another host");

    // Every RSA signature algorithm uses an `ssh-rsa` key.
    std::fs::write(&path, "[localhost]:13265 ssh-rsa AAAAB3NzaC1yc2E\n").unwrap();
    let order = v.key_preference("localhost", 13265);
    assert!(order[..3]
        .iter()
        .all(|algorithm| matches!(algorithm, Algorithm::Rsa { .. })));
    assert_eq!(order[3], Algorithm::Ed25519);
}

// ---------------------------------------------------------------------------
// The session's own key set (doc 04 §Host key verification)
// ---------------------------------------------------------------------------

/// The session key set entry for a key blob, as the verifier adds it.
fn trusted(blob: &str) -> SessionHostKey {
    SessionHostKey {
        key_type: "ssh-ed25519".into(),
        key: blob.trim_end_matches('=').into(),
    }
}

/// A verifier that asks both questions, records each one asked, and
/// answers `answer`; its session trusts nothing yet.
fn asking(
    policy: HostKeyPolicy,
    path: &Path,
    answer: HostKeyDecision,
) -> (HostKeyVerifier, Arc<Mutex<Vec<HostKeyQuestion>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    let v = HostKeyVerifier::asking(
        policy,
        Arc::new(move |question, _facts| {
            log.lock().unwrap().push(question);
            answer
        }),
        Some(Arc::from(path)),
    );
    (v, asked)
}

/// [`asking`], for a session trusting `keys`.
fn asking_trusting(
    policy: HostKeyPolicy,
    path: &Path,
    answer: HostKeyDecision,
    keys: &[&str],
) -> (HostKeyVerifier, Arc<Mutex<Vec<HostKeyQuestion>>>) {
    let (v, asked) = asking(policy, path, answer);
    let set = SessionKeySet::new(keys.iter().map(|key| trusted(key)));
    (v.with_session_keys(set), asked)
}

/// A key in the session's set is accepted without a question under every
/// policy, whether known_hosts names the host with another key or not at
/// all, and known_hosts is left as it was.
#[test]
fn a_key_the_session_trusts_is_accepted_without_a_prompt() {
    for policy in POLICIES {
        for global in [None, Some(ED25519_KEY_A)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("known_hosts");
            if let Some(global) = global {
                write_known_hosts(&path, "localhost", 13265, &make_key(global));
            }
            let before = std::fs::read(&path).ok();
            let (v, asked) = asking_trusting(
                policy.clone(),
                &path,
                HostKeyDecision::Reject,
                &[ED25519_KEY_B],
            );
            assert_eq!(
                v.verify(&make_facts(), &make_key(ED25519_KEY_B)),
                HostKeyDecision::Accept,
                "{policy:?}, known_hosts holding {global:?}"
            );
            assert!(asked.lock().unwrap().is_empty(), "{policy:?}");
            assert_eq!(std::fs::read(&path).ok(), before);
            assert!(v.session_keys().added().is_empty());
        }
    }
}

/// A changed key the user accepts for the session joins the session's set
/// — and only it: known_hosts stays byte for byte as it was. The same
/// verifier then accepts the key without asking again.
#[test]
fn a_changed_key_accepted_for_the_session_never_touches_known_hosts() {
    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        write_known_hosts(&path, "localhost", 13265, &make_key(ED25519_KEY_A));
        let before = std::fs::read(&path).unwrap();

        let (v, asked) = asking(policy.clone(), &path, HostKeyDecision::AcceptAndSaveSession);
        let presented = make_key(ED25519_KEY_B);
        assert_eq!(
            v.verify(&make_facts(), &presented),
            HostKeyDecision::AcceptAndSaveSession,
            "{policy:?}"
        );
        assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
        assert_eq!(std::fs::read(&path).unwrap(), before, "{policy:?}");
        assert_eq!(v.session_keys().added(), [trusted(ED25519_KEY_B)]);

        assert_eq!(v.verify(&make_facts(), &presented), HostKeyDecision::Accept);
        assert_eq!(asked.lock().unwrap().len(), 1, "asked again");
        assert_eq!(v.session_keys().added().len(), 1);
    }
}

/// Denying a changed key rejects it and trusts nothing; so does any answer
/// but the session one — `AcceptAndSave` included, which must never write
/// a changed key into known_hosts.
#[test]
fn a_changed_key_is_rejected_unless_accepted_for_the_session() {
    for answer in [
        HostKeyDecision::Reject,
        HostKeyDecision::Accept,
        HostKeyDecision::AcceptAndSave,
    ] {
        for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("known_hosts");
            write_known_hosts(&path, "localhost", 13265, &make_key(ED25519_KEY_A));
            let before = std::fs::read(&path).unwrap();

            let (v, asked) = asking(policy.clone(), &path, answer);
            assert_eq!(
                v.verify(&make_facts(), &make_key(ED25519_KEY_B)),
                HostKeyDecision::Reject,
                "{policy:?} answered {answer:?}"
            );
            assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(v.session_keys().keys().is_empty());
        }
    }
}

/// Keys of the session's own make the host known to it: another key is a
/// changed key even where known_hosts has no entry — asked about as one,
/// and never recorded silently under `accept-new`.
#[test]
fn a_key_other_than_the_sessions_is_a_changed_key_without_known_hosts_entries() {
    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let (v, asked) = asking_trusting(
            policy.clone(),
            &path,
            HostKeyDecision::Reject,
            &[ED25519_KEY_A],
        );
        assert_eq!(
            v.verify(&make_facts(), &make_key(ED25519_KEY_B)),
            HostKeyDecision::Reject,
            "{policy:?}"
        );
        assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
        assert!(!path.exists(), "{policy:?} recorded a changed key");

        let (v, _) = asking_trusting(
            policy.clone(),
            &path,
            HostKeyDecision::AcceptAndSaveSession,
            &[ED25519_KEY_A],
        );
        assert_eq!(
            v.verify(&make_facts(), &make_key(ED25519_KEY_B)),
            HostKeyDecision::AcceptAndSaveSession
        );
        assert_eq!(
            v.session_keys().keys(),
            [trusted(ED25519_KEY_A), trusted(ED25519_KEY_B)]
        );
        assert!(!path.exists());
    }
}

/// `off` never asks: a changed key, whether known_hosts or the session
/// holds the other key, is rejected as before; a host nothing records is
/// accepted.
#[test]
fn off_rejects_a_changed_key_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let presented = make_key(ED25519_KEY_B);

    let (v, asked) = asking_trusting(
        HostKeyPolicy::Off,
        &path,
        HostKeyDecision::AcceptAndSaveSession,
        &[ED25519_KEY_A],
    );
    assert_eq!(v.verify(&make_facts(), &presented), HostKeyDecision::Reject);

    write_known_hosts(&path, "localhost", 13265, &make_key(ED25519_KEY_A));
    let (v, asked_too) = asking(
        HostKeyPolicy::Off,
        &path,
        HostKeyDecision::AcceptAndSaveSession,
    );
    assert_eq!(v.verify(&make_facts(), &presented), HostKeyDecision::Reject);
    assert!(asked.lock().unwrap().is_empty() && asked_too.lock().unwrap().is_empty());
    assert!(v.session_keys().keys().is_empty());

    let (v, _) = asking(
        HostKeyPolicy::Off,
        &dir.path().join("none"),
        HostKeyDecision::Reject,
    );
    assert_eq!(v.verify(&make_facts(), &presented), HostKeyDecision::Accept);
}

#[test]
fn a_rejected_host_key_is_logged_without_key_material() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    write_known_hosts(&path, "localhost", 13265, &make_key(ED25519_KEY_A));
    let v = verifier(HostKeyPolicy::Off, HostKeyDecision::Reject, Some(path));

    let (buffer, guard) = crate::logging::tests::capture(None);
    let decision = v.verify(&make_facts(), &make_key(ED25519_KEY_B));
    drop(guard);
    assert_eq!(decision, HostKeyDecision::Reject);

    let output = buffer.contents();
    let line = output
        .lines()
        .find(|line| line.contains("host key rejected"))
        .unwrap_or_else(|| panic!("no rejection line in:\n{output}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("host=localhost"), "{line}");
    assert!(line.contains("port=13265"), "{line}");
    assert!(
        line.contains("known_hosts holds another key for this host"),
        "{line}"
    );
    for key in [ED25519_KEY_A, ED25519_KEY_B] {
        assert!(!output.contains(key), "{output}");
    }
    assert!(!output.contains("SHA256:"), "{output}");
}

/// An `@revoked` entry in known_hosts outranks the session's set under
/// every policy: no prompt, no connection.
#[test]
fn a_revoked_key_is_rejected_even_where_the_session_trusts_it() {
    for policy in POLICIES {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, format!("@revoked * ssh-ed25519 {ED25519_KEY_A}\n")).unwrap();
        let (v, asked) = asking_trusting(
            policy.clone(),
            &path,
            HostKeyDecision::AcceptAndSaveSession,
            &[ED25519_KEY_A],
        );
        assert_eq!(
            v.verify(&make_facts(), &make_key(ED25519_KEY_A)),
            HostKeyDecision::Reject,
            "{policy:?}"
        );
        assert!(asked.lock().unwrap().is_empty(), "{policy:?} asked");
    }
}

/// The unknown-host question is still the one asked where nothing records
/// the host; its accept records known_hosts, never the session's set, and
/// the changed-key answer is no answer to it.
#[test]
fn an_unknown_host_is_still_asked_about_and_recorded_globally() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = make_key(ED25519_KEY_A);

    let (v, asked) = asking(
        HostKeyPolicy::Strict,
        &path,
        HostKeyDecision::AcceptAndSaveSession,
    );
    assert_eq!(v.verify(&make_facts(), &key), HostKeyDecision::Reject);
    assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Unknown]);
    assert!(!path.exists());
    assert!(v.session_keys().keys().is_empty());

    let (v, asked) = asking(HostKeyPolicy::Strict, &path, HostKeyDecision::AcceptAndSave);
    assert_eq!(
        v.verify(&make_facts(), &key),
        HostKeyDecision::AcceptAndSave
    );
    assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Unknown]);
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains(ED25519_KEY_A));
    assert!(v.session_keys().keys().is_empty());
}

/// Whatever keeps known_hosts from answering still rejects when the
/// session trusts the key: the file could revoke it. Only `off` connects.
#[test]
fn failing_closed_outranks_the_sessions_set() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_file = dir.path().to_path_buf();
    let key = make_key(ED25519_KEY_A);
    for (policy, expected) in [
        (HostKeyPolicy::Strict, HostKeyDecision::Reject),
        (HostKeyPolicy::AcceptNew, HostKeyDecision::Reject),
        (HostKeyPolicy::Off, HostKeyDecision::Accept),
    ] {
        let (v, asked) = asking_trusting(
            policy.clone(),
            &not_a_file,
            HostKeyDecision::AcceptAndSaveSession,
            &[ED25519_KEY_A],
        );
        assert_eq!(v.verify(&make_facts(), &key), expected, "{policy:?}");
        assert!(asked.lock().unwrap().is_empty());
    }

    let facts = HostKeyFacts {
        host: "*".into(),
        ..make_facts()
    };
    let path = dir.path().join("known_hosts");
    let (v, asked) = asking_trusting(
        HostKeyPolicy::Strict,
        &path,
        HostKeyDecision::AcceptAndSaveSession,
        &[ED25519_KEY_A],
    );
    assert_eq!(v.verify(&facts, &key), HostKeyDecision::Reject);
    assert!(asked.lock().unwrap().is_empty());
}

/// The types of the keys the session trusts count as recorded: asked for
/// first, beside those known_hosts records.
#[test]
fn the_types_of_session_trusted_keys_are_offered_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let ecdsa = SessionHostKey {
        key_type: "ecdsa-sha2-nistp256".into(),
        key: ECDSA_KEY.into(),
    };
    let v = verifier(
        HostKeyPolicy::Strict,
        HostKeyDecision::Reject,
        Some(path.clone()),
    )
    .with_session_keys(SessionKeySet::new([ecdsa]));
    let p256 = Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP256,
    };
    let order = v.key_preference("localhost", 13265);
    assert_eq!(order[0], p256, "no known_hosts entry");
    assert_eq!(order[1], Algorithm::Ed25519, "the rest keep their order");
    assert_eq!(order.len(), Preferred::DEFAULT.key.len());

    // Beside known_hosts' types, in the default order.
    std::fs::write(&path, "[localhost]:13265 ssh-rsa AAAAB3NzaC1yc2E\n").unwrap();
    let order = v.key_preference("localhost", 13265);
    let first: Vec<_> = order.iter().take(4).cloned().collect();
    assert!(first.contains(&p256), "{first:?}");
    assert!(
        first[..]
            .iter()
            .filter(|algorithm| matches!(algorithm, Algorithm::Rsa { .. }))
            .count()
            == 3
    );
    assert!(!first.contains(&Algorithm::Ed25519));
    assert_eq!(order[4], Algorithm::Ed25519);
}

#[test]
fn a_session_key_set_is_seeded_unpadded_and_reports_only_what_it_gained() {
    let padded = SessionHostKey {
        key_type: "ssh-ed25519".into(),
        key: format!("{ED25519_KEY_A}=="),
    };
    let set = SessionKeySet::new([padded.clone(), trusted(ED25519_KEY_A)]);
    assert_eq!(
        set.keys(),
        [trusted(ED25519_KEY_A)],
        "deduplicated, unpadded"
    );
    assert!(set.added().is_empty());

    let clone = set.clone();
    clone.add(trusted(ED25519_KEY_B));
    clone.add(padded);
    assert_eq!(set.added(), [trusted(ED25519_KEY_B)], "shared, never twice");
    assert_eq!(set.keys().len(), 2);
    assert!(SessionKeySet::default().keys().is_empty());
}
