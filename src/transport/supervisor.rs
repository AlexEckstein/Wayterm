//! [`ConnectionSupervisor`]: one authenticated SSH session per
//! (host, port, username), shared by every consumer and disconnected after
//! the last one releases it (doc 04 §Connection Supervisor).

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use russh::client::{self, Config, Handle};

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
    wrap_and_authenticate(handle, params, method, creds, responder).await
}

/// Wrap a fresh russh handle in an [`SshConnection`] and run the auth chain
/// on it (doc 11 §Flow), disconnecting on failure. Shared by the direct
/// dial above and the jump dial (`transport::jump`), which arrives with a
/// handle built over a direct-tcpip channel instead of a TCP socket.
pub(crate) async fn wrap_and_authenticate(
    handle: Handle<AppClientHandler>,
    params: &ConnectParams,
    method: &AuthMethod,
    creds: &CredentialManager,
    responder: Option<ChallengeResponder>,
) -> Result<SshConnection, TransportError> {
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
pub(crate) fn connect_error(host: &str, err: russh::Error) -> TransportError {
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
mod tests;
