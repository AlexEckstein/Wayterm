# Transport Layer Design

## Purpose

The transport layer handles the connection to the remote host (or local
PTY). It is fully separated from the terminal backend — the terminal
widget does not know or care where bytes come from.

> **Revision note (2026-08):** this doc previously implied a pull-style
> `read()` loop that nothing implemented, and had `SSHTransport` privately
> owning the SSH session while doc 09's SFTP sidebar needed a handle to it.
> This revision makes the read path push-based and explicit, moves session
> ownership into a shared `SshConnection`, and adds the lifecycle/cancellation
> semantics. Auth/host-key details now live in
> [11-ssh-auth-security.md](11-ssh-auth-security.md).

## Rust Advantage

All transports run as tokio async tasks. The GTK UI runs on the GLib main
loop. Data crosses between them via channels — no manual locking, no data
races (compiler-enforced via `Send` + `Sync` bounds). Multiple concurrent
SSH sessions, SFTP transfers, and VNC polling tasks coexist safely.

## Threading Model (read this before the code)

The single most important architectural fact: **GTK widget calls happen on
the main thread only; all network I/O happens on tokio workers.** Every
data path below crosses that boundary exactly once, via a channel:

```
                        GTK main thread                 tokio runtime
                     ┌────────────────────┐      ┌──────────────────────────┐
 user types ─────────► VTE commit signal   │      │                          │
                     │   try_send ────────┼─────►│  pump task               │
                     │   (overflow: idle  │      │   term_rx.recv() ──┐     │
                     │    flush, §below)  │      │                     ▼     │
                     │                    │      │          Transport::write/resize
 terminal  ◄─────────┤ VteBackend::write() │      │                          │
 output    ┌─────────┤   ▲                 │      │  channel reader task     │
           │ glib   │   │ glib MainContext │      │   channel.wait() ──┐    │
           │ channel│   │ channel attach    │      │                    ▼    │
           └─────────┤   │                 │◄─────┼── DataReceived(chunk) ──┘
                     │                    │      │                          │
                     └────────────────────┘      └──────────────────────────┘
```

1. **Transport → terminal (the read path):** each channel spawns a reader
   task on tokio. It reads chunks (up to 64 KiB per read), sends each
   chunk as one message over a `glib::MainContext::channel`, and the
   receiver (attached to the default main context) calls
   `backend.write(&chunk)` on the main thread. The reader stops on EOF
   (`ChannelMsg::Eof`/`Close`) and emits `TransportEvent::ConnectionLost`.
