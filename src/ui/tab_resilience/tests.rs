//! The resilience registry, close guard, and the two actions — all
//! display-free.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::transport::reconnect::{ConnectFn, ConnectFuture};
use crate::transport::types::TransportState;

const SESSION: i64 = 42;

fn always_ok_connect() -> ConnectFn {
    Arc::new(|| Box::pin(async { Ok(()) }) as ConnectFuture)
}

fn counting_connect(calls: Arc<AtomicU32>) -> ConnectFn {
    Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) }) as ConnectFuture
    })
}

fn tab_in(state: TransportState) -> LiveTab {
    LiveTab::new(always_ok_connect(), CancellationToken::new(), state)
}

/// The registry is process-global, so its tests take turns.
fn fresh() -> tokio::sync::MutexGuard<'static, ()> {
    loop {
        match TEST_LOCK.try_lock() {
            Ok(guard) => {
                reset_registry();
                return guard;
            }
            // A sync test cannot await, so spin instead.
            Err(_) => std::thread::yield_now(),
        }
    }
}

/// [`fresh`] for async tests.
async fn fresh_async() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().await;
    reset_registry();
    guard
}

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ── Settings installation ───────────────────────────────────────────────────

#[test]
fn settings_default_to_confirm_close_and_reconnect_on() {
    let _guard = fresh();
    assert!(confirm_close_setting());
    assert!(reconnect_on_drop_setting());
}

#[test]
fn install_settings_honours_both_rows() {
    let _guard = fresh();
    install_settings(false, false);
    assert!(!confirm_close_setting());
    assert!(!reconnect_on_drop_setting());
}

#[test]
fn policy_follows_the_reconnect_on_drop_setting() {
    let _guard = fresh();
    install_settings(true, false);
    assert_eq!(
        policy().mode,
        crate::transport::reconnect::ReconnectMode::Prompt
    );
    install_settings(true, true);
    assert_eq!(
        policy().mode,
        crate::transport::reconnect::ReconnectMode::Auto
    );
}

// ── Registry ────────────────────────────────────────────────────────────────

#[test]
fn an_unregistered_tab_reads_as_disconnected() {
    let _guard = fresh();
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);
    assert!(live_tab(SESSION).is_none());
}

#[test]
fn registering_a_live_tab_publishes_its_state() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    assert_eq!(tab_state(SESSION), TransportState::Connected);
}

#[test]
fn registering_the_same_session_twice_replaces_the_entry() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    register_live_tab(SESSION, tab_in(TransportState::Authenticating));
    assert_eq!(tab_state(SESSION), TransportState::Authenticating);
}

#[test]
fn set_tab_state_updates_a_registered_tab() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connecting));
    set_tab_state(SESSION, TransportState::Reconnecting { attempt: 2 });
    assert_eq!(
        tab_state(SESSION),
        TransportState::Reconnecting { attempt: 2 }
    );
}

#[test]
fn set_tab_state_ignores_a_closed_tab() {
    let _guard = fresh();
    set_tab_state(SESSION, TransportState::Connected);
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);
}

#[test]
fn unregistering_forgets_the_transport() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    unregister_live_tab(SESSION);
    assert!(live_tab(SESSION).is_none());
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);
}

#[test]
fn unregistering_an_unknown_tab_is_a_no_op() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    unregister_live_tab(999);
    assert_eq!(tab_state(SESSION), TransportState::Connected);
}

#[test]
fn tabs_are_tracked_per_session() {
    let _guard = fresh();
    register_live_tab(1, tab_in(TransportState::Connected));
    register_live_tab(2, tab_in(TransportState::Disconnected));
    assert_eq!(tab_state(1), TransportState::Connected);
    assert_eq!(tab_state(2), TransportState::Disconnected);
    unregister_live_tab(1);
    assert_eq!(tab_state(2), TransportState::Disconnected);
}

// ── Confirm-before-close (doc 03 §Lifecycle, setting `confirm_close`) ───────

#[test]
fn a_connected_tab_asks_before_close_when_the_setting_is_on() {
    let _guard = fresh();
    install_settings(true, true);
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    assert!(should_confirm_close_for_tab(SESSION));
}

#[test]
fn a_reconnecting_tab_asks_before_close() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Reconnecting { attempt: 3 }));
    assert!(should_confirm_close_for_tab(SESSION));
}

#[test]
fn a_tab_with_no_transport_closes_without_asking() {
    let _guard = fresh();
    assert!(!should_confirm_close_for_tab(SESSION));
}

#[test]
fn a_disconnected_tab_closes_without_asking() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Disconnected));
    assert!(!should_confirm_close_for_tab(SESSION));
}

#[test]
fn an_errored_tab_closes_without_asking() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Error("boom".into())));
    assert!(!should_confirm_close_for_tab(SESSION));
}

