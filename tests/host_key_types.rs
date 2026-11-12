//! Which key a server holding several is asked for, what a known host
//! showing a key of another type gets, and how a session comes to trust a
//! changed key (doc 04 §Host key verification), against an in-process
//! russh server. Needs no sshd.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use russh::keys::{load_secret_key, PrivateKey};
use russh::server::{self, Auth};
use tokio::net::TcpListener;

use wayterm::store::model::{HostKeyPolicy, SessionHostKey};
use wayterm::transport::hostkey::{
    HostKeyDecision, HostKeyQuestion, HostKeyVerifier, SessionKeySet,
};
use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth};
use wayterm::transport::types::TransportError;

struct Server;

impl server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }
}

/// A server on loopback holding `keys` as its host keys.
async fn start_server(keys: Vec<PrivateKey>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Arc::new(server::Config {
        keys,
        ..Default::default()
    });
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, stream, Server).await {
                    let _ = session.await;
                }
            });
        }
    });
    addr
}

/// A throwaway ECDSA key, generated per run so none is committed; `None`
/// where `ssh-keygen` is missing.
fn ecdsa_key(dir: &Path) -> Option<PrivateKey> {
    let path = dir.join("ecdsa");
    let generated = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ecdsa", "-b", "256", "-N", "", "-C", "", "-f"])
        .arg(&path)
        .status()
        .is_ok_and(|status| status.success());
    if !generated {
        println!("Skipping: no ssh-keygen to make an ECDSA host key");
        return None;
    }
    Some(load_secret_key(&path, None).expect("load the generated key"))
}

fn params(addr: SocketAddr) -> ConnectParams {
    ConnectParams {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "user".into(),
    }
}

/// `key` as another client would have recorded it for the server at `addr`.
fn record(known_hosts: &Path, addr: SocketAddr, key: &PrivateKey) -> String {
    let line = format!(
        "[{}]:{} {}\n",
        addr.ip(),
        addr.port(),
        key.public_key().to_openssh().unwrap()
    );
    std::fs::write(known_hosts, &line).unwrap();
    line
}

/// Password auth under `policy` against `known_hosts`; `prompted` records
/// a prompt, which rejects.
fn auth(policy: HostKeyPolicy, known_hosts: &Path, prompted: &Arc<AtomicBool>) -> HopAuth {
    let prompted = Arc::clone(prompted);
    HopAuth {
        verifier: HostKeyVerifier::new(
            policy,
            Arc::new(move |_| {
                prompted.store(true, Ordering::SeqCst);
                HostKeyDecision::Reject
            }),
            Some(Arc::from(known_hosts)),
        ),
        ..common::password_auth("any")
    }
}

/// A server holding an Ed25519 and an ECDSA key, known by the ECDSA one:
/// russh would ask for Ed25519 first, which known_hosts does not hold.
#[tokio::test]
async fn a_host_is_asked_for_the_key_type_known_hosts_holds() {
    let dir = tempfile::tempdir().unwrap();
    let Some(ecdsa) = ecdsa_key(dir.path()) else {
        return;
    };
    let addr = start_server(vec![common::host_key(), ecdsa.clone()]).await;
    let known_hosts = dir.path().join("known_hosts");
    let recorded = record(&known_hosts, addr, &ecdsa);
    let prompted = Arc::new(AtomicBool::new(false));

    let connected = ConnectionSupervisor::new()
        .get_or_connect(
            &params(addr),
            &auth(HostKeyPolicy::Strict, &known_hosts, &prompted),
        )
        .await;
    assert!(connected.is_ok(), "{:?}", connected.err());
    assert!(
        !prompted.load(Ordering::SeqCst),
        "a known host was asked about"
    );
    assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), recorded);
}

/// A server holding only an Ed25519 key where known_hosts holds an ECDSA
/// one for that address: not a new host, under either policy, but a
/// changed key, and denying it rejects the connection.
#[tokio::test]
async fn a_known_host_showing_a_key_of_another_type_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let Some(ecdsa) = ecdsa_key(dir.path()) else {
        return;
    };
    let addr = start_server(vec![common::host_key()]).await;
    let known_hosts = dir.path().join("known_hosts");
    let recorded = record(&known_hosts, addr, &ecdsa);

    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        let (auth, asked) = asking(policy.clone(), &known_hosts, HostKeyDecision::Reject, &[]);
        let result = ConnectionSupervisor::new()
            .get_or_connect(&params(addr), &auth)
            .await;
        assert!(
            matches!(result, Err(TransportError::HostKeyRejected(_))),
            "{policy:?}: {:?}",
            result.err()
        );
        assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
        assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), recorded);
    }
}

/// `key`'s public half as a session key set holds it: type and unpadded
/// base64 blob.
fn session_key(key: &PrivateKey) -> SessionHostKey {
    let line = key.public_key().to_openssh().unwrap();
    let mut fields = line.split(' ');
    SessionHostKey {
        key_type: fields.next().unwrap().to_string(),
        key: fields.next().unwrap().trim_end_matches('=').to_string(),
    }
}

