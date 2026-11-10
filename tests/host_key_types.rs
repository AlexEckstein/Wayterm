//! Which key a server holding several is asked for, and what a known host
//! showing a key of another type gets (doc 04 §Host key verification),
//! against an in-process russh server. Needs no sshd.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use russh::keys::{load_secret_key, PrivateKey};
use russh::server::{self, Auth};
use tokio::net::TcpListener;

use wayterm::store::model::HostKeyPolicy;
use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
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
/// one for that address: not a new host, under either policy.
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
        let prompted = Arc::new(AtomicBool::new(false));
        let result = ConnectionSupervisor::new()
            .get_or_connect(
                &params(addr),
                &auth(policy.clone(), &known_hosts, &prompted),
            )
            .await;
        assert!(
            matches!(result, Err(TransportError::HostKeyRejected(_))),
            "{policy:?}: {:?}",
            result.err()
        );
        assert!(!prompted.load(Ordering::SeqCst), "{policy:?} prompted");
        assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), recorded);
    }
}
