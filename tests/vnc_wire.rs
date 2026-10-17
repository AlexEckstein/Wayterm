//! Wire-level tests for the VNC engine: the real vnc-rs client and our engine
//! against the hand-rolled RFB server in `tests/common/vnc.rs`. Everything
//! runs on loopback and is deterministic, so it runs in CI.

#[path = "common/vnc.rs"]
mod vnc_mock;

use std::time::{Duration, Instant};

use tokio::sync::mpsc::Receiver;
use vnc_mock::{ClientMsg, Failure, MockConfig, MockServer, Update};
use wayterm::vnc::{
    ClientKeyEvent, ClientMouseEvent, VncEngine, VncError, VncPixels, VncSession, VncUiEvent,
    X11Event,
};
use zeroize::Zeroizing;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for "promptly": the engine polls every 16 ms.
const PROMPT: Duration = Duration::from_secs(2);
const PASSWORD: &str = "correct-horse";

fn secret(pw: &str) -> Option<Zeroizing<Vec<u8>>> {
    Some(Zeroizing::new(pw.as_bytes().to_vec()))
}

async fn connect(server: &MockServer, password: Option<&str>) -> Result<VncSession, VncError> {
    VncSession::connect("127.0.0.1", server.port(), password.and_then(secret)).await
}

async fn engine(config: MockConfig, password: Option<&str>) -> (MockServer, VncEngine) {
    let server = MockServer::start(config).await;
    let engine = connect(&server, password).await.expect("connect").spawn();
    (server, engine)
}

fn with_password() -> MockConfig {
    MockConfig {
        password: Some(PASSWORD.into()),
        ..MockConfig::default()
    }
}

/// Receive events until `pred` accepts one; panics on timeout.
async fn wait_event(
    events: &mut Receiver<VncUiEvent>,
    pred: impl Fn(&VncUiEvent) -> bool,
) -> VncUiEvent {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Some(event)) if pred(&event) => return event,
            Ok(Some(_)) => {}
            Ok(None) => panic!("event channel closed before the expected event"),
            Err(_) => panic!("timed out waiting for the expected event"),
        }
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn pixels(engine: &VncEngine) -> VncPixels {
    engine.pixels.lock().unwrap().clone()
}

fn px(p: &VncPixels, x: u16, y: u16) -> u32 {
    p.pixels()[usize::from(y) * usize::from(p.dims().0) + usize::from(x)] & 0x00FF_FFFF
}

fn input_msgs(server: &MockServer) -> Vec<ClientMsg> {
    server
        .recorded()
        .into_iter()
        .filter(|m| matches!(m, ClientMsg::Key { .. } | ClientMsg::Pointer { .. }))
        .collect()
}

// ---- handshake & authentication ------------------------------------------

#[tokio::test]
async fn security_none_round_trip_announces_size_and_our_settings() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| *e == VncUiEvent::Resolved(8, 6)).await;
    assert_eq!(pixels(&engine).dims(), (8, 6));
    assert_eq!(server.security_passed(), Some(true));

    wait_until("SetPixelFormat + SetEncodings + first request", || {
        server.recorded().len() >= 3
    })
    .await;
    let msgs = server.recorded();
    // Client-chosen BGRA32: 32bpp, depth 24, little endian, true colour,
    // max 255 each, shifts r16 g8 b0.
    assert_eq!(
        msgs[0],
        ClientMsg::SetPixelFormat([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])
    );
    // Tight, ZRLE, CopyRect, Raw, DesktopSize, Cursor.
    assert_eq!(
        msgs[1],
        ClientMsg::SetEncodings(vec![7, 16, 1, 0, -223, -239])
    );
    assert_eq!(
        msgs[2],
        ClientMsg::UpdateRequest {
            incremental: false,
            x: 0,
            y: 0,
            w: 8,
            h: 6
        }
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn vnc_auth_with_the_right_password_connects() {
    let (server, mut engine) = engine(with_password(), Some(PASSWORD)).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(8, 6))
    })
    .await;
    assert_eq!(server.security_passed(), Some(true));
    engine.shutdown().await;
}

#[tokio::test]
async fn every_rfb_version_negotiates_for_both_security_types() {
    for version in [b"RFB 003.003\n", b"RFB 003.007\n", b"RFB 003.008\n"] {
        for password in [None, Some(PASSWORD)] {
            let config = MockConfig {
                version: *version,
                password: password.map(Into::into),
                ..MockConfig::default()
            };
            let (server, mut engine) = engine(config, password).await;
            wait_event(&mut engine.events, |e| {
                matches!(e, VncUiEvent::Resolved(8, 6))
            })
            .await;
            assert_eq!(
                server.security_passed(),
                Some(true),
                "{} password={password:?}",
                String::from_utf8_lossy(version).trim()
            );
            engine.shutdown().await;
        }
    }
}

