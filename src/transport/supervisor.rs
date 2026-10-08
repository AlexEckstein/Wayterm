//! [`ConnectionSupervisor`]: one authenticated SSH session per
//! (host, port, username), shared by every consumer and disconnected after
//! the last one releases it (doc 04 §Connection Supervisor).

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

/// How often a release monitor checks for the last consumer (short in unit
/// tests so release assertions stay fast).
#[cfg(not(test))]
const MONITOR_POLL: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(test)]
const MONITOR_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Where to dial, and the pool key: one connection per identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// What the pool holds: anything it can disconnect once released.
#[async_trait]
pub trait PooledConnection: Send + Sync + 'static {
    async fn disconnect_now(&self);
}

#[async_trait]
impl PooledConnection for SshConnection {
    async fn disconnect_now(&self) {
        let _ = self.disconnect().await;
    }
}

/// Shared with the release monitors, which outlive the supervisor handle.
type Pool<T> = Arc<Mutex<HashMap<ConnectParams, Arc<T>>>>;

fn lock_pool<T>(pool: &Pool<T>) -> MutexGuard<'_, HashMap<ConnectParams, Arc<T>>> {
    // A poisoned lock still guards a consistent map.
    pool.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hands out shared `Arc`s; a monitor task per connection disconnects and
/// evicts it once only the pool's own reference is left.
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

    fn lookup(&self, key: &ConnectParams) -> Option<Arc<T>> {
        lock_pool(&self.pool).get(key).map(Arc::clone)
    }

    /// Reuse the pooled connection for `key`, else `dial` one. No lock is
    /// held across the dial, so two dials for one key may race; the loser
    /// disconnects its own connection and adopts the winner's.
    async fn get_or_connect_with<F, Fut>(
        &self,
        key: ConnectParams,
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
    /// The pooled connection for `params`, dialing and authenticating only
    /// when none is live.
    pub async fn get_or_connect(
        &self,
        params: &ConnectParams,
        method: &AuthMethod,
        creds: &CredentialManager,
        verifier: HostKeyVerifier,
        responder: Option<ChallengeResponder>,
    ) -> Result<Arc<SshConnection>, TransportError> {
        self.get_or_connect_with(params.clone(), || async {
            let conn = dial_ssh(params, method, creds, verifier, responder).await?;
            Ok(Arc::new(conn))
        })
        .await
    }
}

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

/// A rejected host key is its own outcome (doc 11), not a connect failure.
fn connect_error(host: &str, err: russh::Error) -> TransportError {
    if matches!(err, russh::Error::UnknownKey) || err.to_string().contains("host key rejected for")
    {
        TransportError::HostKeyRejected(host.to_string())
    } else {
        TransportError::ConnectionFailed(err.to_string())
    }
}

/// Disconnect and evict once every consumer has released the connection.
/// Consumers clone under the pool lock and the count is read under it too,
/// so a strong count of 1 means exactly "only the pool's `Arc` is left".
async fn monitored_release<T: PooledConnection>(key: ConnectParams, weak: Weak<T>, pool: Pool<T>) {
    loop {
        tokio::time::sleep(MONITOR_POLL).await;
        let released = {
            let mut guard = lock_pool(&pool);
            match guard.get(&key) {
                // Evicted, or replaced by a later dial.
                None => return,
                Some(conn) if Arc::as_ptr(conn) != weak.as_ptr() => return,
                Some(conn) if Arc::strong_count(conn) > 1 => {
                    drop(guard);
                    continue;
                }
                Some(_) => guard.remove(&key),
            }
        };
        if let Some(conn) = released {
            conn.disconnect_now().await;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
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
}