2. **Terminal → transport (the write path):** the VTE `commit` signal
   fires on the main thread and does `try_send` into a **bounded** tokio
   mpsc channel (the pump task's input). `try_send`, never `blocking_send`
   — blocking the GTK thread stalls the whole UI.
3. **Backpressure (overflow of the bounded channel):** if `try_send`
   fails, park the bytes in a per-tab `RefCell<Vec<u8>>` and schedule a
   `glib::idle_add_local` flush that retries. GTK runs the idle callback
   as soon as the main loop is free — keystrokes never block, never drop.
   Large pastes are the case that exercises this.
4. **Resize:** same path as keystrokes (a `TerminalEvent::SizeChange`
   message into the pump task).

## Trait Definition

```rust
use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq)]
pub enum TransportState {
    Disconnected,
    Connecting,
    VerifyingHostKey,   // new — see doc 11
    Authenticating,     // new
    Connected,
    Reconnecting { attempt: u32 },  // new
    Error(String),
}

#[derive(Clone, Debug)]
pub enum TransportEvent {
    /// Pushed by the transport's reader task — the ONLY way bytes
    /// arrive from the remote side. (There is no `read()` method.)
    DataReceived(Vec<u8>),
    StateChanged(TransportState),
    Error(String),
    ConnectionLost,
}

#[derive(Clone, Debug)]
pub enum TransportType {
    SSH,
    LocalPTY,
    Serial,
    // RDP and VNC are not byte-stream transports — handled separately (doc 08)
}

/// The transport owns ONE channel (session shell/exec/PTY), never the
/// underlying SSH session. The session lives in `SshConnection` (below)
/// so the SFTP sidebar (doc 09) and future tunnels can share it.
#[async_trait]
pub trait Transport: Send {
    /// Connect: establish session (if needed), open channel, request PTY,
    /// start shell (or tmux), and SPAWN the reader task that pushes
    /// `DataReceived` events.
    async fn connect(&mut self) -> Result<(), TransportError>;

    /// Disconnect and clean up. Cancels the reader task. Idempotent.
    async fn disconnect(&mut self) -> Result<(), TransportError>;

    fn state(&self) -> TransportState;

    /// Bytes → remote (called by the pump task on tokio, never from GTK)
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;

    /// Notify remote PTY of terminal size change
    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError>;

    /// Enable/disable tmux session persistence (must be set pre-connect)
    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String);

    fn transport_type(&self) -> TransportType;

    /// Human-readable description ("user@host:22")
    fn description(&self) -> String;
}

#[derive(Debug)]
pub enum TransportError {
    ConnectionFailed(String),
    HostKeyRejected(String),   // new — distinct from auth failure (doc 11)
    AuthFailed(String),
    ChannelError(String),
    IoError(std::io::Error),
    Disconnected,
    Cancelled,                 // new — tab closed / user aborted
}
```

**There is deliberately no `read()`.** Data flows out of a transport only
via the `TransportEvent` sender given at construction. This replaces the
earlier pull-style `transport.read()` sketch in doc 02's `transport_loop`
(which implemented a method the trait never had).

## Connection Supervisor and `SshConnection`

Doc 09's SFTP sidebar needs to open a channel on the same authenticated
session as the terminal, and doc 11's port forwarding needs the same.
Therefore the SSH session is **not** owned by `SSHTransport`:

```rust
/// One authenticated russh session, shared by consumers.
/// Created and owned by ConnectionSupervisor; consumers hold Arc.
pub struct SshConnection {
    handle: russh::client::Handle<AppClientHandler>,
    host: String,
    port: u16,
    username: String,
}

impl SshConnection {
    /// Shell/exec/PTY channels — used by SSHTransport.
    pub async fn open_session_channel(&self) -> Result<russh::Channel<russh::ChannelMsg>, TransportError>;
    /// direct-tcpip channels — used by local forwards and jump hops (doc 11).
    pub async fn open_direct_tcpip(&self, host: &str, port: u16) -> Result<russh::Channel<russh::ChannelMsg>, TransportError>;
    /// SFTP subsystem channel — used by the sidebar (doc 09).
    pub async fn open_sftp_channel(&self) -> Result<russh::Channel<russh::ChannelMsg>, TransportError>;
}

/// Owns connections; disconnects a session when its last Arc drops.
/// (Explicit `disconnect()` on close is still preferred for cleanliness;
/// drop-based disconnection is the safety net.)
pub struct ConnectionSupervisor {
    connections: Vec<(Arc<SshConnection>, JoinHandle<()>)>,
}
```

`SSHTransport` is now: *a channel-open + PTY + shell/exec wrapper over an
`Arc<SshConnection>`* — it no longer owns the session. One user opening
two tabs to the same host shares one TCP connection, two channels
(future optimization; v1 may simply open two connections — the supervisor
makes dedup possible without an API break).

## SSHTransport (russh)

```rust
use russh::client::{self, Handle};
use russh::{ChannelMsg, Disconnect};
use tokio_util::sync::CancellationToken;

pub struct SSHTransport {
    // Shared, supervisor-owned — NOT owned here (see SshConnection above)
    connection: Arc<SshConnection>,

    // Our channel on that connection
    channel: Option<russh::Channel<ChannelMsg>>,

    // Reader task JoinHandle — spawned in connect(), aborted in disconnect()
    reader_task: Option<tokio::task::JoinHandle<()>>,

    // Where reader task pushes DataReceived chunks (bound to the GTK side)
    event_tx: mpsc::Sender<TransportEvent>,

    // Initial PTY size, taken from the terminal backend at connect time —
    // NOT a hardcoded 80x24. The first resize event corrects later changes.
    initial_rows: u32,
    initial_cols: u32,

    state: TransportState,
    tmux_enabled: bool,
    tmux_session_name: String,
}
```

### connect()

```rust
#[async_trait]
impl Transport for SSHTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.state = TransportState::VerifyingHostKey;
        self.emit_state();

        // 1. Session is established by ConnectionSupervisor::connect() —
        //    TCP + KEX + host key verification + authentication all happen
        //    there, per doc 11. (Nothing security-related lives here.)
        //    On failure, supervisor returns TransportError::{HostKeyRejected,
        //    AuthFailed, ConnectionFailed} — surfaced verbatim.

        self.state = TransportState::Connected; // after channel setup below
        self.emit_state();

        // 2. Open our channel
        let mut channel = self.connection.open_session_channel().await?;

        // 3. Request PTY with the ACTUAL current terminal size
        channel.request_pty(
            false, "xterm-256color",
            self.initial_cols, self.initial_rows, 0, 0, &[]
        ).await?;

        // 4. Shell or tmux
        if self.tmux_enabled {
            // Quoted session name; convention: "<app>-<sessionName>"
            let cmd = format!("tmux new -A -s {}", shell_quote(&self.tmux_session_name));
            channel.exec(false, cmd.as_bytes()).await?;
        } else {
            channel.request_shell(false).await?;
        }

        // 5. Spawn the reader task — THE read path. Pushes chunks until EOF.
        let event_tx = self.event_tx.clone();
        let cancel = self.cancel_token.clone();
        self.reader_task = Some(tokio::spawn(async move {
            let mut channel = channel;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = channel.wait() => match msg {
                        Some(ChannelMsg::Data { data }) => {
                            if event_tx.send(TransportEvent::DataReceived(data.to_vec())).await.is_err() {
                                break; // UI side gone (tab closed)
                            }
                        }
                        Some(ChannelMsg::ExtendedData { data, .. }) => { /* same, stderr bucket */ }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                            let _ = event_tx.send(TransportEvent::ConnectionLost).await;
                            break;
                        }
                        None => {
                            let _ = event_tx.send(TransportEvent::ConnectionLost).await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }));

        Ok(())
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if let Some(ch) = &mut self.channel {
            ch.data(data).await?;   // handles window sizing internally
            Ok(())
        } else {
            Err(TransportError::Disconnected)
        }
    }

    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError> {
        if let Some(ch) = &mut self.channel {
            ch.window_change(cols, rows, 0, 0).await?;
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        // Idempotent: safe to call twice, safe if never connected
        if let Some(task) = self.reader_task.take() { task.abort(); }
        if let Some(ch) = self.channel.take() {
            ch.close().await.ok();
        }
        // NOTE: the SshConnection itself is NOT disconnected here — other
        // tabs/the SFTP sidebar may share it. The supervisor disconnects
        // it when the last Arc drops.
        self.state = TransportState::Disconnected;
        self.emit_state();
        Ok(())
    }
}
```

**Auth is not shown here because it is not this type's job** — the
supervisor does host-key verification and authentication per doc 11,
including keyboard-interactive. The earlier sketch in this doc (a)
hardcoded `"ssh-ed25519"` as the key hash algorithm — wrong for RSA
keys, derive per key type instead; (b) omitted keyboard-interactive
entirely; (c) referenced agent APIs (`AgentClient::connect_env`,
`authenticate_future`) that must be re-verified against russh 0.63. All
three issues are now specified in doc 11 §Auth.

### Reader-task details that matter

- **Chunking:** `ChannelMsg::Data` arrives framed by the SSH layer; large
  pastes produce many small messages. The reader coalesces nothing —
  coalescing happens on the GTK side is unnecessary; VTE `feed()` is
  cheap per call. If profiling ever shows main-thread pressure, batch in
  the reader with a 1 ms timer, not in GTK.
- **`event_tx` is bounded** (256). If the UI stops draining (user
  dragging a window), the reader awaits — SSH flow control then applies
  backpressure to the server via the channel window. This is correct
  behavior: the network slows instead of memory growing.
- **Cancellation:** `CancellationToken` (tokio-util). Tab close cancels;
  the reader exits without sending further events.

## LocalTransport (forkpty via Rust)

```rust
use nix::sys::pty::*;
use nix::unistd::ForkResult;

pub struct LocalTransport {
    master_fd: i32,        // owned; AsyncFd wrapper for tokio
    child_pid: i32,
    shell: String,
    working_dir: String,
    state: TransportState,
    event_tx: mpsc::Sender<TransportEvent>,
    reader_task: Option<tokio::task::JoinHandle<()>>,
}
```

- `connect()`: `forkpty`, child execs the shell (parent sets up
  `tokio::io::unix::AsyncFd` on the master fd).
- The reader task is an AsyncFd read loop pushing
  `TransportEvent::DataReceived` — same push model as SSH, same
  cancellation token. `EIO` on the master = child exited = emit
  `ConnectionLost` (plus `waitpid` to reap the zombie — reap on
  disconnect or via a watcher task; do NOT leak children).
- `write()`: AsyncFd write (or plain `nix::unistd::write` — acceptable
  at terminal data rates).
- `resize()`: `TIOCSWINSZ` ioctl on the master fd.
- Env: inherit parent env plus `TERM=xterm-256color`, `COLORTERM=truecolor`.

## SerialTransport

Unchanged from the previous revision (correct as written): `serialport`
crate, blocking `write_all` under the async trait is acceptable at serial
data rates (or spawn a dedicated thread doing read → event_tx push —
same push model). `resize()` is a no-op. Note: `serialport`'s blocking
reads must run on `spawn_blocking` or a dedicated thread, never directly
on the async executor.

## Transport Factory

```rust
pub fn create_transport(
    config: &SessionConfig,
    connection: Option<Arc<SshConnection>>,   // SSH only; supervisor-supplied
    event_tx: mpsc::Sender<TransportEvent>,
    initial_rows: u32, initial_cols: u32,     // from the terminal backend
) -> Result<Box<dyn Transport>, TransportError> {
    match config.session_type {
        SessionType::SSH => Ok(Box::new(SSHTransport::new(
            connection.ok_or(TransportError::ChannelError("no connection".into()))?,
            event_tx, initial_rows, initial_cols,
        ))),
        SessionType::LocalShell => Ok(Box::new(LocalTransport::new(/* ... */))),
        SessionType::Serial => Ok(Box::new(SerialTransport::new(/* ... */))),
        // RDP/VNC return Err, not panic!() — a malformed session row in the
        // DB must not crash the app
        SessionType::RDP | SessionType::VNC => Err(TransportError::ChannelError(
            "RDP/VNC don't use byte-stream transport (doc 08)".into(),
        )),
    }
}
```

## Wiring: Transport ↔ Terminal Backend

One pump per tab. The pump lives on **tokio** (it awaits `write`), and
bridges to GTK via a glib channel for the reverse direction.

```rust
pub struct TerminalTab {
    transport: Box<dyn Transport>,
    backend: Arc<dyn TerminalBackend>,       // Arc — GTK side shares it
    pump_task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

impl TerminalTab {
    pub fn new(
        config: &SessionConfig,
        connection: Option<Arc<SshConnection>>,
        backend: Arc<dyn TerminalBackend>,
        main_ctx: &glib::MainContext,
    ) -> Self {
        let (term_tx, term_rx) = mpsc::channel::<TerminalEvent>(256);
        let (gtk_tx, gtk_rx) = glib::MainContext::channel::<Vec<u8>>();

        // 1. GTK side: terminal output arrives as Vec<u8> chunks on the
        //    glib channel; attach() runs on the main thread.
        let backend_clone = backend.clone();
        gtk_rx.attach(Some(main_ctx), move |chunk| {
            backend_clone.write(&chunk);
            glib::ControlFlow::Continue
        });
        // (TransportEvent variants other than DataReceived are routed
        //  through a second glib channel — status bar, reconnect prompt.)

        // 2. Backend emits events (keystrokes, resizes) via term_tx —
        //    the backend was constructed with term_tx (see doc 02's
        //    TerminalHandle). try_send + idle-flush on overflow there.

        // 3. Tokio side: pump task.
        let mut transport = create_transport(config, connection, /* ... */)?;
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let pump_task = tokio::spawn(async move {
            if transport.connect().await.is_err() { return; }
            loop {
                tokio::select! {
                    _ = cancel2.cancelled() => break,
                    ev = term_rx.recv() => match ev {
                        Some(TerminalEvent::DataOut(d)) => { transport.write(&d).await.ok(); }
                        Some(TerminalEvent::SizeChange{rows, cols}) => { transport.resize(rows, cols).await.ok(); }
                        Some(TerminalEvent::Closed) | None => break,
                        _ => {}
                    },
                }
            }
            transport.disconnect().await.ok();
        });

        TerminalTab { /* ... */ }
    }
}
```

This replaces the earlier `run_transport_loop` sketch, which selected on
a `transport_rx` that no code ever sent to, and referenced a
`backend.clone()` that didn't exist in scope. The shape above is the
whole story: **reader task pushes → glib channel → backend; backend
pushes → bounded mpsc → pump task → transport.write.**

## Lifecycle & Cancellation

State machine per tab (mirrors `TransportState`, with UI actions):

```
 Disconnected ──connect──► Connecting ─► VerifyingHostKey ─► Authenticating ─► Connected
      ▲                       │              │                   │              │
      │                       ▼              ▼                   ▼              ▼
      └──── cancel/close ◄── any state: abort tasks, close channel, emit Disconnected
                              │                                                  │
                              ▼ (network drop)                                    ▼ (network drop)
                          Error(msg) ◄──────────────────── ConnectionLost ──► Reconnect? (doc 10 phase 8)
```

- **Tab close:** cancel token → pump task exits → `transport.disconnect()`
  → supervisor drops the `Arc<SshConnection>` when last consumer releases
  it. Confirm-before-close if processes running (setting `confirm_close`).
- **Connection drop:** reader task emits `ConnectionLost`; UI shows the
  reconnect prompt (or auto-reconnect if enabled per session). Reconnect
  = fresh `connect()` through the supervisor, reattach tmux if enabled
  (the tmux server on the remote host kept the session alive — that's
  the entire point of the feature).
- **Backoff for auto-reconnect:** 1s, 2s, 5s, 10s, capped 60s; unlimited
  attempts while the tab is open; each attempt updates
  `TransportState::Reconnecting { attempt }`.
- **Cancellation of dialogs:** host-key and auth prompts (doc 11) are
  cancellable; cancellation aborts the connect with `TransportError::Cancelled`.

## Auth & Credential Handling

Credentials are never stored in the session config. The full design —
host-key verification policy, keyboard-interactive flows, the
challenge-response dialog, keyring schema, and zeroization — now lives in
[11-ssh-auth-security.md](11-ssh-auth-security.md). Summary of the rules
that bind this doc:

1. `CredentialManager` resolves passwords/passphrases (keyring → prompt →
   store-if-checked) before/during connect, on the GTK side.
2. Secrets cross to the transport task as `Zeroizing<Vec<u8>>` and are
   dropped/zeroed after use.
3. Auth failures that leave remaining methods retry with the next method
   (agent → key → password → keyboard-interactive); exhaustion = clean
   error screen.
4. Nothing secret is ever passed on a command line, stored in SQLite,
   or logged.

## Tmux Session Persistence

Per-session toggle in session config: "Persist via tmux: ☑"

When enabled, SSHTransport runs `tmux new -A -s <name>` instead of a
plain shell. This means:

- New session: creates a new tmux session
- Existing session: attaches to it (`-A` = attach if exists, create if not)
- Connection drops: tmux session survives on the remote host
- Reconnect: re-attaches to same tmux session, full state restored

Session naming convention: `<app>-<sessionName>` to avoid conflicts with
the user's own tmux sessions. Quote the name — session names may contain
shell metacharacters (`;` is legal in tmux session names and would
otherwise split the command).

Caveat: tmux scrollback persists on the remote host after disconnect —
data-at-rest on the remote side, out of scope client-side (doc 11).

## Crate Dependencies

```toml
[dependencies]
russh = "0.63"           # Pure Rust SSH client (tokio-native)
russh-sftp = "2.4"       # SFTP subsystem (doc 09)
russh-config = "0.58"    # ~/.ssh/config parsing (doc 10 phase 9 import)
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"       # CancellationToken
nix = { version = "0.31", features = ["pty", "process", "term", "signal"] }
serialport = "4.10"
libsecret = "0.9"        # OS keyring
zeroize = { version = "1", features = ["zeroizing"] }
```

Versions checked against crates.io on 2026-08-28. russh moves fast —
re-verify at Phase 1 kickoff (doc 11 "Version anchors").