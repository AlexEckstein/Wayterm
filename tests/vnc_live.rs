//! Real-server VNC test. Skips unless `VNC_TEST_HOST`, `VNC_TEST_PORT` and
//! `VNC_TEST_PASSWORD` are set (e.g. `x11vnc` serving an Xvfb display). It
//! is the real-world counterpart to the mock in `tests/vnc_wire.rs`.

use std::time::{Duration, Instant};

use wayterm::vnc::{ClientMouseEvent, VncSession, VncUiEvent, X11Event};
use zeroize::Zeroizing;

struct Fixture {
    host: String,
    port: u16,
    password: String,
}

/// The fixture, or `None` after printing the skip notice.
fn fixture() -> Option<Fixture> {
    let read = || {
        Some(Fixture {
            host: std::env::var("VNC_TEST_HOST").ok()?,
            port: std::env::var("VNC_TEST_PORT").ok()?.parse().ok()?,
            password: std::env::var("VNC_TEST_PASSWORD").ok()?,
        })
    };
    let fixture = read();
    if fixture.is_none() {
        println!(
            "Skipping VNC live test: set VNC_TEST_HOST, VNC_TEST_PORT and \
             VNC_TEST_PASSWORD to run it."
        );
    }
    fixture
}

#[tokio::test]
async fn connects_gets_a_frame_takes_input_and_closes() {
    let Some(f) = fixture() else { return };
    let password = Zeroizing::new(f.password.into_bytes());
    let mut engine = VncSession::connect(&f.host, f.port, Some(password))
        .await
        .expect("connect")
        .spawn();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dirtied = false;
    while !dirtied {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, engine.events.recv()).await {
            Ok(Some(VncUiEvent::Dirtied)) => dirtied = true,
            Ok(Some(VncUiEvent::Ended(reason))) => panic!("session ended early: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => panic!("no framebuffer update within 5 s"),
        }
    }

    let (w, h) = engine.pixels.lock().unwrap().dims();
    assert!(w > 0 && h > 0, "desktop is {w}x{h}");

    assert!(engine.input.send(X11Event::PointerEvent(ClientMouseEvent {
        position_x: w / 2,
        position_y: h / 2,
        bottons: 0,
    })));
    engine.shutdown().await;
}
