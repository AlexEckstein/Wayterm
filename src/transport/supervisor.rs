//! Connection supervisor — one authenticated SSH session per identity,
//! shared by consumers and disconnected when the last consumer releases it
//! (doc 04 §Connection Supervisor).
//!
//! Design contracts:
//!   - docs/04-transport-layer.md §Connection Supervisor

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use russh::client::{self, Config};

use crate::transport::auth::ChallengeResponder;
use crate::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
use crate::transport::credentials::CredentialManager;
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::types::TransportError;

/// How often each release monitor checks whether its connection has been
/// fully released by every consumer. Short in unit tests so release-counting
/// assertions do not take seconds.
#[cfg(not(test))]
const MONITOR_POLL: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(test)]
const MONITOR_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Pool identity: a connection is shared exactly per (host, port, username)
/// (doc 04).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// Everything needed to dial a new SSH connection.
#[derive(Clone, Debug)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// A connection type the supervisor can tear down once every consumer has
/// released it.
#[async_trait]
pub trait PooledConnection: Send + Sync + 'static {
    /// Close this connection (graceful disconnect for SSH).
    async fn disconnect_now(&self);
}

#[async_trait]
impl PooledConnection for SshConnection {
    async fn disconnect_now(&self) {
        let _ = self.disconnect().await;
    }
}

/// Pool map, reference-counted so release monitors keep it alive after the
/// supervisor handle itself is dropped.
type Pool<T> = Arc<Mutex<HashMap<ConnectionKey, Arc<T>>>>;

fn lock_pool<T>(pool: &Pool<T>) -> MutexGuard<'_, HashMap<ConnectionKey, Arc<T>>> {
    // Poisoning only means some earlier task panicked while touching the
    // map; the map invariants still hold, so recover instead of cascading.
    match pool.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Shares one authenticated connection per [`ConnectionKey`].
///
/// Consumers receive `Arc`s; when the last consumer drops its handle and the
/// supervisor no longer holds one either, a monitor task disconnects the
/// session and evicts the pool entry (release counting, doc 04).
pub struct ConnectionSupervisor<T: PooledConnection = SshConnection> {
    pool: Pool<T>,
}

