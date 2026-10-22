//! Display-free tests for the reconnect core (doc 03
//! §Lifecycle).
//!
//! These drive [`wayterm::transport::reconnect`] from outside the crate, so
//! they double as a check that the policy, the driver, and the
//! confirm-close rule are all reachable as public API — the GTK half in
//! `wayterm::ui::tab_resilience` consumes exactly this surface.
//!
//! The driver tests use `#[tokio::test(start_paused = true)]` so the
//! backoff curve is asserted against *virtual* time: a fail-3-then-succeed
//! run costs exactly 1s + 2s + 5s of simulated clock and the test still
//! finishes in microseconds of wall time.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use wayterm::transport::reconnect::*;
use wayterm::transport::types::{TransportError, TransportEvent, TransportState};

/// A connect closure plus everything the tests need to inspect: how many
/// attempts ran, when each one ran (virtual clock), and the event log.
struct MockConnector {
    calls: Arc<AtomicU32>,
    /// `Instant` of each attempt, on tokio's (possibly paused) clock.
    attempt_times: Arc<Mutex<Vec<tokio::time::Instant>>>,
    /// Result for call number `n` (1-based).
    script: Arc<dyn Fn(u32) -> Result<(), TransportError> + Send + Sync>,
}

impl MockConnector {
    fn always_ok() -> Self {
        Self::scripted(|_| Ok(()))
    }

    fn always_err() -> Self {
        Self::scripted(|n| Err(TransportError::ConnectionFailed(format!("fail {n}"))))
    }

