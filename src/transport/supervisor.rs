//! [`ConnectionSupervisor`]: one authenticated SSH session per
//! (host, port, username), shared by every consumer and disconnected after
//! the last one releases it (doc 04 §Connection supervisor).

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use russh::client::{self, Config, Handle};

use crate::transport::auth::ChallengeResponder;
use crate::transport::connection::{AppClientHandler, AuthMethod, HandlerError, SshConnection};
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
    /// Host to dial.
    pub host: String,
    /// Port to dial.
    pub port: u16,
    /// User to authenticate as.
    pub username: String,
}

/// What the pool holds: anything it can disconnect once released.
#[async_trait]
pub trait PooledConnection: Send + Sync + 'static {
    /// Disconnect the connection.
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

/// Lock the pool, ignoring poison.
fn lock_pool<T>(pool: &Pool<T>) -> MutexGuard<'_, HashMap<ConnectParams, Arc<T>>> {
    // A poisoned lock still guards a consistent map.
    pool.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hands out shared `Arc`s; a monitor task per connection disconnects and
/// evicts it once only the pool's own reference is left.
pub struct ConnectionSupervisor<T: PooledConnection = SshConnection> {
    /// Live connections by identity.
    pool: Pool<T>,
}

impl<T: PooledConnection> Default for ConnectionSupervisor<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: PooledConnection> ConnectionSupervisor<T> {
    /// An empty pool.
    pub fn new() -> Self {
        Self {
            pool: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The pooled connection for `key`, if any.
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

/// How one hop authenticates (doc 04 §Authentication): the method, where its secrets
/// come from, the host-key check, and who answers challenges. Each hop of
/// a jumped connection brings its own.
pub struct HopAuth {
    /// The session's preferred method.
    pub method: AuthMethod,
    /// Where secrets come from.
    pub creds: CredentialManager,
    /// Checks the hop's host key.
    pub verifier: HostKeyVerifier,
    /// Answers keyboard-interactive and re-prompts; `None` declines.
    pub responder: Option<ChallengeResponder>,
}

impl ConnectionSupervisor<SshConnection> {
    /// The pooled connection for `params`, dialing and authenticating with
    /// `auth` only when none is live.
    pub async fn get_or_connect(
        &self,
        params: &ConnectParams,
        auth: &HopAuth,
    ) -> Result<Arc<SshConnection>, TransportError> {
        self.get_or_connect_with(params.clone(), || async {
            let handler =
                AppClientHandler::new(params.host.clone(), params.port, auth.verifier.clone());
            let handle = client::connect(
                Arc::new(Config::default()),
                (params.host.as_str(), params.port),
                handler,
            )
            .await
            .map_err(|err| connect_error(&params.host, err))?;
            Ok(Arc::new(authenticated(handle, params, auth).await?))
        })
        .await
    }
}

/// Wrap a fresh russh handle in an [`SshConnection`] and run the auth chain
/// on it (doc 04 §Authentication), disconnecting on failure. Shared by the direct
/// dial above and the jump dial (`transport::jump`), which arrives with a
/// handle built over a direct-tcpip channel instead of a TCP socket.
pub(crate) async fn authenticated(
    handle: Handle<AppClientHandler>,
    params: &ConnectParams,
    auth: &HopAuth,
) -> Result<SshConnection, TransportError> {
    let conn = SshConnection::new(
        handle,
        params.host.clone(),
        params.port,
        params.username.clone(),
    );
    let authenticated = conn
        .authenticate(&auth.method, &auth.creds, auth.responder.clone())
        .await;
    if let Err(err) = authenticated {
        let _ = conn.disconnect().await;
        return Err(err);
    }
    Ok(conn)
}

/// A rejected host key is its own outcome (doc 04), not a connect failure.
pub(crate) fn connect_error(host: &str, err: HandlerError) -> TransportError {
    match err {
        HandlerError::HostKeyRejected(_) | HandlerError::Ssh(russh::Error::UnknownKey) => {
            TransportError::HostKeyRejected(host.to_string())
        }
        other => TransportError::ConnectionFailed(other.to_string()),
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
mod tests;
