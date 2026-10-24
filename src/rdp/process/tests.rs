use super::*;
use std::time::Duration;

fn full_spec() -> RdpSpec {
    RdpSpec {
        host: "rdp.example.lan".to_string(),
        port: 3389,
        username: "ada".to_string(),
        domain: Some("CORP".to_string()),
        width: 1920,
        height: 1080,
        depth: Some(32),
        audio: true,
        clipboard: true,
        fonts: true,
        printer: true,
        compression: true,
        cert_arg: Some(format!(
            "{}4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b",
            crate::rdp::cert::CERT_ARG_PREFIX
        )),
    }
}

#[test]
fn argv_matches_the_contract() {
    let args = build_args(&full_spec(), Some(0x2a00007));
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
            "/printer",
            "/audio",
            "+compression",
            "/parent-window:44040199",
            "/cert:fingerprint:sha256:4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b",
        ]
    );
}

#[test]
fn unset_optionals_disappear_and_off_toggles_spell_out() {
    let mut spec = full_spec();
    spec.domain = None;
    spec.depth = None;
    spec.audio = false;
    spec.clipboard = false;
    spec.fonts = false;
    spec.printer = false;
    spec.compression = false;
    spec.cert_arg = None;
    let args = build_args(&spec, None);
    assert_eq!(
        args,
        vec![
            "/v:rdp.example.lan:3389",
            "/u:ada",
            "/w:1920",
            "/h:1080",
            "/from-stdin:force",
            "/sec:rdp:off",
            "-clipboard",
            "-fonts",
            "-compression",
        ]
    );
}

#[test]
fn legacy_rdp_security_is_always_refused() {
    // Without it a peer can skip TLS — and the pin — entirely.
    let mut spec = full_spec();
    for cert in [None, spec.cert_arg.clone()] {
        spec.cert_arg = cert;
        for xid in [None, Some(1)] {
            let args = build_args(&spec, xid);
            assert_eq!(args.iter().filter(|a| *a == SEC_RDP_OFF).count(), 1);
        }
    }
}

#[test]
fn parent_window_present_iff_embedded() {
    let spec = full_spec();
    assert!(build_args(&spec, Some(99))
        .iter()
        .any(|a| a == "/parent-window:99"));
    assert!(build_args(&spec, None)
        .iter()
        .all(|a| !a.starts_with("/parent-window")));
    // The verified flag name is /parent-window:, never -id suffixed.
    assert!(build_args(&spec, Some(99))
        .iter()
        .all(|a| !a.contains("parent-window-id")));
}

#[test]
fn cert_ignore_never_appears() {
    // Mutation check: if anyone swaps the fingerprint token for
    // /cert:ignore (or appends it), this goes red. The builder is the
    // last gate — only fingerprint-prefixed tokens pass; a hostile
    // cert_arg is dropped, never forwarded.
    for cert in [
        None,
        Some("/cert:fingerprint:sha256:abcd".to_string()),
        Some("/cert:ignore".to_string()),
    ] {
        let mut spec = full_spec();
        spec.cert_arg = cert;
        let args = build_args(&spec, Some(1));
        assert!(
            args.iter().all(|a| a != "/cert:ignore"),
            "argv must never carry /cert:ignore: {args:?}"
        );
    }
    // The hostile token is dropped entirely, not smuggled elsewhere.
    let mut spec = full_spec();
    spec.cert_arg = Some("/cert:ignore".to_string());
    let args = build_args(&spec, Some(1));
    assert!(args.iter().all(|a| !a.contains("ignore")));
}

#[test]
fn password_has_no_place_in_argv() {
    // The builder does not even take a password: the compiler enforces
    // what this test documents. Pin the FreeRDP password flag too, so a
    // future `/p:` smuggling goes red.
    let args = build_args(&full_spec(), Some(7));
    assert!(args.iter().all(|a| !a.starts_with("/p:")));
    assert!(!args.iter().any(|a| a.contains("s3cr3t")));
}

#[test]
fn version_parser_reads_parenthesized_triples() {
    assert_eq!(
        parse_version("This is FreeRDP version 3.15.0 (3.15.0)"),
        Some("3.15.0".to_string())
    );
    assert_eq!(
        parse_version("prefix (3.15.1) suffix"),
        Some("3.15.1".to_string())
    );
    assert_eq!(parse_version("no version here"), None);
    assert_eq!(parse_version("empty () parens"), None);
    assert_eq!(parse_version("(abc) not numeric"), None);
    assert_eq!(parse_version("unbalanced (3.15.0"), None);
}

#[test]
fn version_parser_falls_back_to_bare_version_and_ignores_hashes() {
    // Real `xfreerdp --version` shape: the triple is bare, the parens
    // hold a git revision that must NOT parse as a version.
    assert_eq!(
        parse_version("This is FreeRDP version 3.15.1 (2681e62)"),
        Some("3.15.1".to_string())
    );
    assert_eq!(parse_version("(2681e62) only a hash"), None);
    // A bare triple with no keyword is not a version report.
    assert_eq!(parse_version("just 3.15.1 floating"), None);
}