/// Password auth under `policy` against `known_hosts`, for a session
/// trusting `keys`; the prompt records each question and answers `answer`.
fn asking(
    policy: HostKeyPolicy,
    known_hosts: &Path,
    answer: HostKeyDecision,
    keys: &[SessionHostKey],
) -> (HopAuth, Arc<Mutex<Vec<HostKeyQuestion>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    let verifier = HostKeyVerifier::asking(
        policy,
        Arc::new(move |question, _| {
            log.lock().unwrap().push(question);
            answer
        }),
        Some(Arc::from(known_hosts)),
    )
    .with_session_keys(SessionKeySet::new(keys.to_vec()));
    let auth = HopAuth {
        verifier,
        ..common::password_auth("any")
    };
    (auth, asked)
}

/// A server whose key changed: known_hosts records another one for its
/// address.
async fn changed_host(dir: &Path) -> (SocketAddr, PrivateKey, std::path::PathBuf, String) {
    let server_key = common::host_key();
    let addr = start_server(vec![server_key.clone()]).await;
    let known_hosts = dir.join("known_hosts");
    let recorded = record(&known_hosts, addr, &common::host_key());
    (addr, server_key, known_hosts, recorded)
}

/// Accepting a changed key for the session connects, adds the key to the
/// session's set, and leaves known_hosts as it was.
#[tokio::test]
async fn a_changed_key_accepted_for_the_session_connects_and_is_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, server_key, known_hosts, recorded) = changed_host(dir.path()).await;

    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        let (auth, asked) = asking(
            policy.clone(),
            &known_hosts,
            HostKeyDecision::AcceptAndSaveSession,
            &[],
        );
        let connected = ConnectionSupervisor::new()
            .get_or_connect(&params(addr), &auth)
            .await;
        assert!(connected.is_ok(), "{policy:?}: {:?}", connected.err());
        assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
        assert_eq!(
            auth.verifier.session_keys().added(),
            [session_key(&server_key)]
        );
        assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), recorded);
    }
}

/// Denying a changed key is a host-key rejection, never an auth failure,
/// and trusts nothing.
#[tokio::test]
async fn a_changed_key_denied_is_a_host_key_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _, known_hosts, recorded) = changed_host(dir.path()).await;
    let (auth, asked) = asking(
        HostKeyPolicy::Strict,
        &known_hosts,
        HostKeyDecision::Reject,
        &[],
    );
    let result = ConnectionSupervisor::new()
        .get_or_connect(&params(addr), &auth)
        .await;
    assert!(
        matches!(result, Err(TransportError::HostKeyRejected(_))),
        "{:?}",
        result.err()
    );
    assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
    assert!(auth.verifier.session_keys().keys().is_empty());
    assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), recorded);
}

/// Once the session trusts the key, a redial connects without a question;
/// a session that does not trust it is never handed the connection the
/// first one accepted, and is asked for itself.
#[tokio::test]
async fn a_redial_with_the_accepted_key_does_not_ask_again() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, server_key, known_hosts, _) = changed_host(dir.path()).await;
    let supervisor = ConnectionSupervisor::new();
    let (auth, _) = asking(
        HostKeyPolicy::Strict,
        &known_hosts,
        HostKeyDecision::AcceptAndSaveSession,
        &[],
    );
    let accepted = supervisor
        .get_or_connect(&params(addr), &auth)
        .await
        .expect("accepted for the session");
    let trusted = auth.verifier.session_keys().keys();
    assert_eq!(trusted, [session_key(&server_key)]);

    // The same bookmark, its set saved: no question, on a fresh pool too.
    for pool in [&supervisor, &ConnectionSupervisor::new()] {
        let (auth, asked) = asking(
            HostKeyPolicy::Strict,
            &known_hosts,
            HostKeyDecision::Reject,
            &trusted,
        );
        let redial = pool.get_or_connect(&params(addr), &auth).await;
        assert!(redial.is_ok(), "{:?}", redial.err());
        assert!(asked.lock().unwrap().is_empty(), "asked again");
    }

    // Another bookmark to the same user@host, trusting nothing of its own.
    let (other, asked) = asking(
        HostKeyPolicy::Strict,
        &known_hosts,
        HostKeyDecision::Reject,
        &[],
    );
    let result = supervisor.get_or_connect(&params(addr), &other).await;
    assert!(
        matches!(result, Err(TransportError::HostKeyRejected(_))),
        "handed the connection another session accepted: {:?}",
        result.err()
    );
    assert_eq!(*asked.lock().unwrap(), [HostKeyQuestion::Changed]);
    drop(accepted);
}

/// A server holding an Ed25519 and an ECDSA key, the session trusting the
/// ECDSA one and known_hosts holding nothing: the session's key type is
/// asked for first, so the connection needs no question.
#[tokio::test]
async fn a_host_is_asked_for_the_key_type_the_session_trusts() {
    let dir = tempfile::tempdir().unwrap();
    let Some(ecdsa) = ecdsa_key(dir.path()) else {
        return;
    };
    let addr = start_server(vec![common::host_key(), ecdsa.clone()]).await;
    let known_hosts = dir.path().join("known_hosts");
    let (auth, asked) = asking(
        HostKeyPolicy::Strict,
        &known_hosts,
        HostKeyDecision::Reject,
        &[session_key(&ecdsa)],
    );
    let connected = ConnectionSupervisor::new()
        .get_or_connect(&params(addr), &auth)
        .await;
    assert!(connected.is_ok(), "{:?}", connected.err());
    assert!(asked.lock().unwrap().is_empty(), "asked: {asked:?}");
    assert!(!known_hosts.exists());
}
