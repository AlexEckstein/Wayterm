//! [`ConnectionSupervisor`]: one authenticated SSH session per
//! (host, port, username), host-key verification on or off, and set of
//! session-trusted host keys, shared by every consumer and disconnected
//! after the last one releases it (doc 04 §Connection supervisor).

use std::collections::hash_map::Entry as MapEntry;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Config, Handle};
use russh::Preferred;

use crate::store::model::{SessionHostKey, SshOptions};
use crate::transport::auth::ChallengeResponder;
use crate::transport::connection::{AppClientHandler, AuthMethod, HandlerError, SshConnection};
use crate::transport::credentials::CredentialManager;
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::types::TransportError;

/// How often a release monitor checks for the last consumer (short in unit
/// tests so release assertions stay fast).
#[cfg(not(test))]
const MONITOR_POLL: Duration = Duration::from_millis(250);
#[cfg(test)]
const MONITOR_POLL: Duration = Duration::from_millis(25);

/// Where to dial: one identity on one endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// What a pooled connection is shared under: its identity, whether its
/// host key was checked, and the host keys its session trusts beside
/// known_hosts. A session that verifies host keys is never handed a
/// connection dialed with verification off, nor one whose key only another
/// session's set trusts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    pub params: ConnectParams,
    pub verified: bool,
    pub session_keys: BTreeSet<SessionHostKey>,
}

impl PoolKey {
    /// The key a dial to `params` with `auth` is shared under, as its
    /// session's key set stands now.
    pub(crate) fn for_hop(params: &ConnectParams, auth: &HopAuth) -> Self {
        Self {
            params: params.clone(),
            verified: auth.verifier.verifies(),
            session_keys: auth.verifier.session_keys().identity(),
        }
    }
}

/// What the pool holds: anything it can disconnect once released, and
/// tell apart once closed.
#[async_trait]
pub trait PooledConnection: Send + Sync + 'static {
    async fn disconnect_now(&self);
    fn is_closed(&self) -> bool;
}

#[async_trait]
impl PooledConnection for SshConnection {
    async fn disconnect_now(&self) {
        let _ = self.disconnect().await;
    }

    fn is_closed(&self) -> bool {
        SshConnection::is_closed(self)
    }
}

/// Shared with the release monitors, which outlive the supervisor handle.
type Pool<T> = Arc<Mutex<HashMap<PoolKey, Arc<T>>>>;

fn lock_pool<T>(pool: &Pool<T>) -> MutexGuard<'_, HashMap<PoolKey, Arc<T>>> {
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

    /// The pooled connection for `key`. A closed one is evicted instead, so
    /// the caller dials fresh rather than opening channels on a dead session.
    fn lookup(&self, key: &PoolKey) -> Option<Arc<T>> {
        let mut pool = lock_pool(&self.pool);
        let conn = pool.get(key)?;
        let (host, port) = (&key.params.host, key.params.port);
        if conn.is_closed() {
            pool.remove(key);
            tracing::info!(host = %host, port, "pooled connection found closed; evicting it");
            None
        } else {
            tracing::debug!(host = %host, port, "reusing the pooled connection");
            Some(Arc::clone(conn))
        }
    }

    /// Reuse the pooled connection for `key`, else `dial` one. No lock is
    /// held across the dial, so two dials for one key may race; the loser
    /// disconnects its own connection and adopts the winner's.
    #[cfg(test)]
    async fn get_or_connect_with<F, Fut>(
        &self,
        key: PoolKey,
        dial: F,
    ) -> Result<Arc<T>, TransportError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<T>, TransportError>>,
    {
        self.get_or_connect_settled(key, dial, PoolKey::clone).await
    }

    /// [`Self::get_or_connect_with`], pooling the new connection under
    /// `settle(&key)`: a dial whose session accepted a changed key trusts
    /// one more key than it was looked up with, and only sessions that
    /// trust it too may share the connection.
    async fn get_or_connect_settled<F, Fut, S>(
        &self,
        key: PoolKey,
        dial: F,
        settle: S,
    ) -> Result<Arc<T>, TransportError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<T>, TransportError>>,
        S: FnOnce(&PoolKey) -> PoolKey,
    {
        if let Some(conn) = self.lookup(&key) {
            return Ok(conn);
        }
        let conn = dial().await?;
        let key = settle(&key);
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
                tracing::debug!(
                    host = %key.params.host,
                    port = key.params.port,
                    "lost a dial race; adopting the pooled connection"
                );
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

/// Only logs: pooled connections stay with their release monitors.
impl<T: PooledConnection> Drop for ConnectionSupervisor<T> {
    fn drop(&mut self) {
        let pooled = lock_pool(&self.pool).len();
        tracing::info!(pooled, "connection supervisor shut down");
    }
}

/// How one hop authenticates (doc 04 §Authentication): the method, where its secrets
/// come from, the host-key check, and who answers challenges, plus the
/// keepalives its connection runs. Each hop of a jumped connection brings
/// its own.
pub struct HopAuth {
    pub method: AuthMethod,
    pub creds: CredentialManager,
    pub verifier: HostKeyVerifier,
    pub responder: Option<ChallengeResponder>,
    pub keepalive: Keepalive,
}

/// SSH keepalives (doc 04 §Keepalives): a probe after `interval` with
/// nothing from the server, and the session closes once more than `max`
/// go unanswered. `max` 0 keeps probing but never closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keepalive {
    pub interval: Option<Duration>,
    pub max: usize,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self {
            interval: Some(Duration::from_secs(15)),
            max: 3,
        }
    }
}

