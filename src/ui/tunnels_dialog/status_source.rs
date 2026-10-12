//! The forward-status seam: how the Tunnels dialog learns a forward rule's
//! live state and how its enable toggle reaches the forward engine.
//!
//! The forward engine (`src/transport/forward.rs`) publishes per-rule
//! status through its own channel of `ForwardEvent`s. This trait is the
//! dialog-side shape of that feed: a snapshot for the initial paint, a
//! broadcast subscription for live updates, and one control hook.
//! [`EngineStatusBridge`] joins the two by wrapping the engine's event
//! receiver in this trait; the dialog never imports transport types.
//!
//! GTK discipline follows `tab.rs`: status travels on a
//! `tokio::sync::mpsc` unbounded channel whose receiver is drained by a
//! `MainContext::spawn_local` future (`while let Some … = rx.recv().await`),
//! and `send` never blocks — so there is no `blocking_recv` on the UI path
//! and no lock held across `.await` (the dialog keeps no async fns at all).
//! `set_rule_enabled` queues the request and returns.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

use crate::transport::forward::{ForwardEngine, ForwardEvent, ForwardState as EngineState};

/// One rule's live state, by position in the session's `forward_rules`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardStatus {
    pub rule_index: usize,
    pub state: ForwardState,
}

/// The per-rule states the dialog renders (doc 11 §Port forwarding).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForwardState {
    /// The listener is up and idle.
    Listening,
    /// The listener is up with `conns` open channels.
    Active { conns: u32 },
    /// The listener failed; the engine keeps the last error for display.
    Failed { error: String },
    /// The rule is disabled or its listener was stopped.
    Stopped,
}

/// What the Tunnels dialog needs from a forward engine. Object-safe and
/// `Send + Sync` so the dialog can hold `Arc<dyn ForwardStatusSource>`
/// while the engine drives it from another thread.
pub trait ForwardStatusSource: Send + Sync {
    /// The current state of every rule, in rule order. Rules without an
    /// entry render as [`ForwardState::Stopped`].
    fn snapshot(&self) -> Vec<ForwardStatus>;
    /// Start (`true`) or stop (`false`) one rule's listener. Queued, never
    /// blocking; the resulting state arrives as a [`ForwardStatus`] update.
    fn set_rule_enabled(&self, rule_index: usize, enabled: bool);
    /// Send every future [`ForwardStatus`] to `sender` as well. The dialog
    /// calls this once with its drain's sender; [`EngineStatusBridge`]
    /// forwards engine events into all registered senders.
    fn subscribe(&self, sender: UnboundedSender<ForwardStatus>);
}

/// An in-process [`ForwardStatusSource`] for tests and for the dialog's
/// behavior proof: drive it with [`Self::emit`] from any thread, read the
/// toggle requests back with [`Self::toggles`].
pub struct FakeStatusSource {
    inner: Mutex<FakeInner>,
}

struct FakeInner {
    states: Vec<ForwardState>,
    senders: Vec<UnboundedSender<ForwardStatus>>,
    toggles: Vec<(usize, bool)>,
}

impl FakeStatusSource {
    /// A source with one state per rule, in rule order.
    pub fn new(states: Vec<ForwardState>) -> Self {
        Self {
            inner: Mutex::new(FakeInner {
                states,
                senders: Vec::new(),
                toggles: Vec::new(),
            }),
        }
    }

    /// Publish `status`: the snapshot moves and every subscriber is told.
    /// Senders whose receiver is gone are dropped.
    pub fn emit(&self, status: ForwardStatus) {
        let mut inner = self.inner.lock().expect("fake source lock");
        if inner.states.len() <= status.rule_index {
            inner
                .states
                .resize(status.rule_index + 1, ForwardState::Stopped);
        }
        inner.states[status.rule_index] = status.state.clone();
        inner
            .senders
            .retain(|sender| sender.send(status.clone()).is_ok());
    }

    /// Every `(rule_index, enabled)` the dialog requested, in order.
    pub fn toggles(&self) -> Vec<(usize, bool)> {
        self.inner.lock().expect("fake source lock").toggles.clone()
    }
}

impl ForwardStatusSource for FakeStatusSource {
    fn snapshot(&self) -> Vec<ForwardStatus> {
        self.inner
            .lock()
            .expect("fake source lock")
            .states
            .iter()
            .enumerate()
            .map(|(rule_index, state)| ForwardStatus {
                rule_index,
                state: state.clone(),
            })
            .collect()
    }

