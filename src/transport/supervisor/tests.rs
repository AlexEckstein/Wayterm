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
        session_keys: BTreeSet::new(),
    }
}

fn host_key(key: &str) -> SessionHostKey {
    SessionHostKey {
        key_type: "ssh-ed25519".to_string(),
        key: key.to_string(),
    }
}

/// `key("a")` for a session trusting `keys`.
fn trusting(keys: &[&str]) -> PoolKey {
    PoolKey {
        session_keys: keys.iter().map(|key| host_key(key)).collect(),
        ..key("a")
    }
}

/// A hop under `policy` whose session trusts `keys`.
fn hop(policy: crate::store::model::HostKeyPolicy, keys: &[&str]) -> HopAuth {
    use crate::transport::hostkey::{HostKeyDecision, SessionKeySet};
    HopAuth {
        method: AuthMethod::None,
        creds: CredentialManager::new(Vec::new()),
        verifier: HostKeyVerifier::new(policy, Arc::new(|_| HostKeyDecision::Reject), None)
            .with_session_keys(SessionKeySet::new(keys.iter().map(|key| host_key(key)))),
        responder: None,
        keepalive: Keepalive::default(),
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

/// The pool key of a hop names its session's trusted keys as a set: two
/// sessions trusting different keys get different keys, the same keys in
/// any order the same one, and verification on or off still counts.
#[test]
fn the_pool_key_tells_session_key_sets_apart() {
    use crate::store::model::HostKeyPolicy::{Off, Strict};
    let of = |policy, keys: &[&str]| PoolKey::for_hop(&params("a"), &hop(policy, keys));
    assert_eq!(of(Strict, &[]), key("a"), "no keys of its own: as before");
    assert_eq!(of(Strict, &[]), of(Strict, &[]));
    assert_eq!(of(Strict, &["K1", "K2"]), of(Strict, &["K2", "K1"]));
    assert_eq!(of(Strict, &["K1"]), of(Strict, &["K1="]), "padding aside");
    assert_ne!(of(Strict, &["K1"]), of(Strict, &[]));
    assert_ne!(of(Strict, &["K1"]), of(Strict, &["K2"]));
    assert_ne!(of(Strict, &["K1"]), of(Strict, &["K1", "K2"]));
    assert_ne!(of(Strict, &["K1"]), of(Off, &["K1"]));
    assert_ne!(of(Strict, &[]), of(Off, &[]));
}

/// Sessions that trust different keys for one host never share its
/// connection; sessions that trust the same ones do.
#[tokio::test]
async fn sessions_trusting_different_keys_never_share_a_connection() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());
    let dial = |pool_key: PoolKey| {
        let (sup, dials, flags) = (&sup, &dials, &flags);
        async move {
            sup.get_or_connect_with(pool_key, || fake_dial(dials, flags))
                .await
                .unwrap()
        }
    };

    let plain = dial(key("a")).await;
    let trusting_k1 = dial(trusting(&["K1"])).await;
    let trusting_k2 = dial(trusting(&["K2"])).await;
    assert!(!Arc::ptr_eq(&plain, &trusting_k1));
    assert!(!Arc::ptr_eq(&trusting_k1, &trusting_k2));
    assert_eq!(dials.load(Ordering::SeqCst), 3);

    assert!(Arc::ptr_eq(&trusting_k1, &dial(trusting(&["K1"])).await));
    assert!(Arc::ptr_eq(&plain, &dial(key("a")).await));
    assert_eq!(dials.load(Ordering::SeqCst), 3);
}

/// A dial whose session accepted a changed key is pooled under the grown
/// set: only a session trusting that key too is handed it, never one
/// still trusting what the dial started with.
#[tokio::test]
async fn a_dial_that_trusts_a_new_key_is_pooled_under_its_grown_set() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let conn = sup
        .get_or_connect_settled(
            key("a"),
            || fake_dial(&dials, &flags),
            |_| trusting(&["K1"]),
        )
        .await
        .unwrap();
    assert!(sup.lookup(&key("a")).is_none(), "not under the old set");
    let pooled = sup.lookup(&trusting(&["K1"])).expect("under the grown set");
    assert!(Arc::ptr_eq(&conn, &pooled));
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

