//! Reconnect policy and driver — the display-free half of doc 04
//! §"Lifecycle & Cancellation".
//!
//! Doc 04 specifies the whole state machine here:
//!
//! * **Connection drop:** the reader task emits `ConnectionLost`; the UI
//!   shows the reconnect prompt "(or auto-reconnect if enabled per
//!   session)".
//! * **Backoff for auto-reconnect:** `1s, 2s, 5s, 10s`, capped at `60s`,
//!   **unlimited attempts while the tab is open**; each attempt updates
//!   `TransportState::Reconnecting { attempt }`.
//! * **Cancellation of dialogs:** host-key and auth prompts are
//!   cancellable; cancellation aborts the connect with
//!   `TransportError::Cancelled`.
//!
//! Nothing here touches GTK: [`ReconnectDriver`] takes a connect closure,
//! a [`CancellationToken`], and an event sink, so the whole retry loop is
//! unit-testable with a mock connect closure and paused tokio time. The
//! GTK glue (the prompt dialog, the two window actions, the
//! confirm-before-close dialog) lives in `crate::ui::tab_resilience`.
//!
//! # Interpretations (reported, not silent)
//!
//! * The doc lists the curve without saying whether the *first* attempt
//!   waits. [`next_delay`] is applied before **every** attempt, including
//!   attempt 1, so the observed schedule is exactly the documented
//!   `1s, 2s, 5s, 10s, 60s, 60s, …`.
//! * Doc 04 does not classify retryable vs terminal connect errors, so
//!   this module retries every error except
//!   [`TransportError::Cancelled`] — which doc 04 *does* name as the
//!   cancellation signal (it aborts the connect immediately, no backoff).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::transport::types::{TransportError, TransportEvent, TransportState};

/// Doc 04's backoff curve: 1s, 2s, 5s, 10s, capped at 60s.
///
/// The first four rungs are the documented delays; the fifth rung *is*
/// the cap, so every attempt from 5 onwards waits 60s.
pub const BACKOFF_SCHEDULE: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(60),
];

/// The ceiling doc 04 caps the curve at (`max_backoff`).
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Delay to wait before attempt `attempt` (1-based).
///
/// ```
/// use wayterm::transport::reconnect::next_delay;
/// use std::time::Duration;
///
/// assert_eq!(next_delay(1), Duration::from_secs(1));
/// assert_eq!(next_delay(4), Duration::from_secs(10));
/// assert_eq!(next_delay(5), Duration::from_secs(60));
/// assert_eq!(next_delay(900), Duration::from_secs(60));
/// ```
pub fn next_delay(attempt: u32) -> Duration {
    // Saturating: `attempt` beyond the curve is the cap, never an index
    // panic and never a wrap-around back to the fast rungs.
    let idx = usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX);
    BACKOFF_SCHEDULE
        .get(idx)
        .copied()
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

/// What a dropped connection does next: ask the user, or retry silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectMode {
    /// Doc 04's default: the UI shows the reconnect prompt and the
    /// connect only runs after the user says yes.
    Prompt,
    /// "auto-reconnect if enabled per session" (doc 04 §Lifecycle) — the
    /// driver loops through the backoff curve on its own.
    Auto,
}

/// Retry policy for one tab.
///
/// `max_attempts: None` is the documented default: "unlimited attempts
/// while the tab is open". The bounded form exists for tests and for a
/// future "give up after N" preference; nothing in doc 04 sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub mode: ReconnectMode,
    pub max_attempts: Option<u32>,
}

impl ReconnectPolicy {
    /// A policy with the documented unlimited-attempt budget.
    pub const fn new(mode: ReconnectMode) -> Self {
        Self {
            mode,
            max_attempts: None,
        }
    }

    /// Prompt mode, unlimited attempts (doc 04's default path).
    pub const fn prompt() -> Self {
        Self::new(ReconnectMode::Prompt)
    }

    /// Auto mode, unlimited attempts.
    pub const fn auto() -> Self {
        Self::new(ReconnectMode::Auto)
    }

    /// Cap the number of attempts (tests / future preference).
    pub const fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = Some(max_attempts);
        self
    }

    /// May attempt number `attempt` (1-based) still run?
    pub fn allows_attempt(&self, attempt: u32) -> bool {
        match self.max_attempts {
            None => true,
            Some(max) => attempt <= max,
        }
    }
}

impl Default for ReconnectPolicy {
    /// Doc 04's default: prompt.
    fn default() -> Self {
        Self::prompt()
    }
}