impl<T: PooledConnection> Default for ConnectionSupervisor<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: PooledConnection> ConnectionSupervisor<T> {
    pub fn new() -> Self {
        Self {
            pool: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Pool lookup: the live connection for `key`, if one exists.
    fn lookup(&self, key: &ConnectionKey) -> Option<Arc<T>> {
        lock_pool(&self.pool).get(key).map(Arc::clone)
    }

    /// Core of [`Self::get_or_connect`]: reuse a pooled connection, else
    /// await `dial`, then insert the winner and arm its release monitor.
    ///
    /// Two concurrent dials for the same key are both allowed to race (no
    /// lock is held across the await); the loser disconnects its own fresh
    /// connection and adopts the winner's, so consumers never observe two
    /// live connections for one key.
    async fn get_or_connect_with<F, Fut>(
        &self,
        key: ConnectionKey,
        dial: F,
    ) -> Result<Arc<T>, TransportError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<T>, TransportError>>,
    {
        if let Some(conn) = self.lookup(&key) {
            return Ok(conn);
        }
        let conn = dial().await?;
        // The pool lock is scoped to the map mutation only — never held
        // across an await (the loser's `disconnect_now` runs lock-free).
        let winner = {
            let mut pool = lock_pool(&self.pool);
            match pool.entry(key.clone()) {
                MapEntry::Occupied(existing) => Some(Arc::clone(existing.get())),
                MapEntry::Vacant(slot) => {
                    slot.insert(Arc::clone(&conn));
                    None
                }
            }
        };
        match winner {
            Some(theirs) => {
                // A concurrent dial beat us: drop ours, adopt theirs, so
                // consumers never observe two live connections for `key`.
                conn.disconnect_now().await;
                Ok(theirs)
            }
            None => {
                let pool = Arc::clone(&self.pool);
                let weak = Arc::downgrade(&conn);
                tokio::spawn(monitored_release(key, weak, pool));
                Ok(conn)
            }
        }
    }
}

impl ConnectionSupervisor<SshConnection> {
    /// Return the pooled SSH connection for `params`, dialing and
    /// authenticating only when no live connection exists (doc 04).
    pub async fn get_or_connect(
        &self,
        params: &ConnectParams,
        method: &AuthMethod,
        creds: &CredentialManager,
        verifier: HostKeyVerifier,
        responder: Option<ChallengeResponder>,
    ) -> Result<Arc<SshConnection>, TransportError> {
        let key = ConnectionKey {
            host: params.host.clone(),
            port: params.port,
            username: params.username.clone(),
        };
        self.get_or_connect_with(key, || async {
            let conn = dial_ssh(params, method, creds, verifier, responder).await?;
            Ok(Arc::new(conn))
        })
        .await
    }
}

/// Dial + authenticate one SSH session (doc 04 §Connect).
async fn dial_ssh(
    params: &ConnectParams,
    method: &AuthMethod,
    creds: &CredentialManager,
    verifier: HostKeyVerifier,
    responder: Option<ChallengeResponder>,
) -> Result<SshConnection, TransportError> {
    let config = Arc::new(Config::default());
    let handler = AppClientHandler::new(
        params.host.clone(),
        params.port,
        params.username.clone(),
        verifier,
        None,
    );
    // `ToSocketAddrs` lets tokio resolve hostnames for us.
    let handle = client::connect(config, (params.host.as_str(), params.port), handler)
        .await
        .map_err(|err| connect_error(&params.host, err))?;
    let conn = SshConnection::new(
        handle,
        params.host.clone(),
        params.port,
        params.username.clone(),
    );
    if let Err(err) = conn.authenticate(method, creds, responder).await {
        let _ = conn.disconnect().await;
        return Err(err);
    }
    Ok(conn)
}

/// Map a russh connect error onto the transport taxonomy: a rejected host
/// key is its own outcome (doc 04 §Host keys), everything else is a
/// connection failure.
fn connect_error(host: &str, err: russh::Error) -> TransportError {
    if matches!(err, russh::Error::UnknownKey) || err.to_string().contains("host key rejected for")
    {
        TransportError::HostKeyRejected(host.to_string())
    } else {
        TransportError::ConnectionFailed(err.to_string())
    }
}

/// Disconnect a connection once every external consumer has released it,
/// then evict it from the pool so the next `get_or_connect` dials fresh.
///
/// The strong count is only ever inspected while holding the pool lock, and
/// consumers clone their handle under the same lock, so "count == 1" means
/// exactly "only the map's own `Arc` remains" — no TOCTOU window.
async fn monitored_release<T: PooledConnection>(key: ConnectionKey, weak: Weak<T>, pool: Pool<T>) {
    loop {
        tokio::time::sleep(MONITOR_POLL).await;
        let released = {
            let mut guard = lock_pool(&pool);
            match guard.get(&key) {
                // Entry evicted elsewhere, or replaced by a later dial:
                // nothing left for this monitor to do.
                None => return,
                Some(conn) if Arc::as_ptr(conn) != weak.as_ptr() => return,
                // Still held by at least one consumer — keep monitoring.
                Some(conn) if Arc::strong_count(conn) > 1 => {
                    drop(guard);
                    continue;
                }
                // Last reference is the map's own — take it out of the pool
                // under the lock, disconnect outside it.
                Some(_) => guard.remove(&key),
            }
        };
        if let Some(conn) = released {
            conn.disconnect_now().await;
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// Observation handle that outlives the connection it belongs to, so
    /// tests can assert "was disconnected" after the object is dropped.
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

    fn key(host: &str) -> ConnectionKey {
        ConnectionKey {
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
}
