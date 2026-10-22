//! Reconnect policy and driver (doc 03 §Lifecycle),
//! display-free so the whole retry loop is testable against paused time.
//! The GTK half (prompt dialog, window actions) is `ui::tab_resilience`.
//!
//! The backoff delay applies before every attempt, the first included, so
//! attempts land at 1s, 2s, 5s, 10s, then every 60s. Every connect error is
//! retried except [`TransportError::Cancelled`], which ends the run.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::transport::types::{TransportError, TransportEvent, TransportState};

/// The backoff rungs before the cap.
pub const BACKOFF_SCHEDULE: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// Every attempt past the schedule waits this long.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Delay before attempt `attempt` (1-based; 0 is treated as 1).
///
/// ```
/// use std::time::Duration;
/// use wayterm::transport::reconnect::next_delay;
///
/// assert_eq!(next_delay(1), Duration::from_secs(1));
/// assert_eq!(next_delay(4), Duration::from_secs(10));
/// assert_eq!(next_delay(900), Duration::from_secs(60));
/// ```
pub fn next_delay(attempt: u32) -> Duration {
    let index = attempt.saturating_sub(1) as usize;
    BACKOFF_SCHEDULE.get(index).copied().unwrap_or(MAX_BACKOFF)
}

/// What a dropped connection does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectMode {
    /// Ask before every attempt (`ReconnectPolicy::default`).
    Prompt,
    /// Retry on the backoff curve without asking.
    Auto,
}

/// Retry policy for one tab; `max_attempts: None` retries while the tab is
/// open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Prompt or automatic.
    pub mode: ReconnectMode,
    /// `None` for unlimited.
    pub max_attempts: Option<u32>,
}

impl ReconnectPolicy {
    /// A policy in `mode`, unlimited attempts.
    pub const fn new(mode: ReconnectMode) -> Self {
        Self {
            mode,
            max_attempts: None,
        }
    }

    /// Ask before each attempt.
    pub const fn prompt() -> Self {
        Self::new(ReconnectMode::Prompt)
    }

    /// Retry without asking.
    pub const fn auto() -> Self {
        Self::new(ReconnectMode::Auto)
    }

    /// Auto when the `reconnect_on_drop` setting is on, prompt otherwise.
    pub const fn for_setting(reconnect_on_drop: bool) -> Self {
        if reconnect_on_drop {
            Self::auto()
        } else {
            Self::prompt()
        }
    }

    /// Cap the run at `max_attempts`.
    pub const fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = Some(max_attempts);
        self
    }

    /// May attempt number `attempt` (1-based) run?
    pub fn allows_attempt(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::prompt()
    }
}

/// Progress of a [`ReconnectDriver`] run, for the UI and the tests.
#[derive(Clone, Debug, PartialEq)]
pub enum ReconnectEvent {
    /// Waiting `delay` before attempt `attempt`.
    BackingOff { attempt: u32, delay: Duration },
    /// Asking whether to try attempt `attempt`.
    Prompting { attempt: u32 },
    /// Attempt `attempt` is connecting.
    Attempting { attempt: u32 },
    /// Attempt `attempt` failed with `error`.
    AttemptFailed { attempt: u32, error: String },
    /// The attempt budget ran out.
    Exhausted { attempts: u32, error: String },
    /// Attempt `attempts` connected.
    Connected { attempts: u32 },
    /// The tab closed or a prompt was dismissed.
    Cancelled,
    /// The user said no to attempt `attempt`.
    Declined { attempt: u32 },
}

/// How a [`ReconnectDriver`] run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectOutcome {
    /// Connected after `attempts` attempts.
    Connected { attempts: u32 },
    /// The tab closed or a prompt was dismissed.
    Cancelled,
    /// The user said no to attempt `attempt`.
    Declined { attempt: u32 },
    /// The attempt budget ran out; `error` is the last failure.
    Exhausted { attempts: u32, error: String },
}

impl ReconnectOutcome {
    /// Whether the run ended connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// One connect attempt.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send>>;

/// One fresh connect through the supervisor, so tmux can reattach.
pub type ConnectFn = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

/// The user's answer to a reconnect prompt.
pub type ConfirmFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Prompt-mode responder: asks about attempt `n`, resolves to "reconnect?".
pub type ConfirmFn = Arc<dyn Fn(u32) -> ConfirmFuture + Send + Sync>;

/// Drives the reconnect state machine for one tab.
#[derive(Clone, Debug)]
pub struct ReconnectDriver {
    /// Mode and attempt budget.
    policy: ReconnectPolicy,
    /// Shared with the tab: closing it stops the loop.
    cancel: CancellationToken,
}

impl ReconnectDriver {
    /// A driver with its own cancel token.
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self::with_cancel_token(policy, CancellationToken::new())
    }