/// The captured line holding `message`, or a panic showing all of them.
fn logged_line<'a>(output: &'a str, message: &str) -> &'a str {
    output
        .lines()
        .find(|line| line.contains(message))
        .unwrap_or_else(|| panic!("no {message:?} line in:\n{output}"))
}

/// Wait, bounded, until the fake at `i` reports disconnected.
async fn until_disconnected(flags: &Mutex<Vec<Flag>>, i: usize) {
    for _ in 0..40 {
        if disconnected(flags, i) {
            return;
        }
        tokio::time::sleep(MONITOR_POLL).await;
    }
    panic!("connection {i} was never disconnected");
}

/// The last consumer's release is logged, naming the endpoint.
#[tokio::test]
async fn a_release_by_the_last_consumer_is_logged() {
    let (buffer, guard) = crate::logging::tests::capture(None);
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let conn = sup
        .get_or_connect_with(key("release.example"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    drop(conn);
    until_disconnected(&flags, 0).await;
    drop(guard);

    let output = buffer.contents();
    let line = logged_line(&output, "last consumer released");
    assert!(line.contains("INFO"), "{line}");
    assert!(line.contains("host=release.example"), "{line}");
    assert!(line.contains("port=22"), "{line}");
}

/// A closed pooled connection is logged as evicted; reusing a live one
/// is logged at debug only.
#[tokio::test]
async fn evicting_a_closed_connection_is_logged_and_reuse_only_at_debug() {
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());

    let (buffer, guard) = crate::logging::tests::capture(None);
    let dead = sup
        .get_or_connect_with(key("evict.example"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    dead.closed.store(true, Ordering::SeqCst);
    let fresh = sup
        .get_or_connect_with(key("evict.example"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    let _reused = sup
        .get_or_connect_with(key("evict.example"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    drop(guard);
    let output = buffer.contents();
    let line = logged_line(&output, "pooled connection found closed");
    assert!(line.contains("host=evict.example"), "{line}");
    assert!(
        !output.contains("reusing the pooled connection"),
        "reuse is debug only:\n{output}"
    );

    let (buffer, guard) = crate::logging::tests::capture(Some("debug"));
    let again = sup
        .get_or_connect_with(key("evict.example"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    drop(guard);
    assert!(Arc::ptr_eq(&fresh, &again));
    let output = buffer.contents();
    let line = logged_line(&output, "reusing the pooled connection");
    assert!(line.contains("host=evict.example"), "{line}");
}

/// Dropping the supervisor logs how many connections it still pooled.
#[tokio::test]
async fn dropping_the_supervisor_logs_what_it_still_pooled() {
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());
    let (buffer, guard) = crate::logging::tests::capture(None);
    let sup = ConnectionSupervisor::<FakeConn>::new();
    let conn = sup
        .get_or_connect_with(key("a"), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    drop(sup);
    drop(guard);

    let output = buffer.contents();
    let line = logged_line(&output, "connection supervisor shut down");
    assert!(line.contains("pooled=1"), "{line}");
    assert!(!disconnected(&flags, 0), "dropping it only logs");
    drop(conn);
}

/// No supervisor event names the user a connection logs in as.
#[tokio::test]
async fn supervisor_events_never_name_the_user() {
    let user_key = PoolKey {
        params: ConnectParams {
            host: "private.example".to_string(),
            port: 22,
            username: "sekrit-user".to_string(),
        },
        verified: true,
        session_keys: BTreeSet::new(),
    };
    let dials = AtomicUsize::new(0);
    let flags = Mutex::new(Vec::new());
    let (buffer, guard) = crate::logging::tests::capture(Some("debug"));
    let sup = ConnectionSupervisor::<FakeConn>::new();

    let conn = sup
        .get_or_connect_with(user_key.clone(), || fake_dial(&dials, &flags))
        .await
        .unwrap();
    let reused = sup
        .get_or_connect_with(user_key, || fake_dial(&dials, &flags))
        .await
        .unwrap();
    drop((conn, reused));
    until_disconnected(&flags, 0).await;
    drop(sup);
    drop(guard);

    let output = buffer.contents();
    logged_line(&output, "last consumer released");
    assert!(!output.contains("sekrit-user"), "{output}");
}
