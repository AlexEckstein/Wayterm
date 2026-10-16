//! VNC engine: dial + handshake (`VncSession`), then a tokio task
//! (`VncEngine`) that owns the framebuffer and talks to the UI over channels
//! (doc 08 §VNC Path B, doc 04 §Threading model). Display-free: nothing here
//! touches GTK or cairo.
//!
//! ## vnc-rs 0.6.0 behavior this module is built around
//!
//! The crate's docs are thin; these facts come from its source.
//!
//! - **Updates are pull-only.** The crate sends one non-incremental request
//!   right after the handshake and never requests another by itself. Without
//!   our periodic `X11Event::Refresh` (an incremental whole-screen request)
//!   the screen freezes after the first frame. See [`REFRESH_INTERVAL`].
//! - **`poll_event` never blocks.** It is a `try_recv` on a 2-slot channel
//!   behind the client's async mutex. `recv_event` holds that mutex while
//!   waiting, which would stall `input()` and `close()`, so the engine polls.
//!   The 2-slot channel back-pressures the decoder: drain it promptly.
//! - **Pseudo-encodings must be requested.** A rect whose encoding is not
//!   in the client's list (Raw excepted) is a fatal decode error, so
//!   `DesktopSizePseudo` and `CursorPseudo` are in [`DEFAULT_ENCODINGS`].
//! - **Server hang-up is silent.** EOF mid-stream emits no `VncEvent::Error`;
//!   the event channel just closes and `poll_event` returns
//!   `ClientNotRunning`. The engine reports that as "server closed".
//! - **Errors arrive as `VncEvent::Error(String)`** after already-queued
//!   events (garbage message type, bad rect, bad encoding).
//! - **Auth failure is reported inconsistently.** RFB 3.8 returns
//!   `General(<server reason>)` (no `WrongPassword`); 3.3/3.7 return
//!   `WrongPassword`. [`VncError::AuthFailed`] covers both: a `General` after
//!   the password callback ran is treated as the server's rejection.
//! - **Passwords are ASCII bytes.** The crate takes `&str` and uses the first
//!   8 UTF-8 bytes as the DES key; non-ASCII passwords therefore don't match
//!   Latin-1 servers. The callback copies the secret into a `String` only to
//!   hand it to the crate, which gives no way to zeroize that copy.
//! - **No handshake timeout in the crate.** A server that accepts and stays
//!   silent hangs `try_start` forever; [`VncSession::connect`] bounds it.
//! - **Pixels:** under `PixelFormat::bgra()` each pixel is `[b, g, r, pad]`
//!   on the wire (`u32::from_le_bytes` -> `pad << 24 | r << 16 | g << 8 | b`).
//! - **Tight JPEG rects come out as `JpegImage` (undecoded JPEG bytes).** We
//!   never request a JPEG quality level, so servers should not send them;
//!   they are ignored if they do.
//! - **`SetCursor` carries cursor-shape pixels, not framebuffer content.**
//!   Writing them into the framebuffer (as the crate example does) paints
//!   the cursor into the desktop; the engine ignores them.
//! - There is no quality/compression knob in 0.6.0, so `vnc_options.quality`
//!   cannot be applied.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::vnc::{PixelFormat, VncClient, VncConnector, VncEncoding, VncEvent, X11Event};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::framebuffer::{PixelRect, VncPixels};

/// TCP connect plus the RFB handshake and authentication.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the engine asks the server for changes. vnc-rs 0.6.0 requests
/// only the first frame; every later update exists because of this request,
/// so this is the frame-rate ceiling, not a safety net. The server answers an
/// incremental request only when something changed, and repeated requests
/// coalesce server-side, so an idle desktop costs a few bytes per tick.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(30);