#[test]
fn only_freerdp_3_and_later_is_supported() {
    for supported in ["3.0.0", "3.15.0", "3.32.1", "4.0"] {
        assert!(is_supported_version(supported), "{supported}");
    }
    for unsupported in ["2.0.0", "2.11.7", "", "x.1", "-3.0"] {
        assert!(!is_supported_version(unsupported), "{unsupported}");
    }
}

#[test]
fn ring_keeps_the_last_200_lines_in_order() {
    let mut ring = ErrorRing::new();
    assert!(ring.is_empty());
    for i in 0..(RING_CAPACITY + 50) {
        ring.push(format!("line {i}"));
    }
    assert_eq!(ring.len(), RING_CAPACITY);
    let digest = ring.digest();
    assert_eq!(digest.len(), RING_CAPACITY);
    assert_eq!(digest[0], "line 50");
    assert_eq!(
        digest[RING_CAPACITY - 1],
        format!("line {}", RING_CAPACITY + 49)
    );
}

#[test]
fn connect_error_marker_detection() {
    let mut ring = ErrorRing::new();
    ring.push("a benign line".to_string());
    assert!(!ring.has_connect_error());
    ring.push("[WARN] ERRCONNECT_CONNECT_CANCELLED needlessly? no".to_string());
    assert!(ring.has_connect_error());
}

#[test]
fn exit_mapping_separates_code_from_signal() {
    use std::os::unix::process::ExitStatusExt;
    let ok = std::process::ExitStatus::from_raw(0);
    assert_eq!(
        exit_event(&ok),
        RdpProcessEvent::Ended {
            code: Some(0),
            signal: None
        }
    );
    let failed = std::process::ExitStatus::from_raw(3 << 8);
    assert_eq!(
        exit_event(&failed),
        RdpProcessEvent::Ended {
            code: Some(3),
            signal: None
        }
    );
    let killed = std::process::ExitStatus::from_raw(9);
    assert_eq!(
        exit_event(&killed),
        RdpProcessEvent::Ended {
            code: None,
            signal: Some(9)
        }
    );
}

#[test]
fn minimal_env_is_allowlisted() {
    for (key, _) in minimal_env() {
        assert!(ALLOWED_ENV.contains(&key.as_str()));
    }
    // The display needs its authorization cookie to admit the child.
    assert!(ALLOWED_ENV.contains(&"XAUTHORITY"));
}

#[test]
fn find_binary_prefers_xfreerdp3_and_skips_nonexec() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("xfreerdp");
    std::fs::write(&legacy, "#!/bin/sh\necho hi\n").unwrap();
    // Not executable yet: invisible to the search.
    let path = std::env::join_paths([dir.path()]).unwrap();
    assert_eq!(find_binary_in(Some(path.as_os_str())), None);

    make_executable(&legacy);
    let path = std::env::join_paths([dir.path()]).unwrap();
    assert_eq!(
        find_binary_in(Some(path.as_os_str())),
        Some("xfreerdp".to_string())
    );

    let modern = dir.path().join("xfreerdp3");
    std::fs::write(&modern, "#!/bin/sh\necho hi\n").unwrap();
    make_executable(&modern);
    assert_eq!(
        find_binary_in(Some(path.as_os_str())),
        Some("xfreerdp3".to_string())
    );
    assert_eq!(find_binary_in(None), None);
}

#[test]
fn probe_binary_reports_a_sane_shape_when_present() {
    // xfreerdp may not be installed: only pin the output SHAPE.
    if let Some((name, version)) = probe_binary() {
        assert!(["xfreerdp3", "xfreerdp"].contains(&name.as_str()));
        assert!(!version.is_empty());
    }
}

fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Write an executable sh script; returns its path.
fn fake_binary(dir: &tempfile::TempDir, body: &str) -> String {
    let path = dir.path().join("fake-xfreerdp");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    make_executable(&path);
    path.to_str().unwrap().to_string()
}

fn test_spec() -> RdpSpec {
    RdpSpec {
        host: "127.0.0.1".to_string(),
        port: 13389,
        username: "tester".to_string(),
        domain: None,
        width: 1024,
        height: 768,
        depth: None,
        audio: false,
        clipboard: true,
        fonts: true,
        printer: false,
        compression: true,
        cert_arg: None,
    }
}

