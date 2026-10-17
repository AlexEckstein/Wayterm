//! The forward-status seam: how the Tunnels dialog learns a forward rule's
//! live state and how its enable toggle reaches the forward engine.
//!
//! The forward engine (`src/transport/forward.rs`) publishes one
//! [`ForwardEvent`] per rule transition on its own channel. This trait is
//! the dialog-side shape of that feed: a snapshot for the initial paint, a
//! broadcast subscription for live updates, and one control hook.
//! [`EngineStatusBridge`] serves it from a live engine, [`FakeStatusSource`]
//! from tests.
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

use crate::transport::forward::ForwardEngine;
pub use crate::transport::forward::{ForwardEvent, ForwardState, RuleId};

/// What the Tunnels dialog needs from a forward engine. Object-safe and
/// `Send + Sync` so the dialog can hold `Arc<dyn ForwardStatusSource>`
/// while the engine drives it from another thread.
pub trait ForwardStatusSource: Send + Sync {
    /// The current state of every rule, in rule order. Rules without an
    /// entry render as [`ForwardState::Stopped`].
    fn snapshot(&self) -> Vec<ForwardEvent>;
    /// Start (`true`) or stop (`false`) one rule's listener. Queued, never
    /// blocking; the resulting state arrives as a [`ForwardEvent`] update.
    fn set_rule_enabled(&self, rule_id: RuleId, enabled: bool);
    /// Send every future [`ForwardEvent`] to `sender` as well. The dialog
    /// calls this once with its drain's sender; [`EngineStatusBridge`]
    /// forwards engine events into all registered senders.
    fn subscribe(&self, sender: UnboundedSender<ForwardEvent>);
}

/// An in-process [`ForwardStatusSource`] for tests and for the dialog's
/// behavior proof: drive it with [`Self::emit`] from any thread, read the
/// toggle requests back with [`Self::toggles`].
pub struct FakeStatusSource {
    inner: Mutex<FakeInner>,
}

struct FakeInner {
    states: Vec<ForwardState>,
    senders: Vec<UnboundedSender<ForwardEvent>>,
    toggles: Vec<(RuleId, bool)>,
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

    /// Publish `event`: the snapshot moves and every subscriber is told.
    /// Senders whose receiver is gone are dropped.
    pub fn emit(&self, event: ForwardEvent) {
        let mut inner = self.inner.lock().expect("fake source lock");
        if inner.states.len() <= event.rule_id {
            inner
                .states
                .resize(event.rule_id + 1, ForwardState::Stopped);
        }
        inner.states[event.rule_id] = event.state.clone();
        inner
            .senders
            .retain(|sender| sender.send(event.clone()).is_ok());
    }

    /// Every `(rule_id, enabled)` the dialog requested, in order.
    pub fn toggles(&self) -> Vec<(RuleId, bool)> {
        self.inner.lock().expect("fake source lock").toggles.clone()
    }
}

impl ForwardStatusSource for FakeStatusSource {
    fn snapshot(&self) -> Vec<ForwardEvent> {
        self.inner
            .lock()
            .expect("fake source lock")
            .states
            .iter()
            .enumerate()
            .map(|(rule_id, state)| ForwardEvent {
                rule_id,
                state: state.clone(),
            })
            .collect()
    }

    fn set_rule_enabled(&self, rule_id: RuleId, enabled: bool) {
        self.inner
            .lock()
            .expect("fake source lock")
            .toggles
            .push((rule_id, enabled));
        let state = if enabled {
            ForwardState::Listening
        } else {
            ForwardState::Stopped
        };
        self.emit(ForwardEvent { rule_id, state });
    }

    fn subscribe(&self, sender: UnboundedSender<ForwardEvent>) {
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
/// awaits at all — fan-out uses `send` on unbounded channels, and the
/// engine's controls never block).
pub struct EngineStatusBridge {
    engine: ForwardEngine,
    states: Mutex<HashMap<RuleId, ForwardState>>,
    senders: Mutex<Vec<UnboundedSender<ForwardEvent>>>,
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
        self.states
            .lock()
            .expect("bridge states lock")
            .insert(event.rule_id, event.state.clone());
        self.senders
            .lock()
            .expect("bridge senders lock")
            .retain(|sender| sender.send(event.clone()).is_ok());
    }
}

impl ForwardStatusSource for EngineStatusBridge {
    fn snapshot(&self) -> Vec<ForwardEvent> {
        // Rules the engine never reported have no entry: per the trait
        // docs they render as `Stopped` at the dialog layer.
        let mut snapshot: Vec<ForwardEvent> = self
            .states
            .lock()
            .expect("bridge states lock")
            .iter()
            .map(|(&rule_id, state)| ForwardEvent {
                rule_id,
                state: state.clone(),
            })
            .collect();
        snapshot.sort_by_key(|event| event.rule_id);
        snapshot
    }

    fn set_rule_enabled(&self, rule_id: RuleId, enabled: bool) {
        // Never blocks; the resulting state arrives back through the event
        // drain.
        self.engine.set_enabled(rule_id, enabled);
    }

    fn subscribe(&self, sender: UnboundedSender<ForwardEvent>) {
        self.senders
            .lock()
            .expect("bridge senders lock")
            .push(sender);
    }
}
