use super::*;
use crate::store::model::RdpOptions;

fn session() -> SessionConfig {
    SessionConfig {
        id: 7,
        name: "rdp-prod".to_string(),
        host: Some(" rdp.example.lan ".to_string()),
        port: None,
        username: Some(" ada ".to_string()),
        rdp_domain: Some(" CORP ".to_string()),
        rdp_width: None,
        rdp_height: None,
        rdp_color_depth: Some(16),
        rdp_compression: true,
        rdp_options: None,
        ..Default::default()
    }
}

#[test]
fn spec_maps_the_row_with_plan_defaults() {
    let spec = spec_from_session(&session(), None);
    assert_eq!(spec.host, "rdp.example.lan");
    assert_eq!(spec.port, 3389);
    assert_eq!(spec.username, "ada");
    assert_eq!(spec.domain.as_deref(), Some("CORP"));
    assert_eq!((spec.width, spec.height), (1024, 768));
    assert_eq!(spec.depth, Some(16));
    assert!(!spec.audio);
    assert!(spec.clipboard && spec.fonts && spec.printer && spec.compression);
    assert_eq!(spec.cert_arg, None);
}

#[test]
fn spec_honors_explicit_options_and_geometry() {
    let mut session = session();
    session.port = Some(3390);
    session.rdp_width = Some(1920);
    session.rdp_height = Some(1080);
    session.rdp_color_depth = None;
    session.rdp_compression = false;
    session.rdp_options = Some(RdpOptions {
        audio: true,
        clipboard: false,
        fonts: false,
        printer: false,
    });
    let spec = spec_from_session(&session, Some("token".to_string()));
    assert_eq!((spec.port, spec.width, spec.height), (3390, 1920, 1080));
    assert_eq!(spec.depth, None);
    assert!(spec.audio);
    assert!(!spec.clipboard && !spec.fonts && !spec.printer && !spec.compression);
    assert_eq!(spec.cert_arg.as_deref(), Some("token"));
}

#[test]
fn version_warning_fires_below_3_20() {
    for version in ["2.0.0", "3.5.1", "3.15.0", "3.19.9"] {
        assert!(warn_below(version), "{version}");
    }
    for version in ["3.20.0", "3.21.1", "4.0.0", "", "not-a-version", "3"] {
        assert!(!warn_below(version), "{version}");
    }
}

#[test]
fn cert_texts_carry_endpoint_and_fingerprints() {
    let fp = "a".repeat(64);
    let unknown = unknown_cert_text("rdp.example.lan", 3389, &fp);
    assert!(unknown.contains("rdp.example.lan:3389"));
    assert!(unknown.contains(&fp));
    let changed = changed_cert_text("stored", "seen", "rdp.example.lan", 3390);
    assert!(changed.contains("rdp.example.lan:3390"));
    assert!(changed.contains("stored") && changed.contains("seen"));
}

#[test]
fn newcomer_tracking_ignores_baseline_and_orphans() {
    assert_eq!(track_newcomer(&[], &[]), None);
    assert_eq!(track_newcomer(&[1, 2], &[1, 2]), None);
    assert_eq!(track_newcomer(&[], &[3, 5]), Some(5));
    assert_eq!(track_newcomer(&[1], &[1, 7]), Some(7));
    // An orphan in the baseline is never tracked, even when youngest.
    assert_eq!(track_newcomer(&[1, 9], &[1, 7, 9]), Some(7));
    assert_eq!(track_newcomer(&[1, 7, 9], &[1, 7, 9]), None);
}

#[test]
fn ended_message_names_the_cause_and_tails_stderr() {
    assert_eq!(
        ended_message(Some(3), None, &[]),
        "The RDP session ended (exit code 3)."
    );
    assert_eq!(
        ended_message(None, Some(9), &[]),
        "The RDP session ended (signal 9)."
    );
    let digest = vec!["first".to_string(), "ERRCONNECT_LOGON_FAILURE".to_string()];
    let message = ended_message(Some(1), None, &digest);
    assert!(message.contains("exit code 1") && message.contains("ERRCONNECT_LOGON_FAILURE"));
}