#[test]
fn the_setting_turns_the_prompt_off() {
    let _guard = fresh();
    install_settings(false, true);
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    assert!(!should_confirm_close_for_tab(SESSION));
}

#[test]
fn do_not_show_again_silences_the_warning_until_reminders_are_cleared() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    let store = SessionStore::in_memory().unwrap();
    assert!(should_show_close_warning(SESSION, Some(&store)));
    assert!(should_show_close_warning(SESSION, None));

    store
        .set_setting(CONFIRM_CLOSE_NOTICE_SETTING, "0")
        .unwrap();
    assert!(!should_show_close_warning(SESSION, Some(&store)));

    crate::ui::settings::general::clear_reminders(&store).unwrap();
    assert!(should_show_close_warning(SESSION, Some(&store)));
}

// ── reconnect action ────────────────────────────────────────────────────────

#[test]
fn reconnect_without_an_active_tab_reports_the_seam() {
    let _guard = fresh();
    assert_eq!(
        activate_reconnect(None, None, None),
        ReconnectAction::NoLiveTab { tab_id: None }
    );
    assert!(!activate_reconnect(None, None, None).started());
}

#[test]
fn reconnect_on_a_tab_with_no_transport_reports_the_seam() {
    let _guard = fresh();
    assert_eq!(
        activate_reconnect(Some(SESSION), None, None),
        ReconnectAction::NoLiveTab {
            tab_id: Some(SESSION)
        }
    );
}

#[tokio::test]
async fn reconnect_starts_a_driver_for_a_registered_tab() {
    let _guard = fresh_async().await;
    install_settings(true, true); // auto mode
    register_live_tab(SESSION, tab_in(TransportState::Disconnected));
    assert!(activate_reconnect(Some(SESSION), None, None).started());
}

#[tokio::test]
async fn a_second_reconnect_does_not_stack_drivers() {
    let _guard = fresh_async().await;
    register_live_tab(SESSION, tab_in(TransportState::Disconnected));
    assert!(activate_reconnect(Some(SESSION), None, None).started());
    assert_eq!(
        activate_reconnect(Some(SESSION), None, None),
        ReconnectAction::AlreadyRunning { tab_id: SESSION }
    );
}

#[tokio::test(start_paused = true)]
async fn reconnect_really_runs_the_connect_closure() {
    let _guard = fresh_async().await;
    let calls = Arc::new(AtomicU32::new(0));
    register_live_tab(
        SESSION,
        LiveTab::new(
            counting_connect(calls.clone()),
            CancellationToken::new(),
            TransportState::Disconnected,
        ),
    );

    activate_reconnect(Some(SESSION), None, None);

    // The driver waits doc 03's 1s rung before its first attempt; the
    // test's own sleep drives the paused clock past it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn reconnect_emits_reconnecting_on_the_tabs_state_channel() {
    let _guard = fresh_async().await;
    register_live_tab(SESSION, tab_in(TransportState::Disconnected));
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();

    activate_reconnect(Some(SESSION), Some(state_tx), None);

    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut saw_reconnecting = false;
    let mut saw_connected = false;
    while let Ok(ev) = state_rx.try_recv() {
        if let TransportEvent::StateChanged(state) = ev {
            match state {
                TransportState::Reconnecting { .. } => saw_reconnecting = true,
                TransportState::Connected => saw_connected = true,
                _ => {}
            }
        }
    }
    assert!(
        saw_reconnecting,
        "driver must publish Reconnecting{{attempt}}"
    );
    assert!(saw_connected, "driver must publish Connected on success");
}

#[tokio::test(start_paused = true)]
async fn disconnect_cancels_a_running_reconnect_loop() {
    let _guard = fresh_async().await;
    let calls = Arc::new(AtomicU32::new(0));
    let cancel = CancellationToken::new();
    register_live_tab(
        SESSION,
        LiveTab::new(
            counting_connect(calls.clone()),
            cancel.clone(),
            TransportState::Disconnected,
        ),
    );

    activate_reconnect(Some(SESSION), None, None);
    let result = activate_disconnect(Some(SESSION));
    assert!(result.cancelled());
    assert!(cancel.is_cancelled());

    // Give the cancelled loop every chance to misbehave.
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a cancelled tab must not run a reconnect attempt"
    );
}