#[tokio::test]
async fn wrong_password_is_an_auth_failure_not_a_hang_and_is_not_retried() {
    // 3.8 sends a reason string; 3.3/3.7 close after the result word.
    for version in [b"RFB 003.008\n", b"RFB 003.007\n", b"RFB 003.003\n"] {
        let server = MockServer::start(MockConfig {
            version: *version,
            ..with_password()
        })
        .await;
        let started = Instant::now();
        let result = connect(&server, Some("wrong")).await;
        assert!(
            matches!(result, Err(VncError::AuthFailed(_))),
            "{}: {:?}",
            String::from_utf8_lossy(version).trim(),
            result.err()
        );
        assert!(started.elapsed() < PROMPT);
        assert_eq!(server.security_passed(), Some(false));
        // No retry loop of our own: still exactly one connection later.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(server.connections(), 1);
    }
}

#[tokio::test]
async fn auth_failure_text_never_contains_the_password() {
    let server = MockServer::start(with_password()).await;
    let err = connect(&server, Some("hunter2-wrong")).await.err().unwrap();
    assert!(!err.to_string().contains("hunter2-wrong"));
    assert!(!format!("{err:?}").contains("hunter2-wrong"));
}

#[tokio::test]
async fn missing_password_when_the_server_wants_one_is_auth_required() {
    let server = MockServer::start(with_password()).await;
    let result = connect(&server, None).await;
    assert!(
        matches!(result, Err(VncError::AuthRequired)),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn garbage_handshake_fails_fast() {
    let server = MockServer::start(MockConfig {
        failure: Failure::GarbageHandshake,
        ..MockConfig::default()
    })
    .await;
    let started = Instant::now();
    let result = connect(&server, None).await;
    assert!(
        matches!(result, Err(VncError::Handshake(_))),
        "{:?}",
        result.err()
    );
    assert!(started.elapsed() < PROMPT);
}

#[tokio::test]
async fn early_disconnect_fails_fast() {
    let server = MockServer::start(MockConfig {
        failure: Failure::EarlyDisconnect,
        ..MockConfig::default()
    })
    .await;
    let started = Instant::now();
    assert!(connect(&server, None).await.is_err());
    assert!(started.elapsed() < PROMPT);
}

#[tokio::test]
async fn a_silent_server_hits_the_handshake_timeout() {
    // Finding: vnc-rs 0.6.0 has no handshake timeout of its own; this is
    // the bound we add.
    let server = MockServer::start(MockConfig {
        failure: Failure::Stall,
        ..MockConfig::default()
    })
    .await;
    let started = Instant::now();
    let result =
        VncSession::connect_timeout("127.0.0.1", server.port(), None, Duration::from_millis(300))
            .await;
    assert!(
        matches!(result, Err(VncError::Timeout(_))),
        "{:?}",
        result.err()
    );
    assert!(started.elapsed() < PROMPT);
}

// ---- framebuffer pipeline -------------------------------------------------

#[tokio::test]
async fn raw_rect_lands_with_the_exact_pixel_values() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(8, 6))
    })
    .await;
    let colors = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0x0012_3456];
    server.send(Update::new().raw(2, 1, 2, 2, &colors).build());

    wait_until("raw rect pixels", || px(&pixels(&engine), 2, 1) == 0xFF0000).await;
    let p = pixels(&engine);
    assert_eq!(px(&p, 2, 1), 0xFF0000);
    assert_eq!(px(&p, 3, 1), 0x00FF00);
    assert_eq!(px(&p, 2, 2), 0x0000FF);
    assert_eq!(px(&p, 3, 2), 0x123456);
    // Opaque even though the wire pad byte was 0, and nothing else touched.
    assert_eq!(p.pixels()[usize::from(p.dims().0) + 2] >> 24, 0xFF);
    let painted = p.pixels().iter().filter(|v| (*v & 0xFFFFFF) != 0).count();
    assert_eq!(painted, 4);
    engine.shutdown().await;
}