/// Event-drain period while idle (see [`POLL_BUSY`]).
const POLL_IDLE: Duration = Duration::from_millis(16);
/// Re-poll delay after a drain that found events: a multi-rect frame is
/// larger than the crate's 2-slot channel and must keep flowing.
const POLL_BUSY: Duration = Duration::from_millis(1);
/// Events handled per drain round before yielding to input and cancel.
const MAX_DRAIN: usize = 256;
/// Pending input events; a full channel drops the newest (see [`VncInput`]).
const INPUT_CAPACITY: usize = 256;
const UI_CAPACITY: usize = 256;

/// The [`VncUiEvent::Ended`] reason of an engine stopped by
/// [`VncEngine::cancel`]: the clean end, which the tab shows as a
/// disconnect rather than an error.
pub const CANCELLED: &str = "cancelled";

/// Encodings requested from the server, in preference order. Raw is always
/// present (RFC 6143 §7.5.2).
pub const DEFAULT_ENCODINGS: [VncEncoding; 6] = [
    VncEncoding::Tight,
    VncEncoding::Zrle,
    VncEncoding::CopyRect,
    VncEncoding::Raw,
    VncEncoding::DesktopSizePseudo,
    VncEncoding::CursorPseudo,
];

#[derive(Debug, Error)]
pub enum VncError {
    #[error("could not reach the server: {0}")]
    Io(#[from] std::io::Error),
    #[error("handshake timed out after {0:?}")]
    Timeout(Duration),
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("the server requires a password")]
    AuthRequired,
    #[error("authentication failed: {0}")]
    AuthFailed(String),
}

type AuthFuture = Pin<Box<dyn Future<Output = Result<String, ::vnc::VncError>> + Send + Sync>>;

/// A connected, authenticated RFB session that is not yet running.
pub struct VncSession {
    client: VncClient,
}

impl VncSession {
    /// Dial `host:port`, handshake and authenticate, bounded by
    /// [`HANDSHAKE_TIMEOUT`]. Never retries. `password` is consumed;
    /// `None` fails with [`VncError::AuthRequired`] if the server wants one.
    pub async fn connect(
        host: &str,
        port: u16,
        password: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<Self, VncError> {
        Self::connect_timeout(host, port, password, HANDSHAKE_TIMEOUT).await
    }

    /// [`connect`](Self::connect) with an explicit bound.
    pub async fn connect_timeout(
        host: &str,
        port: u16,
        password: Option<Zeroizing<Vec<u8>>>,
        timeout: Duration,
    ) -> Result<Self, VncError> {
        tokio::time::timeout(timeout, Self::dial(host, port, password))
            .await
            .map_err(|_| VncError::Timeout(timeout))?
    }

    async fn dial(
        host: &str,
        port: u16,
        password: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<Self, VncError> {
        let stream = TcpStream::connect((host, port)).await?;
        let attempted = Arc::new(AtomicBool::new(false));
        let mut connector = VncConnector::<_, AuthFuture>::new(stream);
        if let Some(password) = password {
            let attempted = Arc::clone(&attempted);
            connector = connector.set_auth_method(Box::pin(async move {
                attempted.store(true, Ordering::SeqCst);
                Ok(String::from_utf8_lossy(&password).into_owned())
            }));
        }
        for encoding in DEFAULT_ENCODINGS {
            connector = connector.add_encoding(encoding);
        }
        let client = connector
            .allow_shared(true)
            .set_pixel_format(PixelFormat::bgra())
            .build()
            .map_err(|e| map_connect_error(e, false))?
            .try_start()
            .await
            .map_err(|e| map_connect_error(e, attempted.load(Ordering::SeqCst)))?
            .finish()
            .map_err(|e| map_connect_error(e, false))?;
        Ok(Self { client })
    }

    /// Start the engine task on the current tokio runtime.
    pub fn spawn(self) -> VncEngine {
        let pixels = Arc::new(Mutex::new(VncPixels::new(0, 0)));
        let (ui_tx, events) = mpsc::channel(UI_CAPACITY);
        let (input, input_rx) = VncInput::new(INPUT_CAPACITY);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            self.client,
            Arc::clone(&pixels),
            input_rx,
            ui_tx,
            cancel.clone(),
        ));
        VncEngine {
            events,
            input,
            pixels,
            cancel,
            task,
        }
    }
}