/// Progress events from a [`ReconnectDriver`] run.
///
/// The UI (status bar, prompt) consumes these; tests assert on them.
#[derive(Clone, Debug, PartialEq)]
pub enum ReconnectEvent {
    /// Waiting `delay` before attempt `attempt` (doc 04's backoff curve).
    BackingOff { attempt: u32, delay: Duration },
    /// Prompt mode: waiting on the UI's yes/no answer for `attempt`.
    Prompting { attempt: u32 },
    /// About to run the connect closure for `attempt`.
    Attempting { attempt: u32 },
    /// Attempt `attempt` failed; another attempt may follow.
    AttemptFailed { attempt: u32, error: String },
    /// Attempt `attempt` failed and no further attempt is allowed.
    Exhausted { attempts: u32, error: String },
    /// Connected again after `attempts` attempts.
    Connected { attempts: u32 },
    /// Cancelled (tab close, disconnect action, or the connect itself
    /// returning `TransportError::Cancelled`).
    Cancelled,
    /// Prompt mode: the user declined this reconnect.
    Declined { attempt: u32 },
}

/// How a [`ReconnectDriver`] run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectOutcome {
    /// The connect closure succeeded on attempt `attempts` (1-based).
    Connected { attempts: u32 },
    /// The cancellation token fired (or a connect returned `Cancelled`).
    Cancelled,
    /// Prompt mode, user said no.
    Declined { attempt: u32 },
    /// The attempt budget ran out; the last error is reported for the UI.
    Exhausted { attempts: u32, error: String },
}

impl ReconnectOutcome {
    /// True only for [`ReconnectOutcome::Connected`].
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// The boxed future a connect closure returns.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send>>;

/// `connect` closure the driver calls once per attempt.
///
/// Doc 04: "Reconnect = fresh `connect()` through the supervisor". The
/// supervisor seam (P3-3) supplies this closure; until then a caller can
/// pass any equivalent (see `crate::ui::tab_resilience`).
pub type ConnectFn = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

/// The boxed future a confirm responder returns: `true` = reconnect.
pub type ConfirmFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Prompt-mode responder: shows the reconnect prompt for `attempt` and
/// resolves to the user's answer.
pub type ConfirmFn = Arc<dyn Fn(u32) -> ConfirmFuture + Send + Sync>;

/// Drives doc 04's reconnect state machine for one tab.
///
/// Cheap to build (no I/O in the constructor) and `Send`, so the UI can
/// hold it and spawn [`ReconnectDriver::run`] on tokio.
#[derive(Clone, Debug)]
pub struct ReconnectDriver {
    policy: ReconnectPolicy,
    cancel: CancellationToken,
}

impl ReconnectDriver {
    /// A driver with a fresh cancellation token.
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            cancel: CancellationToken::new(),
        }
    }

    /// Adopt an existing token (e.g. the tab's own, which `close()` also
    /// fires) so tab close cancels reconnect for free.
    pub fn with_cancel_token(policy: ReconnectPolicy, cancel: CancellationToken) -> Self {
        Self { policy, cancel }
    }

    /// The token that stops this driver (mid-backoff included).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stop the driver: the run returns [`ReconnectOutcome::Cancelled`].
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// True once the run has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The policy this driver enforces.
    pub fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// Run the retry loop until it connects, is cancelled, is declined, or
    /// exhausts [`ReconnectPolicy::max_attempts`].
    ///
    /// * `connect` runs once per attempt (doc 04: a *fresh* `connect()`
    ///   through the supervisor, so tmux can be reattached).
    /// * `events` receives every [`ReconnectEvent`].
    /// * `confirm` is prompt mode's yes/no responder. In auto mode it is
    ///   ignored; in prompt mode a `None` responder is treated as a
    ///   decline rather than an unbounded hang.
    /// * `state_tx`, when given, gets the doc 04 state transitions the
    ///   status bar already consumes: `Reconnecting { attempt }` before
    ///   each attempt, `Connected` on success, `Disconnected` when the run
    ///   ends without a connection.
    pub async fn run(
        &self,
        connect: ConnectFn,
        events: &mpsc::UnboundedSender<ReconnectEvent>,
        confirm: Option<ConfirmFn>,
        state_tx: Option<&mpsc::UnboundedSender<TransportEvent>>,
    ) -> ReconnectOutcome {
        let mut attempt: u32 = 1;

        loop {
            if self.cancelled_or_done(events) {
                return ReconnectOutcome::Cancelled;
            }
            if !self.policy.allows_attempt(attempt) {
                // Only reachable with a zero budget; the budget is
                // otherwise enforced after each failure, where the real
                // error is still known.
                emit(
                    events,
                    ReconnectEvent::Exhausted {
                        attempts: 0,
                        error: "attempt budget exhausted".into(),
                    },
                );
                emit_state(state_tx, TransportState::Disconnected);
                return ReconnectOutcome::Exhausted {
                    attempts: 0,
                    error: "attempt budget exhausted".into(),
                };
            }

            let delay = next_delay(attempt);
            emit(events, ReconnectEvent::BackingOff { attempt, delay });
            if self.sleep_cancellable(delay).await.is_err() {
                return ReconnectOutcome::Cancelled;
            }

            if self.policy.mode == ReconnectMode::Prompt {
                emit(events, ReconnectEvent::Prompting { attempt });
                let confirmed = match &confirm {
                    // A prompt-mode run with no responder cannot ask the
                    // user, so it declines instead of blocking forever.
                    None => false,
                    Some(confirm) => {
                        let answer = confirm(attempt).await;
                        self.cancelled_or_done(events);
                        answer
                    }
                };
                if !confirmed {
                    emit(events, ReconnectEvent::Declined { attempt });
                    emit_state(state_tx, TransportState::Disconnected);
                    return ReconnectOutcome::Declined { attempt };
                }
            }

            emit(events, ReconnectEvent::Attempting { attempt });
            emit_state(state_tx, TransportState::Reconnecting { attempt });

            match connect().await {
                Ok(()) => {
                    emit(events, ReconnectEvent::Connected { attempts: attempt });
                    emit_state(state_tx, TransportState::Connected);
                    return ReconnectOutcome::Connected { attempts: attempt };
                }
                Err(TransportError::Cancelled) => {
                    // Doc 04 §Lifecycle: cancelling a prompt aborts the
                    // connect with TransportError::Cancelled. Retry loops
                    // must not treat that as a transient network fault.
                    emit(events, ReconnectEvent::Cancelled);
                    emit_state(state_tx, TransportState::Disconnected);
                    return ReconnectOutcome::Cancelled;
                }
                Err(err) => {
                    let error = err.to_string();
                    emit(
                        events,
                        ReconnectEvent::AttemptFailed {
                            attempt,
                            error: error.clone(),
                        },
                    );
                    if !self.policy.allows_attempt(attempt.saturating_add(1)) {
                        emit(
                            events,
                            ReconnectEvent::Exhausted {
                                attempts: attempt,
                                error: error.clone(),
                            },
                        );
                        emit_state(state_tx, TransportState::Disconnected);
                        return ReconnectOutcome::Exhausted {
                            attempts: attempt,
                            error,
                        };
                    }
                }
            }

            attempt = attempt.saturating_add(1);
        }
    }

    /// Spawn [`ReconnectDriver::run`] on tokio; the join handle carries
    /// the [`ReconnectOutcome`].
    pub fn spawn(
        self,
        connect: ConnectFn,
        events: mpsc::UnboundedSender<ReconnectEvent>,
        confirm: Option<ConfirmFn>,
        state_tx: Option<mpsc::UnboundedSender<TransportEvent>>,
    ) -> tokio::task::JoinHandle<ReconnectOutcome> {
        tokio::spawn(async move { self.run(connect, &events, confirm, state_tx.as_ref()).await })
    }

    /// `Ok(())` when the wait completed, `Err(())` when the token fired
    /// first — so a cancel mid-backoff stops the loop *before* the attempt.
    async fn sleep_cancellable(&self, delay: Duration) -> Result<(), ()> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(()),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    }

    /// True when the token has fired (also draining the event sink so the
    /// `Cancelled` event is not lost on a full channel).
    fn cancelled_or_done(&self, events: &mpsc::UnboundedSender<ReconnectEvent>) -> bool {
        if self.cancel.is_cancelled() {
            emit(events, ReconnectEvent::Cancelled);
            true
        } else {
            false
        }
    }
}

