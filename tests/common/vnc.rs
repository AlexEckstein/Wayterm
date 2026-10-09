//! Hand-rolled RFB server, the wire-verification fixture shared by the vnc
//! suites (`tests/vnc_wire.rs`). It speaks real RFC 6143 bytes over a real
//! `TcpListener` on loopback, so it is deterministic in CI and needs no VNC
//! server install. Suites include it with
//! `#[path = "common/vnc.rs"] mod vnc_mock;`.
//!
//! Test-only credentials (`mock-pass` etc.) are fake fixtures. DES comes from
//! the dev-dependency `des` crate, independent of vnc-rs's own DES, so a
//! shared bug cannot cancel out.

// Each suite uses a different subset.
#![allow(dead_code)]

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockEncrypt, KeyInit};
use des::Des;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// How the server misbehaves. `None` is a well-behaved server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    None,
    /// Send non-RFB bytes instead of the version string.
    GarbageHandshake,
    /// Accept and immediately close.
    EarlyDisconnect,
    /// Accept, read, never answer (0-byte stall).
    Stall,
}

#[derive(Clone, Debug)]
pub struct MockConfig {
    pub width: u16,
    pub height: u16,
    pub name: String,
    /// `None` = security type None; `Some(pw)` = VNC auth with that password.
    pub password: Option<String>,
    /// Version the server announces: `*b"RFB 003.008\n"` etc.
    pub version: [u8; 12],
    pub failure: Failure,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            width: 8,
            height: 6,
            name: "wayterm-mock".into(),
            password: None,
            version: *b"RFB 003.008\n",
            failure: Failure::None,
        }
    }
}

/// A message the client sent, as the server parsed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMsg {
    SetPixelFormat([u8; 16]),
    SetEncodings(Vec<i32>),
    UpdateRequest {
        incremental: bool,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
    },
    Key {
        down: bool,
        key: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    CutText(String),
}

enum Cmd {
    Send(Vec<u8>),
    Close,
}

pub struct MockServer {
    pub addr: SocketAddr,
    cmd: mpsc::UnboundedSender<Cmd>,
    recorded: Arc<Mutex<Vec<ClientMsg>>>,
    connections: Arc<AtomicUsize>,
    handshake_ok: Arc<Mutex<Option<bool>>>,
}

impl MockServer {
    pub async fn start(config: MockConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let handshake_ok = Arc::new(Mutex::new(None));
        let shared = (
            Arc::clone(&recorded),
            Arc::clone(&connections),
            Arc::clone(&handshake_ok),
        );
        tokio::spawn(async move {
            let cmd_rx = Arc::new(tokio::sync::Mutex::new(cmd_rx));
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                shared.1.fetch_add(1, Ordering::SeqCst);
                let (config, cmd_rx) = (config.clone(), Arc::clone(&cmd_rx));
                let (recorded, ok) = (Arc::clone(&shared.0), Arc::clone(&shared.2));
                tokio::spawn(async move {
                    let _ = serve(stream, config, cmd_rx, recorded, ok).await;
                });
            }
        });
        Self {
            addr,
            cmd,
            recorded,
            connections,
            handshake_ok,
        }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Write raw bytes to the connected client (after the handshake).
    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.cmd.send(Cmd::Send(bytes));
    }

    /// Drop the TCP connection.
    pub fn close(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }

    /// Everything the client sent after the handshake, in order.
    pub fn recorded(&self) -> Vec<ClientMsg> {
        self.recorded.lock().unwrap().clone()
    }

    /// TCP connections accepted so far (retry detection).
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// `Some(true)` once the client passed security; `Some(false)` if the
    /// server rejected its credentials.
    pub fn security_passed(&self) -> Option<bool> {
        *self.handshake_ok.lock().unwrap()
    }
}

/// Builder for a FramebufferUpdate message (RFC 6143 §7.6.1). Pixels are
/// `0x00RRGGBB` and are encoded for the client's `PixelFormat::bgra()`:
/// bytes `[b, g, r, 0]`.
#[derive(Default)]
pub struct Update {
    rects: Vec<u8>,
    count: u16,
}

