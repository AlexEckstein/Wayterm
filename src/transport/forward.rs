//! Local port-forward engine (doc 04 §Port forwarding).
//!
//! The engine is owned by THE CALLER (the connect flow / reconnect driver
//! — a later wiring step), not by the connection: build one
//! [`ForwardEngine`] per live [`SshConnection`] with
//! [`ForwardEngine::new`], [`start`](ForwardEngine::start) it with the
//! session's rules, and on reconnect [`stop`](ForwardEngine::stop) the old
//! engine and start a fresh one on the new connection `Arc`. Listeners die
//! with the connection; they never migrate across it.
//!
//! Per enabled [`ForwardKind::Local`](crate::store::model::ForwardKind)
//! rule the engine binds one `TcpListener`; each accepted connection opens
//! one direct-tcpip channel to the rule's remote and pipes both directions
//! with `copy_bidirectional`. A refused remote closes only the accepted
//! socket (reported as [`ForwardState::LastFailed`]); the listener keeps
//! serving. `Remote`/`Dynamic` rules are v2 (doc 04): they are skipped with
//! one report line each and a [`ForwardState::Stopped`] event so the status
//! snapshot stays honest.
//!
//! Testability: the accept/copy/shutdown core runs against a
//! `ChannelOpener` closure, so unit tests inject an in-process fake while
//! [`ForwardEngine::new`] wires the real `SshConnection` path (covered by
//! `tests/forward_sshd.rs`). `SshConnection` itself stays concrete — no
//! new generics on any protected seam.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::store::model::{ForwardKind, ForwardRule};
use crate::transport::connection::SshConnection;
use crate::transport::types::TransportError;

/// Index into the rules slice handed to [`ForwardEngine::start`]. Rule
/// identity is positional: reordering rules across a restart reassigns ids,
/// so the Tunnels dialog must treat events as applying to the latest
/// `start()` call's ordering.
pub type RuleId = usize;

/// Status snapshot source for the Tunnels dialog (one event per
/// transition; `Active` carries the live count, not per-connection spam).
#[derive(Clone, Debug, PartialEq)]
pub enum ForwardState {
    Listening,
    Active { conns: u32 },
    LastFailed { error: String },
    Stopped,
}

/// One status transition for one rule.
#[derive(Clone, Debug, PartialEq)]
pub struct ForwardEvent {
    pub rule_id: RuleId,
    pub state: ForwardState,
}

/// The far end of a forwarded connection: anything byte-duplex. The
/// production opener boxes a russh channel stream; tests box a duplex to
/// an in-process fake. (One trait because a `dyn` object can only name one
/// non-auto trait.)
pub(crate) trait DuplexStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> DuplexStream for T {}
/// Pinned so the object is `Unpin` and `copy_bidirectional`-compatible.
pub(crate) type BoxStream = Pin<Box<dyn DuplexStream>>;
pub(crate) type OpenerFuture =
    Pin<Box<dyn Future<Output = Result<BoxStream, TransportError>> + Send>>;
/// `(remote_host, remote_port) -> channel stream`. `Send + Sync` so one
/// opener serves every listener and connection task.
pub(crate) type ChannelOpener = Arc<dyn Fn(String, u16) -> OpenerFuture + Send + Sync>;

/// Caller-owned engine handle: cheap to clone, all clones drive the same
/// listeners. `start` replaces any running rules; `stop` is idempotent.
/// Its methods only spawn and abort tasks, so they never block, but they
/// need a tokio runtime.
#[derive(Clone)]
pub struct ForwardEngine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    opener: ChannelOpener,
    events: mpsc::Sender<ForwardEvent>,
    rules: Mutex<Vec<ForwardRule>>,
    tasks: Mutex<HashMap<RuleId, RuleTasks>>,
}

impl EngineInner {
    /// Best-effort, never blocking: events are a status snapshot, and a
    /// full dialog channel must not stall forwarding or shutdown.
    fn emit(&self, rule_id: RuleId, state: ForwardState) {
        let _ = self.events.try_send(ForwardEvent { rule_id, state });
    }
}

/// One serving rule: its listener and the connections it accepted.
struct RuleTasks {
    listener: JoinHandle<()>,
    conns: Vec<JoinHandle<()>>,
}