    fn scripted(
        script: impl Fn(u32) -> Result<(), TransportError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Arc::new(AtomicU32::new(0)),
            attempt_times: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(script),
        }
    }

    /// Fail the first `failures` attempts, then succeed.
    fn fail_then_succeed(failures: u32) -> Self {
        Self::scripted(move |n| {
            if n <= failures {
                Err(TransportError::ConnectionFailed(format!("fail {n}")))
            } else {
                Ok(())
            }
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn connect(&self) -> ConnectFn {
        let calls = self.calls.clone();
        let times = self.attempt_times.clone();
        let script = self.script.clone();
        Arc::new(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            times
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(tokio::time::Instant::now());
            let result = script(n);
            Box::pin(async move { result })
        })
    }

    /// Delay waited before each attempt, measured on the (possibly paused)
    /// clock from the moment the run started. Attempt *n* must have waited
    /// [`next_delay(n)`].
    fn attempt_offsets(&self, run_start: tokio::time::Instant) -> Vec<Duration> {
        let times = self.attempt_times.lock().unwrap_or_else(|e| e.into_inner());
        times
            .iter()
            .map(|t| t.saturating_duration_since(run_start))
            .collect()
    }

    /// Virtual-clock gaps between consecutive attempts.
    fn attempt_gaps(&self) -> Vec<Duration> {
        let times = self.attempt_times.lock().unwrap_or_else(|e| e.into_inner());
        times
            .windows(2)
            .map(|w| w[1].saturating_duration_since(w[0]))
            .collect()
    }
}

/// Prompt-mode responder: answers from a script, records the attempt
/// numbers it was asked about.
struct MockConfirmer {
    answers: Arc<Mutex<Vec<u32>>>,
    script: Arc<dyn Fn(u32) -> bool + Send + Sync>,
}

impl MockConfirmer {
    fn always(answer: bool) -> Self {
        Self {
            answers: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(move |_| answer),
        }
    }

    /// Yes for the first `yeses` prompts, no after that.
    fn yes_then_no(yeses: u32) -> Self {
        Self {
            answers: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(move |n| n <= yeses),
        }
    }

    fn confirm(&self) -> ConfirmFn {
        let answers = self.answers.clone();
        let script = self.script.clone();
        Arc::new(move |attempt| {
            answers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(attempt);
            let answer = script(attempt);
            Box::pin(async move { answer })
        })
    }

    fn asked_for(&self) -> Vec<u32> {
        self.answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn event_channel() -> (
    mpsc::UnboundedSender<ReconnectEvent>,
    mpsc::UnboundedReceiver<ReconnectEvent>,
) {
    mpsc::unbounded_channel()
}

fn state_channel() -> (
    mpsc::UnboundedSender<TransportEvent>,
    mpsc::UnboundedReceiver<TransportEvent>,
) {
    mpsc::unbounded_channel()
}

fn drain_states(rx: &mut mpsc::UnboundedReceiver<TransportEvent>) -> Vec<TransportState> {
    let mut out = Vec::new();
    while let Ok(TransportEvent::StateChanged(s)) = rx.try_recv() {
        out.push(s);
    }
    out
}

fn drain_events(rx: &mut mpsc::UnboundedReceiver<ReconnectEvent>) -> Vec<ReconnectEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

// ── Backoff curve (doc 03: 1s, 2s, 5s, 10s, capped 60s) ────────────────────

#[test]
fn next_delay_walks_the_documented_curve_then_stays_capped() {
    let secs = |n| Duration::from_secs(n);
    let curve: Vec<Duration> = (1..=6).map(next_delay).collect();
    assert_eq!(
        curve,
        [secs(1), secs(2), secs(5), secs(10), secs(60), secs(60)]
    );
    assert_eq!(next_delay(0), secs(1), "attempt 0 is treated as the first");
    for attempt in (7..=u32::MAX).step_by(10_000_000).chain([u32::MAX]) {
        assert_eq!(next_delay(attempt), MAX_BACKOFF, "attempt {attempt}");
    }
}

// ── Policy ──────────────────────────────────────────────────────────────────

#[test]
fn policy_defaults_to_prompt_mode_with_unlimited_attempts() {
    let policy = ReconnectPolicy::default();
    assert_eq!(policy.mode, ReconnectMode::Prompt);
    assert_eq!(policy.max_attempts, None);
    assert!((1..=10_000).all(|attempt| policy.allows_attempt(attempt)));
}

#[test]
fn policy_with_max_attempts_bounds_the_loop() {
    let policy = ReconnectPolicy::auto().with_max_attempts(3);
    assert!(policy.allows_attempt(3));
    assert!(!policy.allows_attempt(4));
}

#[test]
fn the_reconnect_on_drop_setting_selects_the_mode() {
    assert_eq!(ReconnectPolicy::for_setting(true).mode, ReconnectMode::Auto);
    assert_eq!(
        ReconnectPolicy::for_setting(false).mode,
        ReconnectMode::Prompt
    );
}

#[test]
fn driver_starts_uncancelled_and_can_be_cancelled() {
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    assert!(!driver.is_cancelled());
    let token = driver.cancel_token();
    driver.cancel();
    assert!(driver.is_cancelled());
    assert!(token.is_cancelled());
}

#[test]
fn driver_can_adopt_the_tabs_cancellation_token() {
    let tab_token = CancellationToken::new();
    let driver = ReconnectDriver::with_cancel_token(ReconnectPolicy::auto(), tab_token.clone());
    tab_token.cancel();
    assert!(driver.is_cancelled());
}

// ── Auto mode: retry loop (paused tokio time) ───────────────────────────────

#[tokio::test(start_paused = true)]
async fn auto_mode_succeeds_on_first_attempt_without_retrying() {
    let mock = MockConnector::always_ok();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, mut rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 1 });
    assert_eq!(mock.calls(), 1);
    // Attempt 1 still waits its rung (doc 03's curve, see module docs).
    assert_eq!(
        drain_events(&mut rx).first().cloned(),
        Some(ReconnectEvent::BackingOff {
            attempt: 1,
            delay: Duration::from_secs(1)
        })
    );
}

#[tokio::test(start_paused = true)]
async fn auto_mode_fail_three_times_then_succeeds_uses_the_documented_delays() {
    let mock = MockConnector::fail_then_succeed(3);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();
    let run_start = tokio::time::Instant::now();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 4 });
    assert_eq!(mock.calls(), 4);
    // Attempts land at 1s, 1+2s, 1+2+5s, 1+2+5+10s: each one waited its
    // own rung of doc 03's curve.
    assert_eq!(
        mock.attempt_offsets(run_start),
        vec![
            Duration::from_secs(1),
            Duration::from_secs(3),
            Duration::from_secs(8),
            Duration::from_secs(18),
        ]
    );
    assert_eq!(
        mock.attempt_gaps(),
        vec![
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(10),
        ],
        "each retry must wait the rung for its own attempt number"
    );
}