#[tokio::test(start_paused = true)]
async fn prompt_mode_reconnect_defers_to_the_confirm_responder() {
    let _guard = fresh_async().await;
    install_settings(true, false); // prompt mode
    assert_eq!(
        policy().mode,
        crate::transport::reconnect::ReconnectMode::Prompt
    );

    let calls = Arc::new(AtomicU32::new(0));
    register_live_tab(
        SESSION,
        LiveTab::new(
            counting_connect(calls.clone()),
            CancellationToken::new(),
            TransportState::Disconnected,
        ),
    );
    let declined = Arc::new(Mutex::new(Vec::<u32>::new()));
    let seen = declined.clone();
    let confirm: ConfirmFn = Arc::new(move |attempt| {
        seen.lock().unwrap_or_else(|e| e.into_inner()).push(attempt);
        Box::pin(async { false })
    });

    activate_reconnect(Some(SESSION), None, Some(confirm));

    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "declined must not connect");
    assert_eq!(*declined.lock().unwrap(), vec![1]);
}

// ── disconnect action ───────────────────────────────────────────────────────

#[test]
fn disconnect_fires_the_tabs_cancellation_token() {
    let _guard = fresh();
    let cancel = CancellationToken::new();
    register_live_tab(
        SESSION,
        LiveTab::new(
            always_ok_connect(),
            cancel.clone(),
            TransportState::Connected,
        ),
    );

    assert_eq!(
        activate_disconnect(Some(SESSION)),
        DisconnectAction::Cancelled { tab_id: SESSION }
    );
    assert!(cancel.is_cancelled());
}

#[test]
fn disconnect_leaves_the_tab_disconnected_and_open() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    activate_disconnect(Some(SESSION));
    // The registry entry survives (the tab is still open); only its state
    // changes, which is what the status bar reads.
    assert!(live_tab(SESSION).is_some());
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);
}

#[test]
fn disconnect_without_an_active_tab_reports_the_seam() {
    let _guard = fresh();
    assert_eq!(
        activate_disconnect(None),
        DisconnectAction::NoLiveTab { tab_id: None }
    );
    assert!(!activate_disconnect(None).cancelled());
}

#[test]
fn disconnect_on_a_tab_with_no_transport_reports_the_seam() {
    let _guard = fresh();
    assert_eq!(
        activate_disconnect(Some(SESSION)),
        DisconnectAction::NoLiveTab {
            tab_id: Some(SESSION)
        }
    );
}

#[test]
fn a_second_disconnect_stays_idempotent() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    assert!(activate_disconnect(Some(SESSION)).cancelled());
    assert!(activate_disconnect(Some(SESSION)).cancelled());
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);
}

#[test]
fn the_registry_resets_cleanly_between_windows() {
    let _guard = fresh();
    install_settings(false, false);
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    reset_registry();
    assert!(live_tab(SESSION).is_none());
    assert!(confirm_close_setting());
    assert!(reconnect_on_drop_setting());
}

// ── Tab close teardown (doc 03: "Tab close: cancel token → pump task
// exits → transport.disconnect()") ───────────────────────────────────────────

#[test]
fn teardown_fires_the_tabs_token() {
    let _guard = fresh();
    let token = CancellationToken::new();
    register_live_tab(
        SESSION,
        LiveTab::new(
            always_ok_connect(),
            token.clone(),
            TransportState::Connected,
        ),
    );
    assert!(!token.is_cancelled());

    teardown_tab(SESSION);

    assert!(token.is_cancelled());
}

#[test]
fn teardown_forgets_the_transport() {
    let _guard = fresh();
    register_live_tab(SESSION, tab_in(TransportState::Connected));

    teardown_tab(SESSION);

    assert!(live_tab(SESSION).is_none());
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);
}

#[test]
fn teardown_stops_an_in_flight_reconnect_loop() {
    // Doc 03 allows unlimited attempts "while the tab is open" — so a
    // closed tab must not keep a driver looping.
    let _guard = fresh();
    let token = CancellationToken::new();
    register_live_tab(
        SESSION,
        LiveTab::new(
            always_ok_connect(),
            token.clone(),
            TransportState::Reconnecting { attempt: 2 },
        ),
    );
    let driver = ReconnectDriver::with_cancel_token(policy(), token.clone());

    teardown_tab(SESSION);

    assert!(driver.is_cancelled(), "close must cancel the shared token");
}

#[tokio::test]
async fn teardown_clears_the_running_marker_so_a_reopen_can_reconnect() {
    let _guard = fresh_async().await;
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    assert!(matches!(
        activate_reconnect(Some(SESSION), None, None),
        ReconnectAction::Started { .. }
    ));

    teardown_tab(SESSION);

    // Re-register (the connect flow reopened the session) and reconnect again.
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    assert!(
        matches!(
            activate_reconnect(Some(SESSION), None, None),
            ReconnectAction::Started { .. }
        ),
        "a reopened tab must not inherit the old tab's running marker"
    );
}

#[test]
fn teardown_of_an_unregistered_tab_is_a_no_op() {
    let _guard = fresh();
    teardown_tab(999);
    assert!(live_tab(999).is_none());
}

