//! The tray's display-free core: menu, alerts, and the
//! `minimize_to_tray` decision. The D-Bus service itself is checked
//! by hand (see `src/ui/tray.rs`).

use wayterm::store::SessionStore;
use wayterm::ui::tray;

// --- menu action mapping ----------------------------------------------------

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

#[test]
fn terminal_alert_keeps_a_programs_title_and_body() {
    let alert = tray::terminal_alert(7, "prod-web", "CI", "all green").expect("an alert");
    assert_eq!(alert.title, "CI");
    assert_eq!(alert.body, "all green");
}

/// OSC 9 has no title: the session names where the message came from.
#[test]
fn terminal_alert_without_a_title_falls_back_to_the_session() {
    let alert = tray::terminal_alert(7, "prod-web", "", "done").expect("an alert");
    assert_eq!(alert.title, "prod-web");
    assert_eq!(alert.body, "done");
    let blank = tray::terminal_alert(7, "prod-web", " \t", "done").expect("an alert");
    assert_eq!(blank.title, "prod-web");
}

/// The text is the remote side's: glib panics on a NUL in a debug build
/// and cuts the string there in a release one.
#[test]
fn terminal_alert_strips_control_characters() {
    let alert = tray::terminal_alert(7, "s", "a\0b\x1b[31m", "x\ny\0z\u{9b}").expect("an alert");
    assert_eq!(alert.title, "ab[31m");
    assert_eq!(alert.body, "xyz");
}

#[test]
fn terminal_alert_with_nothing_to_say_is_dropped() {
    assert_eq!(tray::terminal_alert(7, "prod-web", "", ""), None);
    assert_eq!(tray::terminal_alert(7, "prod-web", " ", "\0\x07 "), None);
}

#[test]
fn terminal_notification_ids_are_stable_per_tab() {
    let alert = |tab, body| tray::terminal_alert(tab, "s", "", body).unwrap();
    assert_eq!(alert(1, "a").notification_id, alert(1, "b").notification_id);
    assert_ne!(alert(1, "a").notification_id, alert(2, "a").notification_id);
    assert_ne!(
        alert(1, "a").notification_id,
        tray::disconnect_alert("s").notification_id
    );
}

/// Only a tab the user is looking at stays quiet.
#[test]
fn notifications_skip_only_the_tab_in_view() {
    assert!(!tray::should_notify(true, true));
    assert!(tray::should_notify(true, false));
    assert!(tray::should_notify(false, true));
    assert!(tray::should_notify(false, false));
}

#[test]
fn notification_limit_lets_one_through_per_interval() {
    let mut limit = tray::NotificationLimit::default();
    let start = std::time::Instant::now();
    assert!(limit.allow(start));
    assert!(!limit.allow(start));
    let almost = tray::NOTIFICATION_INTERVAL - std::time::Duration::from_millis(1);
    assert!(!limit.allow(start + almost));
    // A refused one does not push the window back.
    assert!(limit.allow(start + tray::NOTIFICATION_INTERVAL));
    assert!(!limit.allow(start + tray::NOTIFICATION_INTERVAL));
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
    assert_eq!(tray::TRAY_ICON_NAME, APP_ID);
}

#[test]
fn tray_pixmap_is_the_app_icon_as_network_order_argb() {
    let icon = tray::tray_icon_pixmap().expect("the embedded PNG decodes");
    assert_eq!((icon.width, icon.height), (64, 64));
    assert_eq!(icon.data.len(), 64 * 64 * 4);
    let pixel = |x: usize, y: usize| &icon.data[(y * 64 + x) * 4..][..4];
    // Outside the rounded tile: transparent.
    assert_eq!(pixel(0, 0)[0], 0);
    // Alpha first: the black tile, then the front window's left edge.
    assert_eq!(pixel(6, 32), [255, 0, 0, 0]);
    assert_eq!(pixel(11, 32), [255, 255, 255, 255]);
}
