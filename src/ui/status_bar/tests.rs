//! Unit tests for the display-free data layer of [`super::StatusBar`].
//!
//! These tests avoid GTK initialization and exercise only
//! [`status_view_from`], [`format_elapsed`] and [`ColorKind`].

use crate::transport::types::TransportState;

use super::{format_elapsed, status_view_from, ColorKind, StatusView};

#[test]
fn disconnected_is_red_without_host() {
    let view = status_view_from(&TransportState::Disconnected, "", 0, None);
    assert_eq!(view.state, "Disconnected");
    assert_eq!(view.color, ColorKind::Red);
    assert_eq!(view.host, None);
    assert_eq!(view.elapsed_secs, None);
    assert_eq!(view.text(), "Disconnected");
}

#[test]
fn connecting_is_yellow() {
    let view = status_view_from(&TransportState::Connecting, "example.com", 22, None);
    assert_eq!(view.state, "Connecting…");
    assert_eq!(view.color, ColorKind::Yellow);
    assert_eq!(view.host.as_deref(), Some("example.com"));
    assert_eq!(view.elapsed_secs, None);
}

#[test]
fn verifying_host_key_is_yellow() {
    let view = status_view_from(&TransportState::VerifyingHostKey, "example.com", 22, None);
    assert_eq!(view.state, "Verifying host key…");
    assert_eq!(view.color, ColorKind::Yellow);
}

#[test]
fn authenticating_is_yellow() {
    let view = status_view_from(&TransportState::Authenticating, "example.com", 22, None);
    assert_eq!(view.state, "Authenticating…");
    assert_eq!(view.color, ColorKind::Yellow);
}

#[test]
fn connected_is_green_with_host_and_port() {
    let view = status_view_from(&TransportState::Connected, "example.com", 2222, Some(754));
    assert_eq!(view.state, "Connected to example.com:2222");
    assert_eq!(view.color, ColorKind::Green);
    assert_eq!(view.host.as_deref(), Some("example.com"));
    assert_eq!(view.elapsed_secs, Some(754));
    assert_eq!(view.text(), "Connected to example.com:2222 — 12:34");
}

#[test]
fn connected_without_host_shows_bare_state() {
    let view = status_view_from(&TransportState::Connected, "", 22, None);
    assert_eq!(view.state, "Connected");
    assert_eq!(view.color, ColorKind::Green);
    assert_eq!(view.text(), "Connected");
}

#[test]
fn reconnecting_reports_attempt_and_is_yellow() {
    let view = status_view_from(&TransportState::Reconnecting { attempt: 3 }, "h", 22, None);
    assert_eq!(view.state, "Reconnecting (attempt 3)…");
    assert_eq!(view.color, ColorKind::Yellow);
    assert_eq!(view.host.as_deref(), Some("h"));
}

#[test]
fn reconnecting_first_attempt_text() {
    let view = status_view_from(&TransportState::Reconnecting { attempt: 1 }, "h", 22, None);
    assert_eq!(view.state, "Reconnecting (attempt 1)…");
}

#[test]
fn error_passes_message_through_in_red() {
    let view = status_view_from(
        &TransportState::Error("connection failed: refused".to_string()),
        "example.com",
        22,
        None,
    );
    assert_eq!(view.state, "connection failed: refused");
    assert_eq!(view.color, ColorKind::Red);
    assert_eq!(view.text(), "connection failed: refused");
}

#[test]
fn non_connected_states_drop_elapsed() {
    for state in [
        TransportState::Disconnected,
        TransportState::Connecting,
        TransportState::VerifyingHostKey,
        TransportState::Authenticating,
        TransportState::Reconnecting { attempt: 2 },
        TransportState::Error("boom".to_string()),
    ] {
        let view = status_view_from(&state, "h", 22, Some(99));
        assert_eq!(
            view.elapsed_secs, None,
            "state {state:?} kept a stale duration"
        );
        assert_eq!(view.text(), view.state);
    }
}

#[test]
fn every_state_maps_to_a_distinct_color_and_text() {
    let states = [
        TransportState::Disconnected,
        TransportState::Connecting,
        TransportState::VerifyingHostKey,
        TransportState::Authenticating,
        TransportState::Connected,
        TransportState::Reconnecting { attempt: 1 },
        TransportState::Error("e".to_string()),
    ];
    let views: Vec<StatusView> = states
        .iter()
        .map(|s| status_view_from(s, "h", 22, None))
        .collect();

    let texts: std::collections::HashSet<&str> = views.iter().map(|v| v.state.as_str()).collect();
    assert_eq!(texts.len(), views.len());

    // Disconnected and Error are red, Connected green, the three
    // transient states plus Reconnecting yellow.
    for (i, expected) in [
        (0, ColorKind::Red),
        (1, ColorKind::Yellow),
        (2, ColorKind::Yellow),
        (3, ColorKind::Yellow),
        (4, ColorKind::Green),
        (5, ColorKind::Yellow),
        (6, ColorKind::Red),
    ] {
        assert_eq!(views[i].color, expected, "state {i} = {:?}", views[i]);
    }
}

#[test]
fn format_elapsed_seconds_and_minutes() {
    assert_eq!(format_elapsed(0), "0:00");
    assert_eq!(format_elapsed(1), "0:01");
    assert_eq!(format_elapsed(59), "0:59");
    assert_eq!(format_elapsed(60), "1:00");
    assert_eq!(format_elapsed(754), "12:34");
    assert_eq!(format_elapsed(3599), "59:59");
}

#[test]
fn format_elapsed_hours() {
    assert_eq!(format_elapsed(3600), "1:00:00");
    assert_eq!(format_elapsed(3661), "1:01:01");
    assert_eq!(format_elapsed(86_400), "24:00:00");
}

#[test]
fn color_kind_hex_colors_are_distinct() {
    let colors = [
        ColorKind::Green.hex(),
        ColorKind::Yellow.hex(),
        ColorKind::Red.hex(),
    ];
    let unique: std::collections::HashSet<&str> = colors.into_iter().collect();
    assert_eq!(unique.len(), 3);
    for color in unique {
        assert!(
            color.starts_with('#') && color.len() == 7,
            "{color} is not a #rrggbb colour"
        );
    }
}

#[test]
fn a_port_of_zero_shows_the_bare_description() {
    let view = status_view_from(&TransportState::Connected, "/bin/bash", 0, None);
    assert_eq!(view.text(), "Connected to /bin/bash");
    let view = status_view_from(&TransportState::Connected, "db", 22, None);
    assert_eq!(view.text(), "Connected to db:22");
}
