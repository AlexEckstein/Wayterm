use std::io::Write;
use std::process::{Command, Stdio};

use super::*;

#[test]
fn the_answer_is_the_last_marked_absolute_path() {
    assert_eq!(
        parse_probe_output(b"wayterm-cwd:/srv/my site\n").as_deref(),
        Some("/srv/my site")
    );
    assert_eq!(
        parse_probe_output(b"Welcome!\nwayterm-cwd:/old\nwayterm-cwd:/tmp\n").as_deref(),
        Some("/tmp"),
        "startup files may print around it"
    );
    assert_eq!(parse_probe_output(b"wayterm-cwd:relative\n"), None);
    assert_eq!(parse_probe_output(b"/tmp\n"), None, "unmarked");
    assert_eq!(parse_probe_output(b""), None);
}

#[test]
fn a_tmux_tab_asks_tmux_for_its_session_exactly() {
    assert_eq!(
        probe_script(Some("wayterm-prod")),
        "tmux display-message -p -t '=wayterm-prod:' '#{pane_current_path}' 2>/dev/null \
         | sed 's/^/wayterm-cwd:/'\n"
    );
    let script = probe_script(Some("wayterm-it's a.b:c"));
    assert!(
        script.contains(r"-t '=wayterm-it'\''s a_b_c:' "),
        "quoted, with tmux's renaming: {script}"
    );
}

#[test]
fn a_plain_tab_walks_proc_and_prints_the_marker() {
    let script = probe_script(None);
    assert_eq!(script, SHELL_SCRIPT);
    assert!(script.contains(&format!("printf '{MARKER}%s\\n'")));
}

/// The walk is POSIX `sh`: it must at least parse there.
#[test]
fn the_plain_shell_walk_parses_as_sh() {
    let mut sh = Command::new("sh")
        .arg("-n")
        .stdin(Stdio::piped())
        .spawn()
        .expect("sh");
    sh.stdin
        .take()
        .expect("stdin")
        .write_all(SHELL_SCRIPT.as_bytes())
        .expect("write");
    assert!(sh.wait().expect("sh exits").success());
}