impl Update {
    pub fn new() -> Self {
        Self::default()
    }

    fn header(&mut self, x: u16, y: u16, w: u16, h: u16, encoding: i32) {
        for v in [x, y, w, h] {
            self.rects.extend_from_slice(&v.to_be_bytes());
        }
        self.rects.extend_from_slice(&encoding.to_be_bytes());
        self.count += 1;
    }

    pub fn raw(mut self, x: u16, y: u16, w: u16, h: u16, rgb: &[u32]) -> Self {
        assert_eq!(rgb.len(), usize::from(w) * usize::from(h));
        self.header(x, y, w, h, 0);
        for p in rgb {
            self.rects
                .extend_from_slice(&[*p as u8, (*p >> 8) as u8, (*p >> 16) as u8, 0]);
        }
        self
    }

    /// Copy the `w` x `h` block at `(src_x, src_y)` to `(x, y)`.
    pub fn copy_rect(mut self, x: u16, y: u16, w: u16, h: u16, src_x: u16, src_y: u16) -> Self {
        self.header(x, y, w, h, 1);
        self.rects.extend_from_slice(&src_x.to_be_bytes());
        self.rects.extend_from_slice(&src_y.to_be_bytes());
        self
    }

    /// DesktopSize pseudo-encoding (-223): the new size is the rect's w/h.
    pub fn desktop_size(mut self, w: u16, h: u16) -> Self {
        self.header(0, 0, w, h, -223);
        self
    }

    pub fn build(self) -> Vec<u8> {
        self.build_claiming(self.count)
    }

    /// Like `build` but announces `count` rects regardless of how many
    /// follow (malformed update).
    pub fn build_claiming(&self, count: u16) -> Vec<u8> {
        let mut out = vec![0, 0];
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&self.rects);
        out
    }
}

/// RFC 6143 §7.2.2: the key is the password (zero-padded/truncated to 8
/// bytes) with each byte's bits reversed; the response is DES-ECB over the
/// two 8-byte halves of the challenge.
pub fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut out = [0u8; 16];
    for (i, chunk) in challenge.chunks(8).enumerate() {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out[i * 8..(i + 1) * 8].copy_from_slice(&block);
    }
    out
}

fn random_challenge() -> [u8; 16] {
    let mut out = [0u8; 16];
    for half in out.chunks_mut(8) {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(half.as_ptr() as usize);
        half.copy_from_slice(&hasher.finish().to_be_bytes());
    }
    out
}