#[tokio::test]
async fn dirtied_is_emitted_for_pixel_changes() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    server.send(Update::new().raw(0, 0, 1, 1, &[0x00AB_CDEF]).build());
    wait_event(&mut engine.events, |e| *e == VncUiEvent::Dirtied).await;
    assert_eq!(px(&pixels(&engine), 0, 0), 0xABCDEF);
    engine.shutdown().await;
}

#[tokio::test]
async fn copy_rect_moves_region_a_to_b() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(8, 6))
    })
    .await;
    let colors = [0x0000_0011, 0x0000_0022, 0x0000_0033, 0x0000_0044];
    server.send(Update::new().raw(0, 0, 2, 2, &colors).build());
    wait_until("source painted", || px(&pixels(&engine), 1, 1) == 0x44).await;

    server.send(Update::new().copy_rect(5, 3, 2, 2, 0, 0).build());
    wait_until("copy applied", || px(&pixels(&engine), 6, 4) == 0x44).await;
    let p = pixels(&engine);
    assert_eq!(
        [px(&p, 5, 3), px(&p, 6, 3), px(&p, 5, 4), px(&p, 6, 4)],
        [0x11, 0x22, 0x33, 0x44]
    );
    assert_eq!(px(&p, 0, 0), 0x11, "source stays");
    engine.shutdown().await;
}

#[tokio::test]
async fn desktop_size_resizes_preserving_overlap_and_announces_it() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(8, 6))
    })
    .await;
    server.send(Update::new().raw(1, 1, 1, 1, &[0x00AA_BBCC]).build());
    wait_until("pixel painted", || px(&pixels(&engine), 1, 1) == 0xAABBCC).await;

    // Grow.
    server.send(Update::new().desktop_size(12, 9).build());
    wait_event(&mut engine.events, |e| *e == VncUiEvent::Resolved(12, 9)).await;
    let p = pixels(&engine);
    assert_eq!(p.dims(), (12, 9));
    assert_eq!(px(&p, 1, 1), 0xAABBCC, "overlap preserved on grow");
    assert_eq!(px(&p, 11, 8), 0, "new area is black");

    // The server may now draw in the new area (crate screen bound moved).
    server.send(Update::new().raw(10, 8, 1, 1, &[0x0011_2233]).build());
    wait_until("pixel in grown area", || {
        px(&pixels(&engine), 10, 8) == 0x112233
    })
    .await;

    // Shrink.
    server.send(Update::new().desktop_size(4, 3).build());
    wait_event(&mut engine.events, |e| *e == VncUiEvent::Resolved(4, 3)).await;
    let p = pixels(&engine);
    assert_eq!(p.dims(), (4, 3));
    assert_eq!(px(&p, 1, 1), 0xAABBCC, "overlap preserved on shrink");
    engine.shutdown().await;
}

#[tokio::test]
async fn bell_and_server_clipboard_reach_the_ui() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    let mut bytes = vec![2]; // Bell
    bytes.extend_from_slice(&[3, 0, 0, 0, 0, 0, 0, 5]); // ServerCutText, len 5
    bytes.extend_from_slice(b"hello");
    server.send(bytes);
    wait_event(&mut engine.events, |e| *e == VncUiEvent::Bell).await;
    wait_event(&mut engine.events, |e| {
        *e == VncUiEvent::Clipboard("hello".into())
    })
    .await;
    engine.shutdown().await;
}

#[tokio::test]
async fn the_engine_keeps_asking_for_updates() {
    // Finding: vnc-rs 0.6.0 requests only the first frame; later frames
    // exist only because the engine sends incremental whole-screen
    // requests on a timer.
    let (server, engine) = engine(MockConfig::default(), None).await;
    wait_until("several incremental requests", || {
        server
            .recorded()
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ClientMsg::UpdateRequest {
                        incremental: true,
                        x: 0,
                        y: 0,
                        w: 8,
                        h: 6
                    }
                )
            })
            .count()
            >= 3
    })
    .await;
    engine.shutdown().await;
}

// ---- client -> server input -----------------------------------------------

