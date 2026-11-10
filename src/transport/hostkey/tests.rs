use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use russh::keys::{load_public_key, Algorithm, EcdsaCurve, PublicKey};
use russh::Preferred;

use crate::store::model::HostKeyPolicy;
use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};

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

#[test]
fn mismatched_key_fails_without_prompt() {
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
/// changed its key, under every policy: no prompt, and nothing recorded.
#[test]
fn a_key_of_another_type_is_rejected_for_a_known_host() {
    for policy in [
        HostKeyPolicy::Strict,
        HostKeyPolicy::AcceptNew,
        HostKeyPolicy::Off,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let recorded = format!("[localhost]:13265 ecdsa-sha2-nistp256 {ECDSA_KEY}\n");
        std::fs::write(&path, &recorded).unwrap();

        let (v, called) = verifier_with_prompt(policy.clone(), Some(path.clone()));
        let presented = make_key(ED25519_KEY_A);
        assert_eq!(
            v.verify(&make_facts(), &presented),
            HostKeyDecision::Reject,
            "{policy:?}"
        );
        assert!(
            !called.load(Ordering::Relaxed),
            "{policy:?} must not prompt"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), recorded);
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