async fn serve(
    mut s: TcpStream,
    config: MockConfig,
    cmd_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Cmd>>>,
    recorded: Arc<Mutex<Vec<ClientMsg>>>,
    handshake_ok: Arc<Mutex<Option<bool>>>,
) -> std::io::Result<()> {
    match config.failure {
        Failure::EarlyDisconnect => return Ok(()),
        Failure::GarbageHandshake => {
            s.write_all(b"\xde\xad\xbe\xef not RFB at all\n").await?;
            let mut sink = [0u8; 64];
            while s.read(&mut sink).await? > 0 {}
            return Ok(());
        }
        Failure::Stall => {
            let mut sink = [0u8; 64];
            while s.read(&mut sink).await? > 0 {}
            return Ok(());
        }
        Failure::None => {}
    }

    // 7.1.1 ProtocolVersion: both sides pick the lower.
    s.write_all(&config.version).await?;
    let mut client_version = [0u8; 12];
    s.read_exact(&mut client_version).await?;
    let minor = |v: &[u8; 12]| match &v[..] {
        b"RFB 003.003\n" => 3,
        b"RFB 003.007\n" => 7,
        _ => 8,
    };
    let version = minor(&config.version).min(minor(&client_version));

    // 7.1.2 Security: 3.3 the server decides; 3.7+ the client picks.
    let want_auth = config.password.is_some();
    let security: u8 = if want_auth { 2 } else { 1 };
    if version == 3 {
        s.write_all(&u32::from(security).to_be_bytes()).await?;
    } else {
        s.write_all(&[1, security]).await?;
        let mut pick = [0u8; 1];
        s.read_exact(&mut pick).await?;
        assert_eq!(
            pick[0], security,
            "client picked an unoffered security type"
        );
    }

    if let Some(password) = &config.password {
        let challenge = random_challenge();
        s.write_all(&challenge).await?;
        let mut response = [0u8; 16];
        s.read_exact(&mut response).await?;
        let ok = response == vnc_auth_response(password, &challenge);
        *handshake_ok.lock().unwrap() = Some(ok);
        s.write_all(&u32::from(!ok).to_be_bytes()).await?;
        if !ok {
            if version >= 8 {
                let reason = b"Authentication failed";
                s.write_all(&(reason.len() as u32).to_be_bytes()).await?;
                s.write_all(reason).await?;
            }
            return Ok(());
        }
    } else {
        // 7.1.3: SecurityResult is sent for None only in 3.8.
        if version >= 8 {
            s.write_all(&0u32.to_be_bytes()).await?;
        }
        *handshake_ok.lock().unwrap() = Some(true);
    }

    // 7.3.1 ClientInit, 7.3.2 ServerInit (32bpp, depth 24, LE, 255/16,8,0).
    let mut shared = [0u8; 1];
    s.read_exact(&mut shared).await?;
    s.write_all(&config.width.to_be_bytes()).await?;
    s.write_all(&config.height.to_be_bytes()).await?;
    s.write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])
        .await?;
    s.write_all(&(config.name.len() as u32).to_be_bytes())
        .await?;
    s.write_all(config.name.as_bytes()).await?;

    // Reading runs in its own task so a command arriving mid-message can
    // never cancel a half-read client message.
    let (mut read, mut write) = s.into_split();
    let mut reader = tokio::spawn(async move {
        while let Ok(msg) = read_client_msg(&mut read).await {
            recorded.lock().unwrap().push(msg);
        }
    });
    let mut cmd_rx = cmd_rx.lock().await;
    let result = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Send(bytes)) => {
                    if let Err(e) = write.write_all(&bytes).await {
                        break Err(e);
                    }
                }
                Some(Cmd::Close) | None => break Ok(()),
            },
            // Client hung up or sent something unparseable.
            _ = &mut reader => break Ok(()),
        }
    };
    // Dropping both halves closes the socket.
    reader.abort();
    result
}

/// Parse one client message (RFC 6143 §7.5).
async fn read_client_msg(s: &mut OwnedReadHalf) -> std::io::Result<ClientMsg> {
    let kind = s.read_u8().await?;
    Ok(match kind {
        0 => {
            let mut pad = [0u8; 3];
            s.read_exact(&mut pad).await?;
            let mut pf = [0u8; 16];
            s.read_exact(&mut pf).await?;
            ClientMsg::SetPixelFormat(pf)
        }
        2 => {
            s.read_u8().await?;
            let n = s.read_u16().await?;
            let mut encodings = Vec::new();
            for _ in 0..n {
                encodings.push(s.read_i32().await?);
            }
            ClientMsg::SetEncodings(encodings)
        }
        3 => ClientMsg::UpdateRequest {
            incremental: s.read_u8().await? != 0,
            x: s.read_u16().await?,
            y: s.read_u16().await?,
            w: s.read_u16().await?,
            h: s.read_u16().await?,
        },
        4 => {
            let down = s.read_u8().await? != 0;
            s.read_u16().await?;
            ClientMsg::Key {
                down,
                key: s.read_u32().await?,
            }
        }
        5 => ClientMsg::Pointer {
            buttons: s.read_u8().await?,
            x: s.read_u16().await?,
            y: s.read_u16().await?,
        },
        6 => {
            let mut pad = [0u8; 3];
            s.read_exact(&mut pad).await?;
            let len = s.read_u32().await? as usize;
            let mut text = vec![0u8; len];
            s.read_exact(&mut text).await?;
            ClientMsg::CutText(String::from_utf8_lossy(&text).into_owned())
        }
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown client message type {other}"),
            ))
        }
    })
}
