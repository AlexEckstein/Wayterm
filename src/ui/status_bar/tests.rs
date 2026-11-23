//! Unit tests for the display-free segments of [`super::StatusBar`].

use crate::store::model::SessionType;
use crate::transport::traffic::Savings;
use crate::transport::types::TransportState;

use super::{
    ColorKind, SessionInfo, elapsed_segment, endpoint_segment, format_elapsed, format_uptime,
    savings_segment, state_segment, uptime_segment,
};

fn ssh_session() -> SessionInfo {
    SessionInfo {
        name: "prod".to_string(),
        kind: SessionType::SSH,
        user: Some("alex".to_string()),
        jump: Some("bastion".to_string()),
    }
}

#[test]
fn disconnected_is_red() {
    let (color, segment) = state_segment(&TransportState::Disconnected, "", 0);
    assert_eq!(color, ColorKind::Red);
    assert_eq!(segment.text, "Disconnected");
    assert_eq!(segment.tooltip, "Disconnected");
    let (_, segment) = state_segment(&TransportState::Disconnected, "db", 22);
    assert_eq!(segment.tooltip, "Not connected to db:22");
}

#[test]
fn the_transient_states_are_yellow_and_name_the_endpoint() {
    for (state, text, tooltip) in [
        (
            TransportState::Connecting,
            "Connecting…",
            "Connecting to example.com:22",
        ),
        (
            TransportState::VerifyingHostKey,
            "Verifying host key…",
            "Checking the host key of example.com:22",
        ),
        (
            TransportState::Authenticating,
            "Authenticating…",
            "Logging in to example.com:22",
        ),
        (
            TransportState::Reconnecting { attempt: 3 },
            "Reconnecting (attempt 3)…",
            "Connection lost, reconnecting to example.com:22, attempt 3",
        ),
    ] {
        let (color, segment) = state_segment(&state, "example.com", 22);
        assert_eq!(color, ColorKind::Yellow, "{state:?}");
        assert_eq!(segment.text, text);
        assert_eq!(segment.tooltip, tooltip);
    }
}

#[test]
fn connected_is_green_and_leaves_the_endpoint_to_its_segment() {
    let (color, segment) = state_segment(&TransportState::Connected, "example.com", 2222);
    assert_eq!(color, ColorKind::Green);
    assert_eq!(segment.text, "Connected");
    assert_eq!(segment.tooltip, "Connected to example.com:2222");
    let (_, segment) = state_segment(&TransportState::Connected, "", 22);
    assert_eq!(segment.tooltip, "Connected");
}

#[test]
fn an_error_shows_its_whole_message_in_the_tooltip() {
    let message = "connection failed: refused".to_string();
    let (color, segment) = state_segment(&TransportState::Error(message.clone()), "h", 22);
    assert_eq!(color, ColorKind::Red);
    assert_eq!(segment.text, message);
    assert_eq!(segment.tooltip, message);
}

#[test]
fn every_state_has_its_own_text() {
    let texts: std::collections::HashSet<String> = [
        TransportState::Disconnected,
        TransportState::Connecting,
        TransportState::VerifyingHostKey,
        TransportState::Authenticating,
        TransportState::Connected,
        TransportState::Reconnecting { attempt: 1 },
        TransportState::Error("e".to_string()),
    ]
    .iter()
    .map(|state| state_segment(state, "h", 22).1.text)
    .collect();
    assert_eq!(texts.len(), 7);
}

#[test]
fn no_endpoint_no_segment() {
    assert_eq!(endpoint_segment("", 22, Some(&ssh_session())), None);
}

#[test]
fn an_ssh_endpoint_lists_the_session_behind_it() {
    let segment = endpoint_segment("db.internal", 22, Some(&ssh_session())).unwrap();
    assert_eq!(segment.text, "db.internal:22");
    assert_eq!(
        segment.tooltip,
        "Session: prod\nType: SSH\nHost: db.internal\nPort: 22\nUser: alex\nJump host: bastion"
    );
}

#[test]
fn local_and_serial_endpoints_are_their_descriptions() {
    let local = SessionInfo {
        name: "Local shell".to_string(),
        kind: SessionType::LocalShell,
        user: Some("ignored".to_string()),
        jump: None,
    };
    let segment = endpoint_segment("/bin/zsh", 0, Some(&local)).unwrap();
    assert_eq!(segment.text, "/bin/zsh");
    assert_eq!(
        segment.tooltip,
        "Session: Local shell\nType: Local shell\nShell: /bin/zsh"
    );

    let serial = SessionInfo {
        name: "console".to_string(),
        kind: SessionType::Serial,
        ..SessionInfo::default()
    };
    let segment = endpoint_segment("/dev/ttyUSB0@115200", 0, Some(&serial)).unwrap();
    assert_eq!(segment.text, "/dev/ttyUSB0@115200");
    assert!(
        segment
            .tooltip
            .ends_with("Type: Serial\nLine: /dev/ttyUSB0@115200")
    );
}

#[test]
fn an_endpoint_without_a_session_shows_host_and_port() {
    let segment = endpoint_segment("vnc.lan", 5900, None).unwrap();
    assert_eq!(segment.text, "vnc.lan:5900");
    assert_eq!(segment.tooltip, "Host: vnc.lan\nPort: 5900");
}

#[test]
fn the_elapsed_tooltip_names_the_start() {
    let segment = elapsed_segment(754, "14:02:11");
    assert_eq!(segment.text, "12:34");
    assert_eq!(segment.tooltip, "Connected since 14:02:11");
    assert_eq!(elapsed_segment(0, "").tooltip, "Time connected");
}

#[test]
fn the_savings_tooltip_shows_the_bytes() {
    let segment = savings_segment(Savings {
        percent: 53,
        wire: 1024 * 1024,
        uncompressed: 2252 * 1024,
    });
    assert_eq!(segment.text, "Compression saves 53%");
    assert_eq!(
        segment.tooltip,
        "1.0 MB received, about 2.2 MB uncompressed\n\
         Counts the terminal, SFTP and forwards on this connection"
    );
}

#[test]
fn uptime_reads_in_the_two_largest_units() {
    assert_eq!(format_uptime(0), "0m");
    assert_eq!(format_uptime(59 * 60 + 59), "59m");
    assert_eq!(format_uptime(3600), "1h 0m");
    assert_eq!(format_uptime(5 * 3600 + 12 * 60), "5h 12m");
    assert_eq!(format_uptime(86_400), "1d 0h");
    assert_eq!(format_uptime(12 * 86_400 + 4 * 3600 + 7 * 60), "12d 4h");
}

#[test]
fn the_uptime_tooltip_spells_out_the_duration_and_boot() {
    let segment = uptime_segment(12 * 86_400 + 4 * 3600 + 7 * 60, "11/11/26 10:02:11");
    assert_eq!(segment.text, "Up 12d 4h");
    assert_eq!(
        segment.tooltip,
        "Server up 12 days, 4 hours, 7 minutes\nBooted 11/11/26 10:02:11"
    );
    assert_eq!(
        uptime_segment(86_400 + 60, "").tooltip,
        "Server up 1 day, 1 minute"
    );
    assert_eq!(
        uptime_segment(30, "").tooltip,
        "Server up less than a minute"
    );
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
