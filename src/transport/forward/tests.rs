use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const EVENT_WAIT: Duration = Duration::from_secs(5);
const IO_WAIT: Duration = Duration::from_secs(5);

fn local_rule(bind_port: u16, remote_host: &str, remote_port: u16, enabled: bool) -> ForwardRule {
    ForwardRule {
        kind: ForwardKind::Local,
        bind_addr: "127.0.0.1".to_string(),
        bind_port,
        remote_host: remote_host.to_string(),
        remote_port,
        enabled,
    }
}

fn engine_with(opener: ChannelOpener) -> (ForwardEngine, mpsc::Receiver<ForwardEvent>) {
    let (tx, rx) = mpsc::channel(64);
    (ForwardEngine::with_opener(opener, tx), rx)
}

/// An opener whose "remote" is an in-process echo task over a duplex
/// pair. When `fail_opens` is set, opens fail (refused remote) instead.
fn echo_opener(fail_opens: Arc<AtomicU32>) -> ChannelOpener {
    Arc::new(move |_host: String, _port: u16| {
        let fail_opens = Arc::clone(&fail_opens);
        Box::pin(async move {
            if fail_opens.load(Ordering::SeqCst) > 0 {
                return Err(TransportError::ChannelError("refused".to_string()));
            }
            let (client_end, mut server_end) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match server_end.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if server_end.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Ok(Box::pin(client_end) as BoxStream)
        }) as OpenerFuture
    })
}

async fn next_event(rx: &mut mpsc::Receiver<ForwardEvent>) -> ForwardEvent {
    timeout(EVENT_WAIT, rx.recv())
        .await
        .expect("timed out waiting for a forward event")
        .expect("event channel closed")
}