    fn set_rule_enabled(&self, rule_index: usize, enabled: bool) {
        let mut inner = self.inner.lock().expect("fake source lock");
        inner.toggles.push((rule_index, enabled));
        let state = if enabled {
            ForwardState::Listening
        } else {
            ForwardState::Stopped
        };
        if inner.states.len() <= rule_index {
            inner.states.resize(rule_index + 1, ForwardState::Stopped);
        }
        inner.states[rule_index] = state.clone();
        let status = ForwardStatus { rule_index, state };
        inner
            .senders
            .retain(|sender| sender.send(status.clone()).is_ok());
    }

    fn subscribe(&self, sender: UnboundedSender<ForwardStatus>) {
        self.inner
            .lock()
            .expect("fake source lock")
            .senders
            .push(sender);
    }
}

/// The bridge from the transport engine
/// (`crate::transport::forward`) into [`ForwardStatusSource`].
///
/// The connect flow builds one per connected tab: it holds the tab's
/// [`ForwardEngine`] handle plus a snapshot cache fed by a tokio task
/// draining the engine's event channel through [`Self::handle_event`].
/// The dialog only ever sees this trait — it never imports transport
/// types. `Send + Sync` via the same lock discipline as
/// [`FakeStatusSource`]: no lock is held across `.await` (nothing here
/// awaits at all — fan-out uses `try_send`, control uses
/// `tokio::spawn`).
pub struct EngineStatusBridge {
    engine: ForwardEngine,
    states: Mutex<HashMap<usize, ForwardState>>,
    senders: Mutex<Vec<UnboundedSender<ForwardStatus>>>,
}

/// The transport's per-rule state in the dialog's shape:
/// `LastFailed` is the engine's name for what the dialog renders as
/// `Failed`; every other variant (and the transport `rule_id` → dialog
/// `rule_index` position) lines up one-to-one.
fn map_engine_state(state: &EngineState) -> ForwardState {
    match state {
        EngineState::Listening => ForwardState::Listening,
        EngineState::Active { conns } => ForwardState::Active { conns: *conns },
        EngineState::LastFailed { error } => ForwardState::Failed {
            error: error.clone(),
        },
        EngineState::Stopped => ForwardState::Stopped,
    }
}

impl EngineStatusBridge {
    /// Wrap a live engine handle. The caller spawns the drain task that
    /// feeds [`Self::handle_event`]; see `ui::tab::arm_forward_engine`.
    pub fn new(engine: ForwardEngine) -> Self {
        Self {
            engine,
            states: Mutex::new(HashMap::new()),
            senders: Mutex::new(Vec::new()),
        }
    }

    /// The wrapped engine, for lifecycle calls (`stop` on tab teardown).
    /// Cheap to clone: every clone drives the same listeners.
    pub fn engine(&self) -> ForwardEngine {
        self.engine.clone()
    }

    /// Fold one engine event into the snapshot and fan it out to every
    /// subscriber. Senders whose receiver is gone are pruned, like
    /// [`FakeStatusSource::emit`]. Never blocks.
    pub fn handle_event(&self, event: ForwardEvent) {
        let status = ForwardStatus {
            rule_index: event.rule_id,
            state: map_engine_state(&event.state),
        };
        self.states
            .lock()
            .expect("bridge states lock")
            .insert(status.rule_index, status.state.clone());
        self.senders
            .lock()
            .expect("bridge senders lock")
            .retain(|sender| sender.send(status.clone()).is_ok());
    }
}

impl ForwardStatusSource for EngineStatusBridge {
    fn snapshot(&self) -> Vec<ForwardStatus> {
        // Rules the engine never reported have no entry: per the trait
        // docs they render as `Stopped` at the dialog layer.
        let mut snapshot: Vec<ForwardStatus> = self
            .states
            .lock()
            .expect("bridge states lock")
            .iter()
            .map(|(&rule_index, state)| ForwardStatus {
                rule_index,
                state: state.clone(),
            })
            .collect();
        snapshot.sort_by_key(|status| status.rule_index);
        snapshot
    }

    fn set_rule_enabled(&self, rule_index: usize, enabled: bool) {
        // Queued on the runtime, never awaited on the GTK thread; the
        // resulting state arrives back through the event drain.
        let engine = self.engine.clone();
        tokio::spawn(async move {
            engine.set_enabled(rule_index, enabled).await;
        });
    }

    fn subscribe(&self, sender: UnboundedSender<ForwardStatus>) {
        self.senders
            .lock()
            .expect("bridge senders lock")
            .push(sender);
    }
}