impl RuleTasks {
    fn abort(self) {
        self.listener.abort();
        self.conns.iter().for_each(JoinHandle::abort);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock still guards a consistent map.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ForwardEngine {
    /// Production constructor: channels open on `connection` via
    /// direct-tcpip. The engine holds the `Arc`; dropping the engine does
    /// NOT stop forwarding — call [`stop`](ForwardEngine::stop) so
    /// listeners close deterministically (aborted tasks die with it, but
    /// only `stop` emits the terminal `Stopped` snapshot).
    pub fn new(connection: Arc<SshConnection>, events: mpsc::Sender<ForwardEvent>) -> Self {
        let opener: ChannelOpener = Arc::new(move |host: String, port: u16| {
            let conn = Arc::clone(&connection);
            Box::pin(async move {
                let channel = conn.open_direct_tcpip(&host, port).await?;
                Ok(Box::pin(channel.into_stream()) as BoxStream)
            }) as OpenerFuture
        });
        Self::with_opener(opener, events)
    }

    pub(crate) fn with_opener(opener: ChannelOpener, events: mpsc::Sender<ForwardEvent>) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                opener,
                events,
                rules: Mutex::new(Vec::new()),
                tasks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Apply a rule set: stops anything running, then serves every enabled
    /// `Local` rule. Disabled rules and skipped (`Remote`/`Dynamic`) kinds
    /// each report one [`ForwardState::Stopped`] so the dialog can render
    /// the full set.
    pub fn start(&self, rules: Vec<ForwardRule>) {
        // Quietly: every rule reports its fresh state right after.
        self.stop_all(false);
        lock(&self.inner.rules).clone_from(&rules);
        for (id, rule) in rules.iter().enumerate() {
            self.start_rule(id, rule);
        }
    }

    /// Live enable toggle for the Tunnels dialog: starts or stops one
    /// rule's listener without touching the others. Unknown ids are
    /// ignored; disabling aborts that rule's live connections.
    pub fn set_enabled(&self, rule_id: RuleId, enabled: bool) {
        let rule = {
            let mut rules = lock(&self.inner.rules);
            let Some(rule) = rules.get_mut(rule_id) else {
                return;
            };
            rule.enabled = enabled;
            rule.clone()
        };
        if enabled {
            self.start_rule(rule_id, &rule);
        } else {
            self.stop_rule(rule_id, true);
        }
    }

    /// Close all listeners and abort all per-connection tasks, emitting
    /// `Stopped` for every rule that was running. Idempotent. After `stop`,
    /// either drop the engine or `start` it again (on a fresh connection
    /// `Arc`, build a fresh engine instead — see the module docs).
    pub fn stop(&self) {
        self.stop_all(true);
    }

    fn start_rule(&self, id: RuleId, rule: &ForwardRule) {
        if !rule.enabled {
            return self.inner.emit(id, ForwardState::Stopped);
        }
        if !matches!(rule.kind, ForwardKind::Local) {
            eprintln!(
                "Wayterm: forward rule {id}: {:?} forwards are v2 (doc 04) — skipped",
                rule.kind,
            );
            return self.inner.emit(id, ForwardState::Stopped);
        }
        // Checked and claimed under one lock: two starts of a rule never
        // both bind.
        let mut tasks = lock(&self.inner.tasks);
        if tasks.contains_key(&id) {
            return;
        }
        let listener = tokio::spawn(serve_rule(Arc::clone(&self.inner), id, rule.clone()));
        tasks.insert(
            id,
            RuleTasks {
                listener,
                conns: Vec::new(),
            },
        );
    }

    /// Stop rule `id` if it is serving, `announce`-ing `Stopped`.
    fn stop_rule(&self, id: RuleId, announce: bool) {
        let tasks = lock(&self.inner.tasks).remove(&id);
        if let Some(tasks) = tasks {
            tasks.abort();
            if announce {
                self.inner.emit(id, ForwardState::Stopped);
            }
        }
    }

    fn stop_all(&self, announce: bool) {
        let ids: Vec<RuleId> = lock(&self.inner.tasks).keys().copied().collect();
        for id in ids {
            self.stop_rule(id, announce);
        }
    }
}

/// One rule's listener: bind, then accept forever (abortion by `stop` /
/// `set_enabled(false)` closes the listener). A dead underlying
/// connection surfaces here as `LastFailed` on the next accepted socket —
/// the listener itself is a local socket and stays up; the reconnect
/// driver replaces the whole engine on a fresh connection `Arc`.
async fn serve_rule(inner: Arc<EngineInner>, id: RuleId, rule: ForwardRule) {
    // An empty bind means loopback, never 0.0.0.0: doc 09's example binds
    // 127.0.0.1, and a tunnel that silently listens on every interface
    // would be a safety hole, not a convenience.
    let bind_addr = match rule.bind_addr.trim() {
        "" => "127.0.0.1",
        addr => addr,
    };
    let listener = match TcpListener::bind((bind_addr, rule.bind_port)).await {
        Ok(listener) => listener,
        Err(err) => {
            let error = format!("bind {bind_addr}:{} failed: {err}", rule.bind_port);
            return inner.emit(id, ForwardState::LastFailed { error });
        }
    };
    inner.emit(id, ForwardState::Listening);
    let active = Arc::new(AtomicU32::new(0));
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let conn = tokio::spawn(handle_one(
            Arc::clone(&inner),
            id,
            rule.clone(),
            Arc::clone(&active),
            socket,
        ));
        let mut tasks = lock(&inner.tasks);
        if let Some(entry) = tasks.get_mut(&id) {
            entry.conns.retain(|h| !h.is_finished());
            entry.conns.push(conn);
        }
        // Else this rule was stopped mid-accept: the spawned task still
        // serves its one connection, then ends with the listener gone.
    }
}

/// One accepted connection: open the channel, pipe both directions. A
/// failed open closes only this socket (clean EOF for the local client)
/// and the listener keeps serving.
async fn handle_one(
    inner: Arc<EngineInner>,
    id: RuleId,
    rule: ForwardRule,
    active: Arc<AtomicU32>,
    mut socket: TcpStream,
) {
    let opened = (inner.opener)(rule.remote_host.clone(), rule.remote_port).await;
    let mut remote = match opened {
        Ok(stream) => stream,
        Err(err) => {
            let error = format!(
                "channel to {}:{} failed: {err}",
                rule.remote_host, rule.remote_port
            );
            return inner.emit(id, ForwardState::LastFailed { error });
        }
    };
    let conns = active.fetch_add(1, Ordering::SeqCst) + 1;
    inner.emit(id, ForwardState::Active { conns });
    let _ = tokio::io::copy_bidirectional(&mut socket, &mut remote).await;
    let conns = active.fetch_sub(1, Ordering::SeqCst) - 1;
    inner.emit(id, ForwardState::Active { conns });
}

#[cfg(test)]
mod tests;
