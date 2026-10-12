use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

/// Outlives its connection, so tests can assert "was disconnected".
type Flag = Arc<AtomicBool>;

struct FakeConn {
    disconnected: Flag,
}

#[async_trait]
impl PooledConnection for FakeConn {
    async fn disconnect_now(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
    }
}

fn key(host: &str) -> ConnectParams {
    ConnectParams {
        host: host.to_string(),
        port: 22,
        username: "u".to_string(),
    }
}

async fn fake_dial(
    dials: &AtomicUsize,
    flags: &Mutex<Vec<Flag>>,
) -> Result<Arc<FakeConn>, TransportError> {
    dials.fetch_add(1, Ordering::SeqCst);
    let flag = Arc::new(AtomicBool::new(false));
    flags.lock().unwrap().push(Arc::clone(&flag));
    Ok(Arc::new(FakeConn { disconnected: flag }))
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
        russh::Error::from(std::io::Error::other("host key rejected for example.test")),
    );
    assert!(matches!(rejected, TransportError::HostKeyRejected(h) if h == "example.test"));

    let unknown = connect_error("example.test", russh::Error::UnknownKey);
    assert!(matches!(unknown, TransportError::HostKeyRejected(_)));

    let failed = connect_error("example.test", russh::Error::SendError);
    assert!(matches!(failed, TransportError::ConnectionFailed(_)));
}
