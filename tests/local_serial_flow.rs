//! Local sessions through the real tab wiring (doc 03 §Wiring): a
//! `LocalShell` config builds a `LocalTransport` over a real
//! `forkpty` bash, wired to a `FakeBackend` through the pump/bridge/drain.
//! Display-free: the drain runs on a private main context driven by hand.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::glib;
use tokio::sync::mpsc;

use wayterm::store::model::{SessionConfig, SessionType};
use wayterm::terminal::{FakeBackend, TerminalEvent, TerminalHandle};
use wayterm::transport::types::TransportEvent;
use wayterm::ui::terminal_tab::TerminalTab;

struct Flow {
    tab: TerminalTab,
    state_rx: mpsc::UnboundedReceiver<TransportEvent>,
    ctx: glib::MainContext,
    term_tx: mpsc::Sender<TerminalEvent>,
    backend: FakeBackend,
}

fn local_flow() -> Flow {
    let config = SessionConfig {
        session_type: SessionType::LocalShell,
        local_shell: Some("/bin/bash".into()),
        ..SessionConfig::default()
    };
    let ctx = glib::MainContext::new();
    let (term_tx, events) = mpsc::channel(256);
    let backend = FakeBackend::new();
    let terminal = TerminalHandle {
        backend: Arc::new(backend.clone()),
        events,
    };
    let (tab, state_rx) =
        TerminalTab::new(&config, None, terminal, &ctx, 24, 80).expect("local tab wires");
    Flow {
        tab,
        state_rx,
        ctx,
        term_tx,
        backend,
    }
}

/// Drive the main context (and, via the sleeps, the tokio tasks) until
/// `ready()`.
fn drive(ctx: &glib::MainContext, what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        ctx.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn written(backend: &FakeBackend) -> String {
    String::from_utf8_lossy(&backend.written()).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_tab_describes_its_shell_and_echoes_through_the_pump() {
    let mut flow = local_flow();
    assert_eq!(flow.tab.description(), "/bin/bash");

    // Quoted apart so the echoed command line cannot satisfy the match.
    flow.term_tx
        .send(TerminalEvent::DataOut(b"echo wayterm-'p5'-flow\n".to_vec()))
        .await
        .unwrap();
    drive(&flow.ctx, "echo round-trip", || {
        written(&flow.backend).contains("wayterm-p5-flow")
    });
    flow.tab.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shell_exiting_reaches_the_state_channel_as_session_ended() {
    let mut flow = local_flow();
    flow.term_tx
        .send(TerminalEvent::DataOut(b"exit\n".to_vec()))
        .await
        .unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = flow.state_rx.recv().await {
            match event {
                TransportEvent::SessionEnded => return true,
                // ConnectionLost would start the reconnect driver.
                TransportEvent::ConnectionLost => return false,
                _ => {}
            }
        }
        false
    })
    .await
    .expect("state event before timeout");
    assert!(ended, "exit must reach the state channel as SessionEnded");
    flow.tab.close().await;
}
