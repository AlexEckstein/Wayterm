use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

/// Outlives its connection, so tests can assert "was disconnected".
type Flag = Arc<AtomicBool>;

struct FakeConn {
    disconnected: Flag,
    closed: AtomicBool,
}

#[async_trait]
impl PooledConnection for FakeConn {
    async fn disconnect_now(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

fn params(host: &str) -> ConnectParams {
    ConnectParams {
        host: host.to_string(),
        port: 22,
        username: "u".to_string(),
    }
}

fn key(host: &str) -> PoolKey {
    PoolKey {
        params: params(host),
        verified: true,
    }
}

async fn fake_dial(
    dials: &AtomicUsize,
    flags: &Mutex<Vec<Flag>>,
) -> Result<Arc<FakeConn>, TransportError> {
    dials.fetch_add(1, Ordering::SeqCst);
    let flag = Arc::new(AtomicBool::new(false));
    flags.lock().unwrap().push(Arc::clone(&flag));
    Ok(Arc::new(FakeConn {
        disconnected: flag,
        closed: AtomicBool::new(false),
    }))
}

fn disconnected(flags: &Mutex<Vec<Flag>>, i: usize) -> bool {
    flags.lock().unwrap()[i].load(Ordering::SeqCst)
}

#[tokio::test]
async fn reuse_returns_the_same_connection() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let first = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    let second = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(dials.load(Ordering::SeqCst), 1);
}

/// A connection dialed with host-key verification off is not the one a
/// verifying session gets, though both name the same user on the same host.
#[tokio::test]
async fn an_unverified_connection_is_not_shared_with_a_verifying_dial() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());
    let unverified = PoolKey {
        verified: false,
        ..key("a")
    };

    let lab = sup
        .get_or_connect_with(unverified.clone(), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    let strict = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&lab, &strict));
    assert_eq!(dials.load(Ordering::SeqCst), 2);

    // Each is still shared among its own kind.
    let lab_again = sup
        .get_or_connect_with(unverified, || fake_dial(&dials, &flags))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&lab, &lab_again));
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn distinct_keys_get_distinct_connections() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let a = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    let b = sup
        .get_or_connect_with(key("b"), || fake_dial(&dials, &flags))
        .await
        .unwrap();

    assert!(!Arc::ptr_eq(&a, &b));
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    assert!(!disconnected(&flags, 0) && !disconnected(&flags, 1));
}

#[tokio::test]
async fn losing_a_dial_race_adopts_the_winner_and_disconnects_its_own() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    // Both calls miss the pool lookup, then dial concurrently; the
    // slower insert must adopt the winner and close its own connection.
    let (a, b) = tokio::join!(
        sup.get_or_connect_with(key("race"), || async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            fake_dial(&dials, &flags).await
        }),
        sup.get_or_connect_with(key("race"), || async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            fake_dial(&dials, &flags).await
        }),
    );
    let a = a.unwrap();
    let b = b.unwrap();

    assert!(
        Arc::ptr_eq(&a, &b),
        "both callers must share one connection"
    );
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    assert_eq!(flags.lock().unwrap().len(), 2);
    let closed = (0..2).filter(|&i| disconnected(&flags, i)).count();
    assert_eq!(closed, 1, "the loser's fresh connection is closed");
}

#[tokio::test]
async fn last_release_disconnects_and_evicts_the_entry() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let conn = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    assert!(sup.lookup(&key("a")).is_some());

    drop(conn);
    tokio::time::sleep(MONITOR_POLL * 4).await;

    assert!(
        disconnected(&flags, 0),
        "release at zero consumers must disconnect"
    );
    assert!(sup.lookup(&key("a")).is_none(), "entry must be evicted");

    // A later request dials fresh instead of resurrecting the old one.
    let fresh = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    assert_eq!(flags.lock().unwrap().len(), 2);
    drop(fresh);
}

#[tokio::test]
async fn a_live_consumer_keeps_the_connection_open() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let conn = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();

    tokio::time::sleep(MONITOR_POLL * 5).await;
    assert!(
        !disconnected(&flags, 0),
        "monitor must not disconnect while a consumer holds the Arc"
    );

    drop(conn);
    tokio::time::sleep(MONITOR_POLL * 4).await;
    assert!(disconnected(&flags, 0));
}