#[tokio::test(start_paused = true)]
async fn auto_mode_attempt_spacing_matches_next_delay_at_every_rung() {
    // Walk past the cap: 7 attempts → 1s,2s,5s,10s,60s,60s between retries.
    let mock = MockConnector::fail_then_succeed(6);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 7 });
    let expected: Vec<Duration> = (2..=7).map(next_delay).collect();
    assert_eq!(mock.attempt_gaps(), expected);
}

#[tokio::test(start_paused = true)]
async fn auto_mode_unlimited_attempts_keep_looping_past_the_cap() {
    let mock = MockConnector::fail_then_succeed(200);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 201 });
    assert_eq!(mock.calls(), 201);
}

#[tokio::test(start_paused = true)]
async fn auto_mode_emits_failed_then_connected_events() {
    let mock = MockConnector::fail_then_succeed(2);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, mut rx) = event_channel();

    driver.run(mock.connect(), &events, None, None).await;

    let log = drain_events(&mut rx);
    let failures: Vec<u32> = log
        .iter()
        .filter_map(|ev| match ev {
            ReconnectEvent::AttemptFailed { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(failures, vec![1, 2]);
    assert_eq!(
        log.last().cloned(),
        Some(ReconnectEvent::Connected { attempts: 3 })
    );
}

#[tokio::test(start_paused = true)]
async fn auto_mode_bounded_policy_reports_exhausted_with_the_last_error() {
    let mock = MockConnector::always_err();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto().with_max_attempts(3));
    let (events, mut rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(mock.calls(), 3);
    match outcome {
        ReconnectOutcome::Exhausted {
            attempts,
            ref error,
        } => {
            assert_eq!(attempts, 3);
            assert!(error.contains("fail 3"), "got: {error}");
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
    assert!(drain_events(&mut rx)
        .iter()
        .any(|ev| matches!(ev, ReconnectEvent::Exhausted { attempts: 3, .. })));
}

#[tokio::test(start_paused = true)]
async fn transport_error_cancelled_stops_the_loop_immediately() {
    // Doc 03: cancelling a prompt aborts the connect with
    // TransportError::Cancelled — that is not a transient fault.
    let mock = MockConnector::scripted(|_| Err(TransportError::Cancelled));
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, mut rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Cancelled);
    assert_eq!(mock.calls(), 1, "Cancelled must not be retried");
    assert!(drain_events(&mut rx)
        .iter()
        .any(|ev| matches!(ev, ReconnectEvent::Cancelled)));
}

// ── Cancellation ────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn cancel_before_the_run_starts_makes_no_attempt() {
    let mock = MockConnector::always_ok();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    driver.cancel();
    let (events, _rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Cancelled);
    assert_eq!(mock.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn cancel_mid_backoff_stops_cleanly_without_the_next_attempt() {
    let mock = MockConnector::always_err();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();

    let run = tokio::spawn({
        let driver = driver.clone();
        let connect = mock.connect();
        let events = events.clone();
        async move { driver.run(connect, &events, None, None).await }
    });
    // Let the driver reach its first sleep, then cancel mid-backoff.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    driver.cancel();

    assert_eq!(run.await.expect("driver task"), ReconnectOutcome::Cancelled);
    assert_eq!(
        mock.calls(),
        0,
        "a cancelled backoff must not run its attempt"
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_after_a_failure_does_not_run_the_queued_attempt() {
    let mock = MockConnector::always_err();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();

    let run = tokio::spawn({
        let driver = driver.clone();
        let connect = mock.connect();
        let events = events.clone();
        async move { driver.run(connect, &events, None, None).await }
    });
    // Let the driver reach its first sleep, then elapse the 1s rung:
    // attempt 1 runs, fails, and the driver sleeps the 2s rung.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(1_100)).await;
    tokio::task::yield_now().await;
    driver.cancel();

    assert_eq!(run.await.expect("driver task"), ReconnectOutcome::Cancelled);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn tab_close_token_cancels_a_shared_driver() {
    // Doc 03: tab close fires the tab's token; the reconnect driver shares
    // it, so closing the tab during backoff stops the loop.
    let tab_token = CancellationToken::new();
    let mock = MockConnector::always_err();
    let driver = ReconnectDriver::with_cancel_token(ReconnectPolicy::auto(), tab_token.clone());
    let (events, _rx) = event_channel();

    let run = tokio::spawn({
        let driver = driver.clone();
        let connect = mock.connect();
        let events = events.clone();
        async move { driver.run(connect, &events, None, None).await }
    });
    tokio::task::yield_now().await;
    tab_token.cancel();

    assert_eq!(run.await.expect("driver task"), ReconnectOutcome::Cancelled);
    assert_eq!(mock.calls(), 0);
}

// ── Prompt mode ─────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn prompt_mode_holds_for_the_ui_confirm_before_connecting() {
    let mock = MockConnector::always_ok();
    let confirmer = MockConfirmer::always(true);
    let driver = ReconnectDriver::new(ReconnectPolicy::prompt());
    let (events, _rx) = event_channel();

    let outcome = driver
        .run(mock.connect(), &events, Some(confirmer.confirm()), None)
        .await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 1 });
    assert_eq!(confirmer.asked_for(), vec![1]);
}

#[tokio::test(start_paused = true)]
async fn prompt_mode_declined_stops_without_connecting() {
    let mock = MockConnector::always_ok();
    let confirmer = MockConfirmer::always(false);
    let driver = ReconnectDriver::new(ReconnectPolicy::prompt());
    let (events, mut rx) = event_channel();

    let outcome = driver
        .run(mock.connect(), &events, Some(confirmer.confirm()), None)
        .await;

    assert_eq!(outcome, ReconnectOutcome::Declined { attempt: 1 });
    assert_eq!(mock.calls(), 0);
    assert!(drain_events(&mut rx)
        .iter()
        .any(|ev| matches!(ev, ReconnectEvent::Declined { attempt: 1 })));
}

#[tokio::test(start_paused = true)]
async fn prompt_mode_without_a_responder_declines_instead_of_hanging() {
    let mock = MockConnector::always_ok();
    let driver = ReconnectDriver::new(ReconnectPolicy::prompt());
    let (events, _rx) = event_channel();

    let outcome = driver.run(mock.connect(), &events, None, None).await;

    assert_eq!(outcome, ReconnectOutcome::Declined { attempt: 1 });
    assert_eq!(mock.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn prompt_mode_reprompts_for_every_attempt_until_the_user_gives_up() {
    let mock = MockConnector::always_err();
    let confirmer = MockConfirmer::yes_then_no(2);
    let driver = ReconnectDriver::new(ReconnectPolicy::prompt());
    let (events, mut rx) = event_channel();

    let outcome = driver
        .run(mock.connect(), &events, Some(confirmer.confirm()), None)
        .await;

    assert_eq!(outcome, ReconnectOutcome::Declined { attempt: 3 });
    assert_eq!(mock.calls(), 2, "declined on the third prompt");
    assert_eq!(confirmer.asked_for(), vec![1, 2, 3]);
    assert!(drain_events(&mut rx)
        .iter()
        .any(|ev| matches!(ev, ReconnectEvent::Prompting { attempt: 3 })));
}

#[tokio::test(start_paused = true)]
async fn prompt_mode_backoff_applies_between_prompts() {
    let mock = MockConnector::fail_then_succeed(2);
    let confirmer = MockConfirmer::always(true);
    let driver = ReconnectDriver::new(ReconnectPolicy::prompt());
    let (events, _rx) = event_channel();

    let outcome = driver
        .run(mock.connect(), &events, Some(confirmer.confirm()), None)
        .await;

    assert_eq!(outcome, ReconnectOutcome::Connected { attempts: 3 });
    assert_eq!(
        mock.attempt_gaps(),
        vec![Duration::from_secs(2), Duration::from_secs(5)]
    );
}

#[tokio::test(start_paused = true)]
async fn auto_mode_never_consults_the_confirm_responder() {
    let mock = MockConnector::always_ok();
    let confirmer = MockConfirmer::always(false);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();

    let outcome = driver
        .run(mock.connect(), &events, Some(confirmer.confirm()), None)
        .await;

    assert!(outcome.is_connected());
    assert!(confirmer.asked_for().is_empty());
}

// ── TransportState emission (status bar / reconnect prompt) ─────────────────

#[tokio::test(start_paused = true)]
async fn run_emits_reconnecting_attempt_then_connected() {
    let mock = MockConnector::fail_then_succeed(2);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, _rx) = event_channel();
    let (state_tx, mut state_rx) = state_channel();

    driver
        .run(mock.connect(), &events, None, Some(&state_tx))
        .await;

    assert_eq!(
        drain_states(&mut state_rx),
        vec![
            TransportState::Reconnecting { attempt: 1 },
            TransportState::Reconnecting { attempt: 2 },
            TransportState::Reconnecting { attempt: 3 },
            TransportState::Connected,
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn exhausted_run_returns_the_tab_to_disconnected() {
    let mock = MockConnector::always_err();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto().with_max_attempts(1));
    let (events, _rx) = event_channel();
    let (state_tx, mut state_rx) = state_channel();

    driver
        .run(mock.connect(), &events, None, Some(&state_tx))
        .await;

    assert_eq!(
        drain_states(&mut state_rx),
        vec![
            TransportState::Reconnecting { attempt: 1 },
            TransportState::Disconnected,
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_run_returns_the_tab_to_disconnected() {
    let mock = MockConnector::always_ok();
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    driver.cancel();
    let (events, _rx) = event_channel();
    let (state_tx, mut state_rx) = state_channel();

    driver
        .run(mock.connect(), &events, None, Some(&state_tx))
        .await;

    assert_eq!(drain_states(&mut state_rx), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn spawn_runs_the_driver_and_reports_the_outcome() {
    let mock = MockConnector::fail_then_succeed(1);
    let driver = ReconnectDriver::new(ReconnectPolicy::auto());
    let (events, mut rx) = event_channel();

    let handle = driver.spawn(mock.connect(), events.clone(), None, None);

    assert_eq!(
        handle.await.expect("driver task"),
        ReconnectOutcome::Connected { attempts: 2 }
    );
    assert!(matches!(
        drain_events(&mut rx).last(),
        Some(ReconnectEvent::Connected { attempts: 2 })
    ));
}

// ── Confirm-before-close policy (doc 03 §Lifecycle, setting `confirm_close`) ─

#[test]
fn live_states_are_the_ones_that_can_have_remote_processes() {
    for live in [
        TransportState::Connected,
        TransportState::Connecting,
        TransportState::VerifyingHostKey,
        TransportState::Authenticating,
        TransportState::Reconnecting { attempt: 4 },
    ] {
        assert!(is_live(&live), "{live:?} should be live");
    }
}

#[test]
fn dead_states_are_not_live() {
    assert!(!is_live(&TransportState::Disconnected));
    assert!(!is_live(&TransportState::Error("boom".into())));
}

#[test]
fn confirm_close_is_required_for_a_live_transport_when_enabled() {
    assert!(should_confirm_close(&TransportState::Connected, true));
    assert!(should_confirm_close(
        &TransportState::Reconnecting { attempt: 1 },
        true
    ));
}

#[test]
fn confirm_close_is_skipped_when_the_setting_is_off() {
    assert!(!should_confirm_close(&TransportState::Connected, false));
}

#[test]
fn confirm_close_is_skipped_for_a_dead_transport() {
    assert!(!should_confirm_close(&TransportState::Disconnected, true));
    assert!(!should_confirm_close(
        &TransportState::Error("gone".into()),
        true
    ));
}

#[test]
fn outcome_is_connected_only_on_success() {
    assert!(ReconnectOutcome::Connected { attempts: 1 }.is_connected());
    assert!(!ReconnectOutcome::Cancelled.is_connected());
    assert!(!ReconnectOutcome::Declined { attempt: 1 }.is_connected());
    assert!(!ReconnectOutcome::Exhausted {
        attempts: 1,
        error: "x".into()
    }
    .is_connected());
}
