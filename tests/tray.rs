//! Display-free tests for the tray core (`src/ui/tray.rs`, P4-5).
//!
//! Nothing here touches GTK or D-Bus: menu mapping, notification
//! formatting, `ConnectionLost` triage, and the `minimize_to_tray`
//! setting logic (init-ok/fail branches + settings-row round-trip).
//! The tray service itself is display-bound — see `TrayHandle`'s
//! doc-comment for the manual verification path
//! (`xvfb-run -a timeout 10 cargo run` → exit 124, no panic).

use wayterm::store::SessionStore;
use wayterm::transport::types::{TransportEvent, TransportState};
use wayterm::ui::tray;

// --- menu action mapping ----------------------------------------------------

#[test]
fn tray_action_labels_are_distinct_and_nonempty() {
    use tray::TrayAction;
    let labels = [
        TrayAction::ShowWindow.label(),
        TrayAction::NewSession.label(),
        TrayAction::Quit.label(),
    ];
    for label in labels {
        assert!(!label.is_empty());
    }
    assert_eq!(labels.len(), 3);
    let mut dedup = labels.to_vec();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), 3, "menu labels must be distinct");
}

#[test]
fn tray_menu_layout_is_open_new_quit() {
    use tray::{MenuEntry, TrayAction};
    assert_eq!(
        tray::tray_menu_layout(),
        vec![
            MenuEntry::Item(TrayAction::ShowWindow),
            MenuEntry::Item(TrayAction::NewSession),
            MenuEntry::Separator,
            MenuEntry::Item(TrayAction::Quit),
        ]
    );
    assert_eq!(TrayAction::ShowWindow.label(), "Open Wayterm");
    assert_eq!(TrayAction::NewSession.label(), "New Session");
    assert_eq!(TrayAction::Quit.label(), "Quit");
}

// --- notification formatting ------------------------------------------------

#[test]
fn disconnect_alert_body_contract() {
    let alert = tray::disconnect_alert("prod-web");
    assert_eq!(alert.title, "Wayterm");
    assert_eq!(alert.body, "prod-web connection lost");
}

#[test]
fn disconnect_notification_ids_are_stable_per_session() {
    let a = tray::disconnect_alert("prod-web");
    let b = tray::disconnect_alert("prod-web");
    let c = tray::disconnect_alert("db-01");
    assert_eq!(a.notification_id, b.notification_id);
    assert_ne!(a.notification_id, c.notification_id);
}

// --- transport event triage -------------------------------------------------

#[test]
fn only_connection_lost_produces_an_alert() {
    let lost = tray::triage_transport_event("s", &TransportEvent::ConnectionLost);
    assert_eq!(lost, Some(tray::disconnect_alert("s")));

    for ignorable in [
        TransportEvent::DataReceived(vec![1, 2, 3]),
        TransportEvent::StateChanged(TransportState::Connected),
        TransportEvent::StateChanged(TransportState::Error("x".into())),
        TransportEvent::Error("boom".into()),
    ] {
        assert_eq!(tray::triage_transport_event("s", &ignorable), None);
    }
}

#[tokio::test]
async fn forwarder_emits_one_alert_per_connection_lost() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(TransportEvent::DataReceived(vec![1])).unwrap();
    tx.send(TransportEvent::ConnectionLost).unwrap();
    tx.send(TransportEvent::StateChanged(TransportState::Disconnected))
        .unwrap();
    tx.send(TransportEvent::ConnectionLost).unwrap();
    drop(tx);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_cb = seen.clone();
    tray::forward_disconnect_alerts("edge-01".to_string(), rx, move |alert| {
        seen_cb.lock().unwrap().push(alert);
    })
    .await;

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        &[
            tray::disconnect_alert("edge-01"),
            tray::disconnect_alert("edge-01"),
        ]
    );
}

#[tokio::test]
async fn forwarder_is_silent_without_connection_lost() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(TransportEvent::StateChanged(TransportState::Connected))
        .unwrap();
    drop(tx);

    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_cb = count.clone();
    tray::forward_disconnect_alerts("quiet".to_string(), rx, move |_| {
        count_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    })
    .await;

    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// --- minimize-to-tray setting logic -----------------------------------------

#[test]
fn setting_defaults_on_when_row_absent() {
    // Fresh store has the 10 doc-06 seed rows but no minimize_to_tray —
    // the contract is fail-open (default ON) without a seed migration.
    let store = SessionStore::in_memory().unwrap();
    assert_eq!(store.get_setting(tray::MINIMIZE_TO_TRAY_KEY).unwrap(), None);
    assert!(tray::minimize_to_tray_enabled(&store));
}

#[test]
fn setting_parse_matrix() {
    use tray::parse_setting_enabled as parse;
    assert!(parse(None));
    for on in [
        "1",
        "true",
        "True",
        "TRUE",
        "yes",
        "on",
        " 1 ",
        "anything-else",
    ] {
        assert!(parse(Some(on)), "expected ON for {on:?}");
    }
    for off in ["0", "false", "False", "FALSE", "no", "off", " 0 ", "OFF"] {
        assert!(!parse(Some(off)), "expected OFF for {off:?}");
    }
}

#[test]
fn settings_row_round_trip() {
    let store = SessionStore::in_memory().unwrap();
    store.set_setting(tray::MINIMIZE_TO_TRAY_KEY, "0").unwrap();
    assert_eq!(
        store
            .get_setting(tray::MINIMIZE_TO_TRAY_KEY)
            .unwrap()
            .as_deref(),
        Some("0")
    );
    assert!(!tray::minimize_to_tray_enabled(&store));

    store.set_setting(tray::MINIMIZE_TO_TRAY_KEY, "1").unwrap();
    assert!(tray::minimize_to_tray_enabled(&store));
}

#[test]
fn should_minimize_branches() {
    use tray::should_minimize_to_tray as decide;
    // init-ok + setting on → minimize.
    assert!(decide(true, true));
    // setting off → quit even with a tray.
    assert!(!decide(true, false));
    // init FAILED (headless CI) → quit regardless of the setting.
    assert!(!decide(false, true));
    assert!(!decide(false, false));
}

// --- tray identity ----------------------------------------------------------

#[test]
fn tray_identity_is_wayterm() {
    use wayterm::app::APP_ID;
    assert_eq!(tray::TRAY_ID, APP_ID);
    assert_eq!(tray::NOTIFICATION_TITLE, "Wayterm");
    assert!(!tray::TRAY_ICON_NAME.is_empty());
}
