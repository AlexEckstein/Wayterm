//! `SerialTransport` against a real TTY with no hardware: the slave side of
//! a PTY pair is a genuine tty device that `serialport` opens by path, and
//! the master side plays the far end of the cable.
//!
//! With `SERIAL_TEST_DEVICE` and `SERIAL_TEST_BAUD` set, the same checks
//! also run against real hardware (see `scripts/serial-loopback.sh`): with
//! `SERIAL_TEST_PEER` naming the second port of a two-port null-modem
//! loopback, that port is the far end; without it, the device needs a
//! TX→RX jumper and its own output must echo back. Unset, the hardware
//! test skips — a missing device never fails `cargo test`.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::timeout;

use wayterm::store::model::{SessionConfig, SessionType};
use wayterm::transport::serial::SerialTransport;
use wayterm::transport::types::{Transport, TransportEvent, TransportState};

const DEADLINE: Duration = Duration::from_secs(5);

/// A PTY pair: the master as a non-blocking `File`, the slave's path. The
/// slave fd stays open so the master never sees a hang-up.
struct Pty {
    master: File,
    _slave: OwnedFd,
    slave_path: String,
}

fn openpty() -> Pty {
    let (mut master, mut slave) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            name.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let slave_path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Pty {
        master: unsafe { File::from_raw_fd(master) },
        _slave: unsafe { OwnedFd::from_raw_fd(slave) },
        slave_path,
    }
}

fn serial_config(device: &str, baud: u32) -> SessionConfig {
    SessionConfig {
        session_type: SessionType::Serial,
        serial_device: Some(device.into()),
        serial_baud_rate: Some(baud),
        ..SessionConfig::default()
    }
}

/// Collect `DataReceived` until `expected` has arrived.
async fn expect_output(rx: &mut mpsc::Receiver<TransportEvent>, expected: &[u8]) {
    let mut seen = Vec::new();
    let collect = async {
        while !seen.windows(expected.len()).any(|w| w == expected) {
            match rx.recv().await {
                Some(TransportEvent::DataReceived(data)) => seen.extend(data),
                Some(TransportEvent::ConnectionLost) | None => panic!("connection lost"),
                Some(_) => {}
            }
        }
    };
    timeout(DEADLINE, collect)
        .await
        .unwrap_or_else(|_| panic!("no {expected:?} in output (saw {seen:?})"));
}

/// Read from the far end until `expected` arrives; `WouldBlock` (PTY
/// master) and `TimedOut` (serialport peer) both mean "nothing yet".
fn expect_far_end(far: &mut dyn Read, expected: &[u8]) {
    let deadline = Instant::now() + DEADLINE;
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    while !seen.windows(expected.len()).any(|w| w == expected) {
        assert!(
            Instant::now() < deadline,
            "far end never saw {expected:?} (saw {seen:?})"
        );
        match far.read(&mut buf) {
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("far end read: {e}"),
        }
    }
}

/// Output shows, input is taken, resize is a no-op, disconnect is clean.
/// `far` is the other end of the line; `None` means a TX→RX loopback.
async fn exercise(device: &str, baud: u32, far: Option<&mut (dyn ReadWrite + Send)>) {
    let (tx, mut rx) = mpsc::channel(64);
    let mut transport = SerialTransport::new(&serial_config(device, baud), tx);
    transport.connect().await.expect("connect");
    assert_eq!(transport.state(), TransportState::Connected);
    assert_eq!(transport.description(), format!("{device}@{baud}"));
    transport.resize(40, 120).await.expect("resize is a no-op");

    match far {
        Some(far) => {
            far.write_all(b"console says hi\r\n").unwrap();
            expect_output(&mut rx, b"console says hi").await;
            transport.write(b"typed input\r").await.expect("write");
            expect_far_end(far, b"typed input\r");
        }
        None => {
            transport.write(b"loopback probe").await.expect("write");
            expect_output(&mut rx, b"loopback probe").await;
        }
    }

    transport.disconnect().await.expect("disconnect");
    transport.disconnect().await.expect("disconnect twice");
    assert_eq!(transport.state(), TransportState::Disconnected);
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[tokio::test]
async fn pty_console_shows_output_and_takes_input() {
    let mut pty = openpty();
    let path = pty.slave_path.clone();
    exercise(&path, 115_200, Some(&mut pty.master)).await;
}

#[tokio::test]
async fn pty_reconnects_after_disconnect() {
    let mut pty = openpty();
    let path = pty.slave_path.clone();
    exercise(&path, 9_600, Some(&mut pty.master)).await;
    exercise(&path, 9_600, Some(&mut pty.master)).await;
}

#[tokio::test]
async fn hardware_device_shows_output_and_takes_input() {
    let (Ok(device), Ok(baud)) = (
        std::env::var("SERIAL_TEST_DEVICE"),
        std::env::var("SERIAL_TEST_BAUD"),
    ) else {
        println!("SERIAL_TEST_DEVICE/SERIAL_TEST_BAUD unset; skipping hardware serial test");
        return;
    };
    let baud: u32 = baud.parse().expect("SERIAL_TEST_BAUD must be a number");
    match std::env::var("SERIAL_TEST_PEER") {
        Ok(peer) => {
            let mut peer = serialport::new(peer, baud)
                .timeout(Duration::from_millis(100))
                .open()
                .expect("open SERIAL_TEST_PEER");
            exercise(&device, baud, Some(&mut peer)).await;
        }
        Err(_) => exercise(&device, baud, None).await,
    }
}

#[tokio::test]
async fn hang_up_is_connection_lost() {
    let pty = openpty();
    let (tx, mut rx) = mpsc::channel(64);
    let mut transport = SerialTransport::new(&serial_config(&pty.slave_path, 115_200), tx);
    transport.connect().await.expect("connect");
    drop(pty.master); // the far end goes away, like an unplugged adapter
    let lost = async {
        loop {
            match rx.recv().await {
                Some(TransportEvent::ConnectionLost) => break,
                Some(_) => {}
                None => panic!("event channel closed"),
            }
        }
    };
    timeout(DEADLINE, lost).await.expect("ConnectionLost");
    transport.disconnect().await.expect("disconnect");
}
