//! Display-free state tests for the RDP tab (doc 08 §RDP): the pure
//! mapping the GTK handlers call, and the argv contract tie-in with P7-1.
//! No GTK widgets are built here. The live rig hook at the bottom is
//! `#[ignore]`-gated on `RDP_TEST_*`.

use std::time::Duration;

use wayterm::rdp::{build_args, CERT_ARG_PREFIX};
use wayterm::store::model::{RdpOptions, SessionConfig, SessionType};
use wayterm::ui::rdp_tab::{
    changed_cert_text, freerdp_too_old_text, spec_from_session, unknown_cert_text, warn_below,
    DEFAULT_PORT,
};

fn session() -> SessionConfig {
    SessionConfig {
        id: 11,
        name: "rdp-prod".to_string(),
        session_type: SessionType::RDP,
        host: Some("rdp.example.lan".to_string()),
        username: Some("ada".to_string()),
        rdp_domain: Some("CORP".to_string()),
        rdp_width: Some(1920),
        rdp_height: Some(1080),
        rdp_color_depth: Some(32),
        rdp_compression: true,
        rdp_options: Some(RdpOptions {
            audio: false,
            clipboard: true,
            fonts: true,
            printer: false,
        }),
        ..Default::default()
    }
}

#[test]
fn spec_defaults_to_3389_and_1024x768() {
    let session = SessionConfig {
        session_type: SessionType::RDP,
        host: Some(" h ".to_string()),
        username: Some(" u ".to_string()),
        ..Default::default()
    };
    assert_eq!(DEFAULT_PORT, 3389);
    let spec = spec_from_session(&session, None);
    assert_eq!((spec.host.as_str(), spec.port), ("h", 3389));
    assert_eq!(spec.username, "u");
    assert_eq!((spec.width, spec.height), (1024, 768));
    assert_eq!(spec.domain, None);
    assert_eq!(spec.depth, None);
    // Absent options: audio off, everything else on.
    assert!(!spec.audio);
    assert!(spec.clipboard && spec.fonts && spec.printer && spec.compression);
}

#[test]
fn spec_carries_row_fields_and_cert_token() {
    let token = format!("{CERT_ARG_PREFIX}{}", "b".repeat(64));
    let spec = spec_from_session(&session(), Some(token.clone()));
    assert_eq!((spec.host.as_str(), spec.port), ("rdp.example.lan", 3389));
    assert_eq!(spec.username, "ada");
    assert_eq!(spec.domain.as_deref(), Some("CORP"));
    assert_eq!((spec.width, spec.height), (1920, 1080));
    assert_eq!(spec.depth, Some(32));
    assert!(!spec.audio && !spec.printer);
    assert!(spec.clipboard && spec.fonts && spec.compression);
    assert_eq!(spec.cert_arg.as_deref(), Some(token.as_str()));
}

#[test]
fn spec_passes_explicit_audio_and_printer_through() {
    let mut session = session();
    session.rdp_options = Some(RdpOptions {
        audio: true,
        clipboard: false,
        fonts: false,
        printer: true,
    });
    session.rdp_compression = false;
    let spec = spec_from_session(&session, None);
    assert!(spec.audio && spec.printer);
    assert!(!spec.clipboard && !spec.fonts && !spec.compression);
}

#[test]
fn version_warning_fires_below_3_20_only() {
    for old in ["2.0.0", "3.5.1", "3.15.0", "3.19.9"] {
        assert!(warn_below(old), "{old}");
    }
    for new in ["3.20.0", "3.20", "4.0.0", "", "three.fifteen", "3"] {
        assert!(!warn_below(new), "{new}");
    }
}

#[test]
fn freerdp_2_is_refused_with_what_to_install() {
    for old in ["2.0.0", "2.11.7"] {
        assert!(!wayterm::rdp::is_supported_version(old), "{old}");
    }
    let text = freerdp_too_old_text("xfreerdp", "2.11.7");
    assert!(text.contains("xfreerdp is FreeRDP 2.11.7"), "{text}");
    assert!(text.contains("FreeRDP 3 or later"), "{text}");
    assert!(text.contains("xfreerdp3"), "{text}");
}

#[test]
fn unknown_cert_text_names_endpoint_and_fingerprint() {
    let fp = "c".repeat(64);
    let text = unknown_cert_text("rdp.example.lan", 3389, &fp);
    assert!(text.contains("rdp.example.lan:3389"), "{text}");
    assert!(text.contains(&fp), "{text}");
}