/// A free loopback port, claimed by binding `:0` first (small reuse
/// race, acceptable in tests).
async fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn write_and_echo(port: u16, payload: &[u8]) -> Vec<u8> {
    let mut sock = timeout(IO_WAIT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .expect("connect timed out")
        .expect("listener refused the connection");
    timeout(IO_WAIT, sock.write_all(payload))
        .await
        .expect("write timed out")
        .expect("write failed");
    let mut out = vec![0u8; payload.len()];
    timeout(IO_WAIT, sock.read_exact(&mut out))
        .await
        .expect("echo timed out")
        .expect("echo read failed");
    out
}

#[tokio::test]
async fn local_forward_copies_both_directions() {
    let (engine, mut rx) = engine_with(echo_opener(Arc::new(AtomicU32::new(0))));
    let port = free_port().await;
    engine.start(vec![local_rule(port, "remote.test", 80, true)]);
    assert_eq!(
        next_event(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Listening,
        }
    );

    let payload = b"hello-forward";
    assert_eq!(write_and_echo(port, payload).await, payload);

    // The connection lived and died as counted transitions.
    assert_eq!(
        next_event(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Active { conns: 1 },
        }
    );
    assert_eq!(
        next_event(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Active { conns: 0 },
        }
    );
    engine.stop();
}

#[tokio::test]
async fn refused_remote_closes_one_conn_and_keeps_listening() {
    let fail_opens = Arc::new(AtomicU32::new(1));
    let (engine, mut rx) = engine_with(echo_opener(Arc::clone(&fail_opens)));
    let port = free_port().await;
    engine.start(vec![local_rule(port, "dead.test", 9, true)]);
    assert!(matches!(
        next_event(&mut rx).await.state,
        ForwardState::Listening
    ));

    // Refused remote: the server closes our socket cleanly (EOF).
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut buf = [0u8; 16];
    let n = timeout(IO_WAIT, sock.read(&mut buf))
        .await
        .expect("close timed out")
        .expect("read failed");
    assert_eq!(n, 0, "refused remote must EOF the accepted socket");
    let event = next_event(&mut rx).await;
    assert!(
        matches!(event.state, ForwardState::LastFailed { .. }),
        "failure must report LastFailed, got: {event:?}"
    );

    // The listener survived: flip the fake to serving and round-trip.
    fail_opens.store(0, Ordering::SeqCst);
    let payload = b"after-refusal";
    assert_eq!(write_and_echo(port, payload).await, payload);
    engine.stop();
}

#[tokio::test]
async fn stop_closes_the_listener() {
    let (engine, mut rx) = engine_with(echo_opener(Arc::new(AtomicU32::new(0))));
    let port = free_port().await;
    engine.start(vec![local_rule(port, "remote.test", 80, true)]);
    assert!(matches!(
        next_event(&mut rx).await.state,
        ForwardState::Listening
    ));

    engine.stop();
    assert_eq!(
        next_event(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Stopped,
        }
    );
    // Second stop is idempotent: no new events, no panic.
    engine.stop();
    assert!(
        timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "idempotent stop must not re-emit"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_err(),
        "stopped engine must not listen"
    );
}

#[tokio::test]
async fn set_enabled_toggles_a_rule_live() {
    let (engine, mut rx) = engine_with(echo_opener(Arc::new(AtomicU32::new(0))));
    let port = free_port().await;
    engine.start(vec![local_rule(port, "remote.test", 80, false)]);
    assert_eq!(
        next_event(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Stopped,
        }
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_err(),
        "disabled rule must not listen"
    );

    engine.set_enabled(0, true);
    assert!(matches!(
        next_event(&mut rx).await.state,
        ForwardState::Listening
    ));
    let payload = b"live-toggle";
    assert_eq!(write_and_echo(port, payload).await, payload);
    // Drain the Active transitions so later assertions see fresh events.
    assert!(matches!(
        next_event(&mut rx).await.state,
        ForwardState::Active { .. }
    ));
    assert!(matches!(
        next_event(&mut rx).await.state,
        ForwardState::Active { .. }
    ));

    engine.set_enabled(0, false);
    assert_eq!(
        next_event(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Stopped,
        }
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_err(),
        "disabling must close the listener"
    );

    // Unknown ids are ignored, never a panic.
    engine.set_enabled(99, true);
    engine.stop();
}

#[tokio::test]
async fn remote_and_dynamic_kinds_are_skipped() {
    let (engine, mut rx) = engine_with(echo_opener(Arc::new(AtomicU32::new(0))));
    let port = free_port().await;
    let skipped = |kind| ForwardRule {
        kind,
        ..local_rule(port, "remote.test", 80, true)
    };
    engine.start(vec![
        skipped(ForwardKind::Remote),
        skipped(ForwardKind::Dynamic),
    ]);
    for id in 0..2 {
        assert_eq!(
            next_event(&mut rx).await,
            ForwardEvent {
                rule_id: id,
                state: ForwardState::Stopped,
            }
        );
    }
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_err(),
        "v2 kinds must bind nothing"
    );
    engine.stop();
}

#[tokio::test]
async fn bind_failure_reports_last_failed() {
    let (engine, mut rx) = engine_with(echo_opener(Arc::new(AtomicU32::new(0))));
    let port = free_port().await;
    let _squat = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    engine.start(vec![local_rule(port, "remote.test", 80, true)]);
    let event = next_event(&mut rx).await;
    assert!(
        matches!(event.state, ForwardState::LastFailed { .. }),
        "taken port must report LastFailed, got: {event:?}"
    );
    engine.stop();
}

#[tokio::test]
async fn empty_bind_addr_means_loopback() {
    let (engine, mut rx) = engine_with(echo_opener(Arc::new(AtomicU32::new(0))));
    let port = free_port().await;
    let rule = ForwardRule {
        bind_addr: "   ".to_string(),
        ..local_rule(port, "remote.test", 80, true)
    };
    engine.start(vec![rule]);
    assert!(matches!(
        next_event(&mut rx).await.state,
        ForwardState::Listening
    ));
    let payload = b"loopback-bind";
    assert_eq!(write_and_echo(port, payload).await, payload);
    engine.stop();
}