#[tokio::test]
async fn pointer_and_key_events_reach_the_server_bit_for_bit() {
    let (server, engine) = engine(MockConfig::default(), None).await;
    let pointer = |x, y, bottons| {
        X11Event::PointerEvent(ClientMouseEvent {
            position_x: x,
            position_y: y,
            bottons,
        })
    };
    let key = |keycode, down| X11Event::KeyEvent(ClientKeyEvent { keycode, down });
    let sent = [
        pointer(0, 0, 0),
        pointer(3, 5, 1),     // left down
        pointer(3, 5, 0),     // left up
        pointer(7, 2, 4),     // right
        pointer(65535, 1, 2), // middle, u16 extreme
        pointer(4, 4, 8),     // scroll up
        pointer(4, 4, 16),    // scroll down
        key(0xFFE1, true),    // Shift_L
        key(0x61, true),      // a
        key(0x61, false),
        key(0xFFE1, false),
        key(0xFF0D, true), // Return
    ];
    for event in sent.clone() {
        assert!(engine.input.send(event));
    }
    wait_until("all input recorded", || {
        input_msgs(&server).len() == sent.len()
    })
    .await;
    assert_eq!(
        input_msgs(&server),
        vec![
            ClientMsg::Pointer {
                buttons: 0,
                x: 0,
                y: 0
            },
            ClientMsg::Pointer {
                buttons: 1,
                x: 3,
                y: 5
            },
            ClientMsg::Pointer {
                buttons: 0,
                x: 3,
                y: 5
            },
            ClientMsg::Pointer {
                buttons: 4,
                x: 7,
                y: 2
            },
            ClientMsg::Pointer {
                buttons: 2,
                x: 65535,
                y: 1
            },
            ClientMsg::Pointer {
                buttons: 8,
                x: 4,
                y: 4
            },
            ClientMsg::Pointer {
                buttons: 16,
                x: 4,
                y: 4
            },
            ClientMsg::Key {
                down: true,
                key: 0xFFE1
            },
            ClientMsg::Key {
                down: true,
                key: 0x61
            },
            ClientMsg::Key {
                down: false,
                key: 0x61
            },
            ClientMsg::Key {
                down: false,
                key: 0xFFE1
            },
            ClientMsg::Key {
                down: true,
                key: 0xFF0D
            },
        ]
    );
    assert_eq!(engine.input.dropped(), 0);
    engine.shutdown().await;
}

// ---- failure after the handshake -------------------------------------------

#[tokio::test]
async fn garbage_after_the_handshake_ends_the_session_promptly() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(..))
    })
    .await;
    let started = Instant::now();
    server.send(vec![0xFF, 0xEE, 0xDD, 0xCC]);
    let ended = wait_event(&mut engine.events, |e| matches!(e, VncUiEvent::Ended(_))).await;
    assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
    let VncUiEvent::Ended(reason) = ended else {
        unreachable!()
    };
    assert!(!reason.is_empty());
    // Ended is the last event.
    assert!(engine.events.recv().await.is_none());
}

#[tokio::test]
async fn server_dropping_the_connection_ends_the_session_promptly() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(..))
    })
    .await;
    let started = Instant::now();
    server.close();
    let ended = wait_event(&mut engine.events, |e| matches!(e, VncUiEvent::Ended(_))).await;
    assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
    assert_eq!(ended, VncUiEvent::Ended("server closed".into()));
}

#[tokio::test]
async fn malformed_update_with_a_bad_rect_count_ends_the_session() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(..))
    })
    .await;
    // Claims 3 rects, delivers 1, then hangs up.
    let update = Update::new().raw(0, 0, 1, 1, &[0x00FF_FFFF]);
    server.send(update.build_claiming(3));
    server.close();
    let started = Instant::now();
    wait_event(&mut engine.events, |e| matches!(e, VncUiEvent::Ended(_))).await;
    assert!(started.elapsed() < PROMPT);
}

#[tokio::test]
async fn a_rect_outside_the_desktop_ends_the_session_without_touching_pixels() {
    let (server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(..))
    })
    .await;
    // 8x6 desktop; this rect spills out of it.
    server.send(Update::new().raw(7, 5, 2, 2, &[0x00FF_FFFF; 4]).build());
    wait_event(&mut engine.events, |e| matches!(e, VncUiEvent::Ended(_))).await;
    let p = pixels(&engine);
    assert!(
        p.pixels().iter().all(|v| v & 0xFFFFFF == 0),
        "a lying server painted something"
    );
}

#[tokio::test]
async fn cancel_closes_the_session_and_ends_with_cancelled() {
    let (_server, mut engine) = engine(MockConfig::default(), None).await;
    wait_event(&mut engine.events, |e| {
        matches!(e, VncUiEvent::Resolved(..))
    })
    .await;
    engine.cancel();
    let ended = wait_event(&mut engine.events, |e| matches!(e, VncUiEvent::Ended(_))).await;
    assert_eq!(
        ended,
        VncUiEvent::Ended(wayterm::vnc::client::CANCELLED.into())
    );
    engine.shutdown().await;
}
