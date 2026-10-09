//! `LocalTransport` against a real `forkpty` + `/bin/bash` (doc 04
//! §Local and serial transports; doc 10 Phase 5: a local shell runs
//! commands).

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use wayterm::store::model::{SessionConfig, SessionType};
use wayterm::transport::local::LocalTransport;
use wayterm::transport::types::{Transport, TransportEvent, TransportState};

const WAIT: Duration = Duration::from_secs(10);

fn bash() -> (LocalTransport, mpsc::Receiver<TransportEvent>) {
    let config = SessionConfig {
        session_type: SessionType::LocalShell,
        local_shell: Some("/bin/bash".into()),
        ..SessionConfig::default()
    };
    let (tx, rx) = mpsc::channel(256);
    (LocalTransport::new(&config, tx, 24, 80), rx)
}

/// Accumulate output until `matches` accepts it; returns the output.
async fn output_until(
    rx: &mut mpsc::Receiver<TransportEvent>,
    mut matches: impl FnMut(&str) -> bool,
) -> String {
    let mut seen = String::new();
    let found = timeout(WAIT, async {
        while !matches(&seen) {
            match rx.recv().await {
                Some(TransportEvent::DataReceived(data)) => {
                    seen.push_str(&String::from_utf8_lossy(&data))
                }
                Some(_) => {}
                None => panic!("event channel closed; output so far: {seen:?}"),
            }
        }
    })
    .await;
    assert!(found.is_ok(), "timed out; output so far: {seen:?}");
    seen
}

/// The first value the shell printed between `start` and `end`. The echoed
/// command line holds the same markers around an unexpanded `$…`, so any
/// value containing `$` is skipped.
fn marked<'a>(seen: &'a str, start: &str, end: &str) -> Option<&'a str> {
    seen.match_indices(start).find_map(|(i, _)| {
        let rest = &seen[i + start.len()..];
        let value = &rest[..rest.find(end)?];
        (!value.contains('$')).then_some(value)
    })
}

#[tokio::test]
async fn echo_output_arrives_as_data_received() {
    let (mut t, mut rx) = bash();
    t.connect().await.expect("connect");
    assert_eq!(t.state(), TransportState::Connected);
    assert_eq!(t.description(), "/bin/bash");

    // Quoted apart so the echoed command line cannot satisfy the match.
    t.write(b"echo wayterm-'p5'-local\n").await.expect("write");
    output_until(&mut rx, |s| s.contains("wayterm-p5-local")).await;
    t.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn resize_reaches_the_child() {
    let (mut t, mut rx) = bash();
    t.connect().await.expect("connect");
    t.resize(37, 113).await.expect("resize");

    t.write(b"echo \"size=$(stty size).\"\n")
        .await
        .expect("write");
    let seen = output_until(&mut rx, |s| marked(s, "size=", ".").is_some()).await;
    assert_eq!(
        marked(&seen, "size=", "."),
        Some("37 113"),
        "output: {seen:?}"
    );
    t.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn exit_reports_connection_lost_and_reaps_the_child() {
    let (mut t, mut rx) = bash();
    t.connect().await.expect("connect");

    t.write(b"echo \"pid=$$.\"\n").await.expect("write");
    let seen = output_until(&mut rx, |s| marked(s, "pid=", ".").is_some()).await;
    let pid: libc::pid_t = marked(&seen, "pid=", ".")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {seen:?}"));

    t.write(b"exit\n").await.expect("write exit");
    let lost = timeout(WAIT, async {
        loop {
            match rx.recv().await {
                Some(TransportEvent::ConnectionLost) => break,
                Some(_) => {}
                None => panic!("event channel closed before ConnectionLost"),
            }
        }
    })
    .await;
    assert!(lost.is_ok(), "no ConnectionLost after exit");

    // The reader reaps before reporting: the pid is no longer our child.
    let mut status = 0;
    // SAFETY: a non-blocking waitpid with a valid status pointer.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(
        (r, errno),
        (-1, Some(libc::ECHILD)),
        "child {pid} not reaped"
    );

    let state = timeout(WAIT, async {
        while t.state() != TransportState::Disconnected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(state.is_ok(), "state stayed {:?}", t.state());
    t.disconnect().await.expect("disconnect after exit");
}