fn emit(events: &mpsc::UnboundedSender<ReconnectEvent>, event: ReconnectEvent) {
    let _ = events.send(event);
}

fn emit_state(state_tx: Option<&mpsc::UnboundedSender<TransportEvent>>, state: TransportState) {
    if let Some(tx) = state_tx {
        let _ = tx.send(TransportEvent::StateChanged(state));
    }
}

/// Is this transport live — i.e. could a process still be running on the
/// other end? (doc 04 §Lifecycle: confirm-before-close "if processes
/// running".)
pub fn is_live(state: &TransportState) -> bool {
    matches!(
        state,
        TransportState::Connected
            | TransportState::Connecting
            | TransportState::VerifyingHostKey
            | TransportState::Authenticating
            | TransportState::Reconnecting { .. }
    )
}

/// Should closing this tab ask first?
///
/// Doc 04 §Lifecycle: "Confirm-before-close if processes running (setting
/// `confirm_close`)". Both halves are factored here so the policy is
/// testable without GTK; the dialog itself is in
/// `crate::ui::tab_resilience`.
pub fn should_confirm_close(state: &TransportState, confirm_close_setting: bool) -> bool {
    confirm_close_setting && is_live(state)
}

/// Parse the `confirm_close` settings row (doc 06: `'1'` / `'0'`).
///
/// Default is ON: doc 06 seeds `'1'`, and a missing row must not silently
/// disable the safety prompt.
pub fn confirm_close_enabled(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Parse the `reconnect_on_drop` settings row into a [`ReconnectPolicy`].
///
/// Doc 04: "prompt (or auto-reconnect if enabled per session)". The
/// settings row is the global default; a per-session override lands with
/// the supervisor seam (P3-3).
pub fn policy_from_settings(reconnect_on_drop: Option<&str>) -> ReconnectPolicy {
    if reconnect_on_drop_enabled(reconnect_on_drop) {
        ReconnectPolicy::auto()
    } else {
        ReconnectPolicy::prompt()
    }
}

/// `reconnect_on_drop` is ON by default (doc 06 seeds `'1'`).
pub fn reconnect_on_drop_enabled(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}
