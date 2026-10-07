use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use russh::keys::{load_public_key, PublicKey};

use crate::store::model::HostKeyPolicy;
use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyVerifier};

const ED25519_KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
const ED25519_KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

fn make_key(blob: &str) -> PublicKey {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("key.pub");
    std::fs::write(&path, format!("ssh-ed25519 {} test\n", blob)).unwrap();
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