#[tokio::test]
async fn supervisor_drop_still_cleans_up_on_last_release() {
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());
    let conn = {
        let sup = ConnectionSupervisor::<FakeConn>::new();
        sup.get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
            .await
            .unwrap()
    };

    // The supervisor handle is gone; the monitor still owns the pool and
    // must disconnect once the last consumer releases.
    drop(conn);
    tokio::time::sleep(MONITOR_POLL * 4).await;
    assert!(disconnected(&flags, 0));
}

#[tokio::test]
async fn a_closed_connection_is_redialled_even_while_held() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let dead = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    dead.closed.store(true, Ordering::SeqCst);

    let fresh = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&dead, &fresh));
    assert_eq!(dials.load(Ordering::SeqCst), 2);

    // The dead one's monitor must not take the fresh entry down with it.
    drop(dead);
    tokio::time::sleep(MONITOR_POLL * 4).await;
    assert!(!disconnected(&flags, 1));
    let again = sup.lookup(&key("a")).expect("fresh entry stays pooled");
    assert!(Arc::ptr_eq(&again, &fresh));
}

#[test]
fn keepalive_reads_stored_options_over_the_default() {
    let options = |interval, count| SshOptions {
        compression: false,
        keepalive_interval: interval,
        keepalive_count: count,
        ssh_w_fallback: false,
    };
    let default = Keepalive::default();
    assert_eq!(Keepalive::from_options(None), default);
    assert_eq!(Keepalive::from_options(Some(&options(None, None))), default);
    assert_eq!(
        Keepalive::from_options(Some(&options(Some(30), Some(5)))),
        Keepalive {
            interval: Some(Duration::from_secs(30)),
            max: 5,
        }
    );
    assert_eq!(
        Keepalive::from_options(Some(&options(Some(0), None))),
        Keepalive {
            interval: None,
            max: default.max,
        }
    );
}

#[test]
fn client_config_carries_the_keepalive_and_the_hosts_key_types() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    std::fs::write(&known_hosts, "a ssh-rsa AAAAB3NzaC1yc2E\n").unwrap();
    let auth = HopAuth {
        method: AuthMethod::None,
        creds: CredentialManager::new(Vec::new()),
        verifier: HostKeyVerifier::new(
            crate::store::model::HostKeyPolicy::Strict,
            Arc::new(|_| crate::transport::hostkey::HostKeyDecision::Reject),
            Some(Arc::from(known_hosts)),
        ),
        responder: None,
        keepalive: Keepalive {
            interval: Some(Duration::from_secs(7)),
            max: 2,
        },
    };
    let config = client_config(&params("a"), &auth);
    assert_eq!(config.keepalive_interval, Some(Duration::from_secs(7)));
    assert_eq!(config.keepalive_max, 2);
    assert!(matches!(
        config.preferred.key[0],
        russh::keys::Algorithm::Rsa { .. }
    ));
    let unknown = client_config(&params("b"), &auth);
    assert_eq!(unknown.preferred.key, Preferred::DEFAULT.key);
}

#[tokio::test]
async fn dial_errors_propagate_and_leave_no_entry() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let result = sup
        .get_or_connect_with(key("boom"), || async {
            Err(TransportError::ConnectionFailed("dial failed".into()))
        })
        .await;

    assert!(matches!(result, Err(TransportError::ConnectionFailed(_))));
    assert!(sup.lookup(&key("boom")).is_none());
}

#[test]
fn connect_error_classifies_host_key_rejection() {
    let rejected = connect_error(
        "example.test",
        HandlerError::HostKeyRejected("example.test".into()),
    );
    assert!(matches!(rejected, TransportError::HostKeyRejected(h) if h == "example.test"));

    let unknown = connect_error("example.test", russh::Error::UnknownKey.into());
    assert!(matches!(unknown, TransportError::HostKeyRejected(_)));

    // A message that merely reads like a rejection is still a failure.
    let lookalike = russh::Error::from(std::io::Error::other("host key rejected for x"));
    let lookalike = connect_error("example.test", lookalike.into());
    assert!(matches!(lookalike, TransportError::ConnectionFailed(_)));

    let failed = connect_error("example.test", russh::Error::SendError.into());
    assert!(matches!(failed, TransportError::ConnectionFailed(_)));
    let certificate = connect_error("example.test", HandlerError::HostCertificate);
    assert!(
        matches!(certificate, TransportError::ConnectionFailed(m) if m.contains("certificate"))
    );
}
