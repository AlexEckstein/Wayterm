use std::io::Write;
use std::process::{Command, Stdio};

use super::*;

#[test]
fn the_answer_is_the_last_marked_number() {
    assert_eq!(parse_uptime(b"wayterm-uptime:12345\n"), Some(12345));
    assert_eq!(
        parse_uptime(b"Welcome!\nwayterm-uptime:1\nwayterm-uptime:90\n"),
        Some(90),
        "startup files may print around it"
    );
    assert_eq!(parse_uptime(b"wayterm-uptime:\n"), None);
    assert_eq!(parse_uptime(b"wayterm-uptime:-5\n"), None);
    assert_eq!(parse_uptime(b"12345\n"), None, "unmarked");
    assert_eq!(parse_uptime(b""), None);
}

#[test]
fn proc_uptime_reads_its_whole_seconds() {
    assert_eq!(parse_proc_uptime("12345.67 890123.45\n"), Some(12345));
    assert_eq!(parse_proc_uptime("7 1\n"), Some(7));
    assert_eq!(parse_proc_uptime(""), None);
    assert_eq!(parse_proc_uptime("x.5 1\n"), None);
}

#[test]
fn this_machine_has_an_uptime() {
    if std::path::Path::new("/proc/uptime").exists() {
        assert!(local_uptime().is_some());
    }
}

/// `sh`'s stdout for `script`, with `path_dir` first on `PATH`.
fn run(script: &str, path_dir: Option<&std::path::Path>) -> Vec<u8> {
    let mut command = Command::new("sh");
    if let Some(dir) = path_dir {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", dir.display()));
    }
    let mut sh = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sh");
    sh.stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = sh.wait_with_output().expect("sh exits");
    assert!(output.status.success());
    output.stdout
}

/// The script is POSIX `sh`; on this (Linux) machine it reads
/// `/proc/uptime`.
#[test]
fn the_script_reports_this_machines_uptime() {
    let stdout = run(UPTIME_SCRIPT, None);
    let expected = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split('.').next()?.parse::<u64>().ok());
    let Some(expected) = expected else { return };
    let reported = parse_uptime(&stdout).expect("a marked answer");
    assert!(reported.abs_diff(expected) <= 2, "{reported} vs {expected}");
}

/// Without `/proc/uptime` the script asks `sysctl`, in either of the
/// forms macOS and the BSDs print.
#[test]
fn without_proc_the_script_reads_kern_boottime() {
    use std::os::unix::fs::PermissionsExt;

    let script = UPTIME_SCRIPT.replace("/proc/uptime", "/nonexistent/uptime");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let boot = now - 3600;
    for output in [
        format!("{{ sec = {boot}, usec = 123 }} Mon Nov 23 10:02:11 2026"),
        boot.to_string(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sysctl = dir.path().join("sysctl");
        std::fs::write(&sysctl, format!("#!/bin/sh\necho '{output}'\n")).unwrap();
        std::fs::set_permissions(&sysctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let reported = parse_uptime(&run(&script, Some(dir.path()))).expect(&output);
        assert!(reported.abs_diff(3600) <= 2, "{output}: {reported}");
    }

    let dir = tempfile::tempdir().unwrap();
    let sysctl = dir.path().join("sysctl");
    std::fs::write(&sysctl, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&sysctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(parse_uptime(&run(&script, Some(dir.path()))), None);
}