#[test]
fn teardown_leaves_other_tabs_alone() {
    let _guard = fresh();
    let other_token = CancellationToken::new();
    register_live_tab(SESSION, tab_in(TransportState::Connected));
    register_live_tab(
        SESSION + 1,
        LiveTab::new(
            always_ok_connect(),
            other_token.clone(),
            TransportState::Connected,
        ),
    );

    teardown_tab(SESSION);

    assert!(live_tab(SESSION).is_none());
    assert!(live_tab(SESSION + 1).is_some());
    assert!(
        !other_token.is_cancelled(),
        "closing one tab is not a disconnect-all"
    );
}

// ── Connect-flow registration (every streaming tab, first connect and
// every redial) ──────────────────────────────────────────────────────────────

#[test]
fn a_wired_tab_registers_as_connecting() {
    let _guard = fresh();
    register_connected_tab(SESSION, always_ok_connect());
    assert_eq!(tab_state(SESSION), TransportState::Connecting);
    assert!(should_confirm_close_for_tab(SESSION));
}

#[test]
fn a_redial_keeps_the_token_its_driver_runs_on() {
    let _guard = fresh();
    let first = register_connected_tab(SESSION, always_ok_connect());
    let rewired = register_connected_tab(SESSION, always_ok_connect());

    teardown_tab(SESSION);

    assert!(rewired.is_cancelled());
    assert!(
        first.is_cancelled(),
        "a driver started before the redial must stop when the tab closes"
    );
}

#[test]
fn a_connect_after_disconnect_gets_a_fresh_token() {
    let _guard = fresh();
    let first = register_connected_tab(SESSION, always_ok_connect());
    activate_disconnect(Some(SESSION));
    assert!(first.is_cancelled());

    let second = register_connected_tab(SESSION, always_ok_connect());
    assert!(!second.is_cancelled());
    assert_eq!(tab_state(SESSION), TransportState::Connecting);
}

#[tokio::test(start_paused = true)]
async fn reconnect_after_disconnect_runs_on_a_fresh_token() {
    let _guard = fresh_async().await;
    let calls = Arc::new(AtomicU32::new(0));
    let old = register_connected_tab(SESSION, counting_connect(calls.clone()));
    activate_disconnect(Some(SESSION));

    assert!(activate_reconnect(Some(SESSION), None, None).started());
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(old.is_cancelled());
    let live = live_tab(SESSION).expect("still registered");
    assert!(!live.cancel.is_cancelled());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the driver must really redial"
    );
}

// ── ConnectionLost (the state drain's half of doc 03 §Lifecycle) ────────────

#[tokio::test(start_paused = true)]
async fn connection_lost_auto_reconnects_on_the_tabs_state_channel() {
    let _guard = fresh_async().await;
    install_settings(true, true);
    let calls = Arc::new(AtomicU32::new(0));
    register_connected_tab(SESSION, counting_connect(calls.clone()));
    set_tab_state(SESSION, TransportState::Connected);
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();

    let action = on_connection_lost(SESSION, state_tx);
    assert_eq!(action, Some(ReconnectAction::Started { tab_id: SESSION }));
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let states: Vec<_> = std::iter::from_fn(|| state_rx.try_recv().ok())
        .filter_map(|event| match event {
            TransportEvent::StateChanged(state) => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            TransportState::Reconnecting { attempt: 1 },
            TransportState::Connected
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn connection_lost_in_prompt_mode_waits_for_the_reconnect_action() {
    let _guard = fresh_async().await;
    install_settings(true, false);
    let calls = Arc::new(AtomicU32::new(0));
    register_connected_tab(SESSION, counting_connect(calls.clone()));
    set_tab_state(SESSION, TransportState::Connected);
    let (state_tx, _state_rx) = mpsc::unbounded_channel();

    assert_eq!(on_connection_lost(SESSION, state_tx), None);
    assert_eq!(tab_state(SESSION), TransportState::Disconnected);

    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "prompt mode never redials alone"
    );
    assert!(
        activate_reconnect(Some(SESSION), None, None).started(),
        "the manual action still finds the tab"
    );
}

#[tokio::test]
async fn connection_lost_after_close_starts_nothing() {
    let _guard = fresh_async().await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    assert_eq!(
        on_connection_lost(SESSION, state_tx),
        Some(ReconnectAction::NoLiveTab {
            tab_id: Some(SESSION)
        })
    );
}

#[tokio::test]
async fn a_second_connection_lost_does_not_stack_drivers() {
    let _guard = fresh_async().await;
    register_connected_tab(SESSION, always_ok_connect());
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    assert!(on_connection_lost(SESSION, state_tx.clone()).is_some_and(ReconnectAction::started));
    assert_eq!(
        on_connection_lost(SESSION, state_tx),
        Some(ReconnectAction::AlreadyRunning { tab_id: SESSION })
    );
}
