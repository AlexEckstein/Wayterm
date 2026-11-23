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
    bar.set_savings_source(Some(Rc::new(move || Some(saving))));
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
    assert!(bar.timer.borrow().is_some());

    bar.set_state(
        &TransportState::Reconnecting { attempt: 2 },
        "db",
        22,
        None,
        Some(&session),
    );
    assert_eq!(bar.state.label.text(), "Reconnecting (attempt 2)…");
    assert!(shown(&bar.endpoint));
    assert!(!shown(&bar.elapsed) && !shown(&bar.savings));
    assert!(!bar.savings.separator.is_visible());
    assert!(bar.timer.borrow().is_none());
}