fn map_connect_error(error: ::vnc::VncError, auth_attempted: bool) -> VncError {
    use ::vnc::VncError as E;
    match error {
        E::NoPassword => VncError::AuthRequired,
        E::WrongPassword => VncError::AuthFailed("wrong password".into()),
        E::General(reason) if auth_attempted => VncError::AuthFailed(reason),
        E::IoError(e) => VncError::Io(e),
        other => VncError::Handshake(other.to_string()),
    }
}

/// What the engine tells the UI. Everything but `Dirtied`/`Bell` is
/// delivered reliably; `Dirtied` is coalesced (one pending is enough).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VncUiEvent {
    /// Pixels changed; read them from [`VncEngine::pixels`].
    Dirtied,
    /// Desktop dimensions (initial size, or a mid-session resize).
    Resolved(u16, u16),
    Bell,
    /// Server clipboard text (Latin-1 per RFC 6143 §7.6.4).
    Clipboard(String),
    /// The engine stopped: error text, "server closed" or [`CANCELLED`].
    /// Always the last event.
    Ended(String),
}

/// Cloneable, non-blocking sender for client input. A full channel drops the
/// event and counts it, so the GTK main thread never waits on the engine.
#[derive(Clone)]
pub struct VncInput {
    tx: mpsc::Sender<X11Event>,
    dropped: Arc<AtomicU64>,
}

impl VncInput {
    fn new(capacity: usize) -> (Self, mpsc::Receiver<X11Event>) {
        let (tx, rx) = mpsc::channel(capacity);
        let input = Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        (input, rx)
    }