impl Keepalive {
    /// A session's stored `ssh_options`, field by field over the default.
    /// An interval of 0 turns probes off, as OpenSSH's `ServerAliveInterval`.
    pub fn from_options(options: Option<&SshOptions>) -> Self {
        let default = Self::default();
        let Some(options) = options else {
            return default;
        };
        Self {
            interval: match options.keepalive_interval {
                None => default.interval,
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs.into())),
            },
            max: options
                .keepalive_count
                .map_or(default.max, |count| count as usize),
        }
    }
}

/// The russh client config for one dial to `params`, direct or through a
/// jump host: the hop's keepalives, and the host-key algorithms in the
/// order its known_hosts entries ask for (doc 04 §Host key verification).
pub(crate) fn client_config(params: &ConnectParams, auth: &HopAuth) -> Arc<Config> {
    Arc::new(Config {
        keepalive_interval: auth.keepalive.interval,
        keepalive_max: auth.keepalive.max,
        preferred: Preferred {
            key: auth.verifier.key_preference(&params.host, params.port),
            ..Preferred::default()
        },
        ..Config::default()
    })
}

impl ConnectionSupervisor<SshConnection> {
    /// The pooled connection for `params`, dialing and authenticating with
    /// `auth` only when none is live.
    pub async fn get_or_connect(
        &self,
        params: &ConnectParams,
        auth: &HopAuth,
    ) -> Result<Arc<SshConnection>, TransportError> {
        let key = PoolKey::for_hop(params, auth);
        let dial = || async {
            let session = auth.creds.session_id();
            let handler =
                AppClientHandler::new(params.host.clone(), params.port, auth.verifier.clone());
            tracing::info!(host = %params.host, port = params.port, session = %session, "connecting");
            let handle = client::connect(
                client_config(params, auth),
                (params.host.as_str(), params.port),
                handler,
            )
            .await
            .map_err(|err| {
                let err = connect_error(&params.host, err);
                tracing::warn!(
                    host = %params.host,
                    port = params.port,
                    session = %session,
                    error = %err,
                    "connect failed"
                );
                err
            })?;
            let conn = authenticated(handle, params, auth).await?;
            tracing::info!(host = %params.host, port = params.port, session = %session, "connected");
            Ok(Arc::new(conn))
        };
        self.get_or_connect_settled(key, dial, |_| PoolKey::for_hop(params, auth))
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
async fn monitored_release<T: PooledConnection>(key: PoolKey, weak: Weak<T>, pool: Pool<T>) {
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
            tracing::info!(
                host = %key.params.host,
                port = key.params.port,
                "last consumer released; disconnecting"
            );
            conn.disconnect_now().await;
            return;
        }
    }
}

#[cfg(test)]
mod tests;