/// Stop a shared, live session the way the tab does: abort the runner
/// (which holds the lock for the whole session), then terminate. Fails
/// if the child outlived the call or it took anywhere near the fake's
/// natural lifetime.
async fn abort_and_terminate(
    proc: &std::sync::Arc<tokio::sync::Mutex<RdpProcess>>,
    runner: tokio::task::JoinHandle<io::Result<()>>,
) {
    let started = std::time::Instant::now();
    runner.abort();
    let mut proc = tokio::time::timeout(Duration::from_secs(5), proc.lock())
        .await
        .expect("the aborted runner releases the lock");
    proc.terminate().await;
    let reaped = proc.child.as_mut().expect("spawned").try_wait().unwrap();
    assert!(reaped.is_some(), "terminate reaps the child");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn run_emits_connected_and_terminate_kills_the_live_child() {
    let dir = tempfile::tempdir().unwrap();
    // Reads ONE stdin line (the password), notes stderr, then idles.
    let bin = fake_binary(&dir, "read -r line\necho fake-ready >&2\nsleep 30");
    let proc = RdpProcess::new(bin.clone(), &test_spec(), None)
        .with_connected_delay(Duration::from_millis(100))
        .with_terminate_grace(Duration::from_secs(2));
    assert_eq!(proc.binary(), bin);
    assert_eq!(proc.argv()[0], "/v:127.0.0.1:13389");
    let proc = std::sync::Arc::new(tokio::sync::Mutex::new(proc));
    let (tx, mut rx) = mpsc::channel(8);
    let password = Zeroizing::new(b"not-a-real-secret".to_vec());
    let runner = {
        let proc = std::sync::Arc::clone(&proc);
        tokio::spawn(async move { proc.lock().await.run(&password, tx).await })
    };
    assert_eq!(rx.recv().await, Some(RdpProcessEvent::Connected));
    abort_and_terminate(&proc, runner).await;
    assert_eq!(rx.recv().await, None, "the aborted runner sends nothing");
}

#[tokio::test]
async fn run_against_failing_child_ends_without_connected() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_binary(
        &dir,
        "read -r line\necho ERRCONNECT_LOGON_FAILURE >&2\nexit 1",
    );
    let mut proc =
        RdpProcess::new(bin, &test_spec(), None).with_connected_delay(Duration::from_secs(60));
    let (tx, mut rx) = mpsc::channel(8);
    let password = Zeroizing::new(b"wrong".to_vec());
    proc.run(&password, tx).await.unwrap();
    assert!(matches!(
        rx.recv().await,
        Some(RdpProcessEvent::Ended { code: Some(1), .. })
    ));
    assert_eq!(rx.recv().await, None, "Ended is always last");
    assert!(proc
        .error_digest()
        .iter()
        .any(|l| l.contains("ERRCONNECT_LOGON_FAILURE")));
}

#[tokio::test]
async fn spawn_retries_past_transient_text_file_busy() {
    // Holding the binary open for writing makes every exec of it fail
    // with ETXTBSY; releasing it from a timer lets the retry loop in
    // run() succeed. Guards the fix for the flake seen under parallel
    // gate runs (spawn racing concurrent forks failed with
    // `ExecutableFileBusy`).
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_binary(&dir, "read -r line\necho hi >&2\nexit 3");
    let held = std::fs::OpenOptions::new().write(true).open(&bin).unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(held);
    });
    let mut proc =
        RdpProcess::new(bin, &test_spec(), None).with_connected_delay(Duration::from_secs(60));
    let (tx, mut rx) = mpsc::channel(8);
    let password = Zeroizing::new(b"pw".to_vec());
    proc.run(&password, tx).await.unwrap();
    assert!(matches!(
        rx.recv().await,
        Some(RdpProcessEvent::Ended { code: Some(3), .. })
    ));
    assert_eq!(rx.recv().await, None, "Ended is always last");
}

#[tokio::test]
async fn child_sees_minimal_env_and_never_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("env-dump");
    let bin = fake_binary(
        &dir,
        &format!(
            "read -r line\nenv | sort > {}\nsleep 30",
            dump.to_str().unwrap()
        ),
    );
    let proc = RdpProcess::new(bin, &test_spec(), None)
        .with_connected_delay(Duration::from_millis(50))
        .with_terminate_grace(Duration::from_secs(2));
    let (tx, mut rx) = mpsc::channel(8);
    let secret = "s3cr3t-that-must-not-leak";
    let password = Zeroizing::new(secret.as_bytes().to_vec());
    let proc = std::sync::Arc::new(tokio::sync::Mutex::new(proc));
    let runner = {
        let proc = std::sync::Arc::clone(&proc);
        tokio::spawn(async move { proc.lock().await.run(&password, tx).await })
    };
    assert_eq!(rx.recv().await, Some(RdpProcessEvent::Connected));
    abort_and_terminate(&proc, runner).await;
    let dumped = std::fs::read_to_string(&dump).unwrap();
    // The fake binary is a shebang script, so its interpreter adds
    // PWD/SHLVL/_ itself — those are not leaks. Everything else the
    // child sees must come from the allowlist.
    const INTERPRETER_ADDED: [&str; 3] = ["PWD", "SHLVL", "_"];
    assert!(!dumped.contains(secret), "password leaked into env");
    for (key, _) in std::env::vars() {
        if ALLOWED_ENV.contains(&key.as_str()) || INTERPRETER_ADDED.contains(&key.as_str()) {
            continue;
        }
        assert!(
            dumped
                .lines()
                .all(|l| l.split('=').next() != Some(key.as_str())),
            "parent env leaked into child: {key}"
        );
    }
    for line in dumped.lines() {
        let key = line.split('=').next().unwrap_or("");
        assert!(
            ALLOWED_ENV.contains(&key) || INTERPRETER_ADDED.contains(&key),
            "unexpected env key in child: {key}"
        );
    }
}