    /// Queue `event`; returns false (and counts a drop) if it was not queued.
    pub fn send(&self, event: X11Event) -> bool {
        let queued = self.tx.try_send(event).is_ok();
        if !queued {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queued
    }

    /// Events dropped so far because the channel was full or closed.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Handle to a running engine.
pub struct VncEngine {
    pub events: mpsc::Receiver<VncUiEvent>,
    pub input: VncInput,
    /// Engine-owned framebuffer; lock briefly to snapshot.
    pub pixels: Arc<Mutex<VncPixels>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl VncEngine {
    /// Ask the engine to stop; it closes the client and emits `Ended`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Cancel and wait for the task to finish.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Outcome of applying one server event to the framebuffer.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    /// Pixels changed.
    Dirty,
    /// Forward this event to the UI.
    Ui(VncUiEvent),
    /// Fatal: the engine must stop with this reason.
    Fatal(String),
    Ignored,
}

/// Mutate `pixels` for one server event. Pure and display-free so the
/// event-to-framebuffer mapping is testable without a socket.
fn apply_event(pixels: &mut VncPixels, event: VncEvent) -> Applied {
    match event {
        VncEvent::SetResolution(screen) => resolve(pixels, screen.width, screen.height),
        VncEvent::DesktopUpdate(update) => match update.layout {
            Some(layout) => resolve(pixels, layout.width, layout.height),
            None => Applied::Ignored,
        },
        VncEvent::RawImage(rect, bytes) => {
            match pixels.write_rect((rect.x, rect.y, rect.width, rect.height), &bytes) {
                Ok(()) => Applied::Dirty,
                Err(e) => Applied::Fatal(format!("bad framebuffer update: {e}")),
            }
        }
        VncEvent::Copy(dst, src) => {
            let rect = |r: ::vnc::Rect| -> PixelRect { (r.x, r.y, r.width, r.height) };
            pixels.copy(rect(dst), rect(src));
            Applied::Dirty
        }
        VncEvent::Bell => Applied::Ui(VncUiEvent::Bell),
        VncEvent::Text(text) => Applied::Ui(VncUiEvent::Clipboard(text)),
        VncEvent::Error(reason) => Applied::Fatal(reason),
        // JpegImage, SetCursor, SetPixelFormat and future variants
        // (`VncEvent` is non_exhaustive): see the module notes.
        _ => Applied::Ignored,
    }
}

/// Adopt the server's desktop size, unless it is beyond what the engine
/// holds: the size is the server's to pick, and allocating it unchecked
/// lets a hostile server exhaust memory with one message.
fn resolve(pixels: &mut VncPixels, width: u16, height: u16) -> Applied {
    if !VncPixels::fits(width, height) {
        return Applied::Fatal(format!(
            "server desktop {width}x{height} exceeds the supported maximum"
        ));
    }
    pixels.resize(width, height);
    Applied::Ui(VncUiEvent::Resolved(width, height))
}

async fn run(
    client: VncClient,
    pixels: Arc<Mutex<VncPixels>>,
    mut input_rx: mpsc::Receiver<X11Event>,
    ui_tx: mpsc::Sender<VncUiEvent>,
    cancel: CancellationToken,
) {
    let reason = drive(&client, &pixels, &mut input_rx, &ui_tx, &cancel).await;
    let _ = client.close().await;
    let _ = ui_tx.send(VncUiEvent::Ended(reason)).await;
}

/// Runs until the session ends; returns the reason.
async fn drive(
    client: &VncClient,
    pixels: &Mutex<VncPixels>,
    input_rx: &mut mpsc::Receiver<X11Event>,
    ui_tx: &mpsc::Sender<VncUiEvent>,
    cancel: &CancellationToken,
) -> String {
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let poll = tokio::time::sleep(POLL_IDLE);
    tokio::pin!(poll);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return CANCELLED.into(),
            event = input_rx.recv() => {
                let Some(event) = event else { return "input closed".into() };
                if let Err(e) = client.input(event).await {
                    return input_error(e);
                }
            }
            _ = refresh.tick() => {
                if let Err(e) = client.input(X11Event::Refresh).await {
                    return input_error(e);
                }
            }
            _ = &mut poll => {
                match drain(client, pixels, ui_tx, cancel).await {
                    Ok(got_events) => {
                        let wait = if got_events { POLL_BUSY } else { POLL_IDLE };
                        poll.as_mut().reset(tokio::time::Instant::now() + wait);
                    }
                    Err(reason) => return reason,
                }
            }
        }
    }
}

fn input_error(error: ::vnc::VncError) -> String {
    match error {
        ::vnc::VncError::ClientNotRunning => "server closed".into(),
        other => other.to_string(),
    }
}

/// Apply up to [`MAX_DRAIN`] queued server events. `Ok(true)` if any were
/// handled; `Err(reason)` when the session is over.
async fn drain(
    client: &VncClient,
    pixels: &Mutex<VncPixels>,
    ui_tx: &mpsc::Sender<VncUiEvent>,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let mut handled = 0;
    let mut dirty = false;
    let mut outcome = Ok(());
    while handled < MAX_DRAIN {
        let event = match client.poll_event().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(e) => {
                outcome = Err(input_error(e));
                break;
            }
        };
        handled += 1;
        let applied = {
            let mut pixels = pixels.lock().unwrap_or_else(|p| p.into_inner());
            apply_event(&mut pixels, event)
        };
        match applied {
            Applied::Dirty => dirty = true,
            Applied::Ui(event) => {
                if !emit(ui_tx, event, cancel).await {
                    outcome = Err("ui closed".into());
                    break;
                }
            }
            Applied::Fatal(reason) => {
                outcome = Err(reason);
                break;
            }
            Applied::Ignored => {}
        }
    }
    // Flush pixels the UI should see even when the session is ending.
    if dirty {
        let _ = ui_tx.try_send(VncUiEvent::Dirtied);
    }
    outcome.map(|()| handled > 0)
}

/// Reliable delivery; false if the UI side is gone or we were cancelled.
async fn emit(
    ui_tx: &mpsc::Sender<VncUiEvent>,
    event: VncUiEvent,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        sent = ui_tx.send(event) => sent.is_ok(),
        _ = cancel.cancelled() => false,
    }
}

#[cfg(test)]
mod tests;