    /// Share the tab's token, so closing the tab also stops the loop.
    pub fn with_cancel_token(policy: ReconnectPolicy, cancel: CancellationToken) -> Self {
        Self { policy, cancel }
    }

    /// The token that stops the run.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stop the run before its next step.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether the run was stopped.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The retry policy.
    pub fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// Retry until connected, cancelled, declined, or out of attempts.
    ///
    /// `confirm` is only consulted in prompt mode, where a missing responder
    /// declines rather than hanging. `state_tx`, when given, receives the
    /// status-bar transitions: `Reconnecting { attempt }` before each
    /// attempt, then `Connected`, or `Disconnected` when the run gives up.
    pub async fn run(
        &self,
        connect: ConnectFn,
        events: &mpsc::UnboundedSender<ReconnectEvent>,
        confirm: Option<ConfirmFn>,
        state_tx: Option<&mpsc::UnboundedSender<TransportEvent>>,
    ) -> ReconnectOutcome {
        let emit = |event| {
            let _ = events.send(event);
        };
        let set_state = |state| {
            if let Some(tx) = state_tx {
                let _ = tx.send(TransportEvent::StateChanged(state));
            }
        };
        let cancelled = || {
            emit(ReconnectEvent::Cancelled);
            ReconnectOutcome::Cancelled
        };

        let mut attempt = 1;
        let mut last_error = String::from("attempt budget exhausted");
        loop {
            if self.is_cancelled() {
                return cancelled();
            }
            if !self.policy.allows_attempt(attempt) {
                let attempts = attempt - 1;
                emit(ReconnectEvent::Exhausted {
                    attempts,
                    error: last_error.clone(),
                });
                set_state(TransportState::Disconnected);
                return ReconnectOutcome::Exhausted {
                    attempts,
                    error: last_error,
                };
            }

            let delay = next_delay(attempt);
            emit(ReconnectEvent::BackingOff { attempt, delay });
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return cancelled(),
                () = tokio::time::sleep(delay) => {}
            }

            if self.policy.mode == ReconnectMode::Prompt {
                emit(ReconnectEvent::Prompting { attempt });
                let confirmed = match &confirm {
                    Some(confirm) => confirm(attempt).await,
                    None => false,
                };
                if self.is_cancelled() {
                    return cancelled();
                }
                if !confirmed {
                    emit(ReconnectEvent::Declined { attempt });
                    set_state(TransportState::Disconnected);
                    return ReconnectOutcome::Declined { attempt };
                }
            }

            emit(ReconnectEvent::Attempting { attempt });
            set_state(TransportState::Reconnecting { attempt });
            match connect().await {
                Ok(()) => {
                    emit(ReconnectEvent::Connected { attempts: attempt });
                    set_state(TransportState::Connected);
                    return ReconnectOutcome::Connected { attempts: attempt };
                }
                Err(TransportError::Cancelled) => {
                    set_state(TransportState::Disconnected);
                    return cancelled();
                }
                Err(err) => {
                    last_error = err.to_string();
                    emit(ReconnectEvent::AttemptFailed {
                        attempt,
                        error: last_error.clone(),
                    });
                }
            }
            attempt = attempt.saturating_add(1);
        }
    }

    /// [`run`](Self::run) on a tokio task.
    pub fn spawn(
        self,
        connect: ConnectFn,
        events: mpsc::UnboundedSender<ReconnectEvent>,
        confirm: Option<ConfirmFn>,
        state_tx: Option<mpsc::UnboundedSender<TransportEvent>>,
    ) -> tokio::task::JoinHandle<ReconnectOutcome> {
        tokio::spawn(async move { self.run(connect, &events, confirm, state_tx.as_ref()).await })
    }
}

/// Could a process still be running on the other end?
pub fn is_live(state: &TransportState) -> bool {
    !matches!(
        state,
        TransportState::Disconnected | TransportState::Error(_)
    )
}

/// Ask before closing a tab whose transport is live (setting `confirm_close`).
pub fn should_confirm_close(state: &TransportState, confirm_close_setting: bool) -> bool {
    confirm_close_setting && is_live(state)
}