#[test]
fn changed_cert_text_names_both_fingerprints() {
    let text = changed_cert_text("stored-fp", "seen-fp", "rdp.example.lan", 3390);
    assert!(text.contains("rdp.example.lan:3390"), "{text}");
    assert!(
        text.contains("stored-fp") && text.contains("seen-fp"),
        "{text}"
    );
}

#[test]
fn spec_ties_into_the_p7_1_argv_contract() {
    // The mapping end to P7-1's pinned argv: flag names and order come from
    // `build_args` (notably `/parent-window:`, never `-id` suffixed).
    let spec = spec_from_session(&session(), None);
    let args = build_args(&spec, Some(0x200004));
    assert_eq!(
        args,
        vec![
            "/v:rdp.example.lan:3389",
            "/u:ada",
            "/d:CORP",
            "/w:1920",
            "/h:1080",
            "/bpp:32",
            "/from-stdin:force",
            "/sec:rdp:off",
            "+clipboard",
            "+fonts",
            "+compression",
            "/parent-window:2097156",
        ]
    );
    // Defaults row, top-level window: no bpp, no domain, no parent flag.
    let plain = spec_from_session(
        &SessionConfig {
            session_type: SessionType::RDP,
            host: Some("h".to_string()),
            username: Some("u".to_string()),
            ..Default::default()
        },
        None,
    );
    let args = build_args(&plain, None);
    assert!(args.iter().all(|a| !a.starts_with("/bpp:")));
    assert!(args.iter().all(|a| !a.starts_with("/parent-window")));
    assert!(args.iter().all(|a| !a.contains("parent-window-id")));
}

/// Real-server RDP test. Skips unless `RDP_TEST_HOST`, `RDP_TEST_PORT`,
/// `RDP_TEST_USER`, `RDP_TEST_PASSWORD` and `RDP_TEST_FINGERPRINT` (lowercase
/// hex, no colons) are set. Runs the pinned spec through a real `RdpProcess`
/// against the rig and expects the 15 s heuristic's `Connected`.
/// Password is read from the environment only — it never appears here.
#[tokio::test]
#[ignore = "needs the RDP rig; set RDP_TEST_* to run it"]
async fn rdp_view_live_against_rig() {
    use wayterm::rdp::{decide, CertDecision, RdpProcess, RdpProcessEvent};
    use zeroize::Zeroizing;

    let read = |name: &str| std::env::var(name).ok();
    let (Some(host), Some(port), Some(user), Some(password), Some(fingerprint)) = (
        read("RDP_TEST_HOST"),
        read("RDP_TEST_PORT"),
        read("RDP_TEST_USER"),
        read("RDP_TEST_PASSWORD"),
        read("RDP_TEST_FINGERPRINT"),
    ) else {
        println!(
            "Skipping RDP live test: set RDP_TEST_HOST, RDP_TEST_PORT, \
             RDP_TEST_USER, RDP_TEST_PASSWORD and RDP_TEST_FINGERPRINT."
        );
        return;
    };
    let port: u16 = port.parse().expect("RDP_TEST_PORT is a port");
    let session = SessionConfig {
        session_type: SessionType::RDP,
        host: Some(host),
        port: Some(port),
        username: Some(user),
        ..Default::default()
    };
    // The rig fingerprint must be pinned before spawn (TOFU pass-through).
    let token = match decide(None, Some(&fingerprint)) {
        CertDecision::AcceptAndRecord { fingerprint } => {
            format!("{CERT_ARG_PREFIX}{fingerprint}")
        }
        unexpected => panic!("TOFU decision for the rig fingerprint: {unexpected:?}"),
    };
    let (binary, version) =
        wayterm::rdp::probe_binary().expect("xfreerdp on PATH for the live test");
    println!("live against {binary} {version}");
    let spec = spec_from_session(&session, Some(token));
    let proc = std::sync::Arc::new(tokio::sync::Mutex::new(RdpProcess::new(
        binary, &spec, None,
    )));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let secret = Zeroizing::new(password.into_bytes());
    let runner = {
        let proc = std::sync::Arc::clone(&proc);
        tokio::spawn(async move { proc.lock().await.run(&secret, tx).await })
    };
    // The 15 s heuristic fires Connected while the session is up; then we
    // tear it down the way the tab does. `run` holds the process lock for
    // the whole session, so the runner is aborted before `terminate`.
    match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
        Ok(Some(RdpProcessEvent::Connected)) => {}
        other => panic!("expected Connected from the rig, got {other:?}"),
    }
    runner.abort();
    tokio::time::timeout(Duration::from_secs(15), async {
        proc.lock().await.terminate().await;
    })
    .await
    .expect("terminate finishes once the runner is aborted");
    assert_eq!(rx.recv().await, None, "the aborted runner sends nothing");
}
