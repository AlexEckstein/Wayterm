//! Widget checks; they need a display and run from `ui::gtk_tests`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use super::*;

fn tooltip(part: &Part) -> String {
    part.area
        .tooltip_text()
        .map(String::from)
        .unwrap_or_default()
}

fn shown(part: &Part) -> bool {
    part.area.is_visible()
}

pub(in crate::ui) fn segments_follow_the_state_with_tooltips() {
    let bar = StatusBar::new();
    assert_eq!(bar.state.label.text(), "Disconnected");
    assert!(!shown(&bar.endpoint) && !bar.endpoint.separator.is_visible());
    assert!(!shown(&bar.elapsed) && !shown(&bar.savings));

    let saving = Savings {
        percent: 40,
        wire: 60 * 1024,
        uncompressed: 100 * 1024,
    };
    let uptime = ServerUptime {
        secs: 2 * 86_400 + 3 * 3600,
        at: Instant::now(),
        boot_clock: "11/21/26 07:00:00".to_string(),
    };
    let live = LiveStatus {
        savings: Some(saving),
        uptime: Some(uptime),
    };
    bar.set_live_source(Some(Rc::new(move || live.clone())));
    let session = SessionInfo {
        name: "prod".to_string(),
        kind: SessionType::SSH,
        user: Some("alex".to_string()),
        jump: None,
    };
    let since = ConnectedSince {
        at: Instant::now() - Duration::from_secs(75),
        clock: "14:02:11".to_string(),
    };
    bar.set_state(
        &TransportState::Connected,
        "db",
        22,
        Some(&since),
        Some(&session),
    );
    assert_eq!(bar.state.label.text(), "Connected");
    assert_eq!(tooltip(&bar.state), "Connected to db:22");
    assert!(shown(&bar.endpoint) && bar.endpoint.separator.is_visible());
    assert_eq!(bar.endpoint.label.text(), "db:22");
    assert!(tooltip(&bar.endpoint).contains("User: alex"));
    assert_eq!(bar.elapsed.label.text(), "1:15", "counted from `since`");
    assert_eq!(tooltip(&bar.elapsed), "Connected since 14:02:11");
    assert_eq!(bar.savings.label.text(), "Compression saves 40%");
    assert_eq!(bar.uptime.label.text(), "Up 2d 3h");
    assert_eq!(
        tooltip(&bar.uptime),
        "Server up 2 days, 3 hours\nBooted 11/21/26 07:00:00"
    );
    assert!(!bar.uptime.separator.is_visible(), "first on its side");
    assert!(bar.elapsed.separator.is_visible() && bar.savings.separator.is_visible());
    assert!(bar.timer.borrow().is_some());

    bar.set_live_source(Some(Rc::new(LiveStatus::default)));
    bar.set_state(
        &TransportState::Connected,
        "db",
        22,
        Some(&since),
        Some(&session),
    );
    assert!(!shown(&bar.uptime) && !shown(&bar.savings));
    assert!(
        !bar.elapsed.separator.is_visible(),
        "no separator before the first shown segment"
    );

    bar.set_state(
        &TransportState::Reconnecting { attempt: 2 },
        "db",
        22,
        None,
        Some(&session),
    );
    assert_eq!(bar.state.label.text(), "Reconnecting (attempt 2)…");
    assert!(shown(&bar.endpoint));
    assert!(!shown(&bar.uptime) && !shown(&bar.elapsed) && !shown(&bar.savings));
    assert!(!bar.savings.separator.is_visible());
    assert!(bar.timer.borrow().is_none());
}
