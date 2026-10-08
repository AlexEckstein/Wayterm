# Transport Layer

A transport moves bytes between a remote host (or a local PTY) and a
terminal backend, and knows nothing about rendering. Authentication and
host keys are specified in [11-ssh-auth-security.md](11-ssh-auth-security.md).

## Threading model

GTK widgets are touched on the main thread only; network I/O runs on
tokio. Each direction crosses once, through a bounded channel:

```
keystrokes  VTE commit ─try_send─► pump task ─► Transport::write / resize
output      reader task ─► bridge task ─► drain (main context) ─► backend.write
```

- The **reader task** (per transport) pushes `DataReceived` for every
  channel message and `ConnectionLost` on EOF or close. It is the only read
  path: the trait has no `read()`.
- The GTK side never blocks: overflow is parked and flushed from an idle
  callback (doc 02 §Backpressure).
- Output channels are bounded, so a stalled UI backs up into the reader and
  then into SSH flow control: the server slows down instead of memory
  growing.

## The trait

```rust
#[async_trait]
pub trait Transport: Send {
    async fn connect(&mut self) -> Result<(), TransportError>;   // channel, PTY, shell/tmux, reader
    async fn disconnect(&mut self) -> Result<(), TransportError>; // idempotent
    fn state(&self) -> TransportState;
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;
    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError>;
    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String); // before connect
    fn transport_type(&self) -> TransportType;                   // SSH | LocalPTY | Serial
    fn description(&self) -> String;                             // "user@host:22"
}

pub enum TransportState { Disconnected, Connecting, VerifyingHostKey, Authenticating,
                          Connected, Reconnecting { attempt: u32 }, Error(String) }
pub enum TransportEvent { DataReceived(Vec<u8>), StateChanged(TransportState),
                          Error(String), ConnectionLost }
pub enum TransportError { ConnectionFailed(String), HostKeyRejected(String),
                          AuthFailed(String), ChannelError(String), IoError(io::Error),
                          Disconnected, Cancelled }
```

`HostKeyRejected` is distinct from `AuthFailed`; `Cancelled` means the user
dismissed a prompt or closed the tab, and is never retried.

## Connection Supervisor

A transport owns one channel, never the SSH session. The session is an
`SshConnection` — one authenticated russh handle that opens session
channels (`SSHTransport`), SFTP subsystem channels (doc 09), and, from
Phase 11, direct-tcpip channels (doc 11).

`ConnectionSupervisor` (one per window) shares one connection per
(host, port, username). Consumers hold `Arc`s; a monitor task disconnects
and evicts the connection once only the pool's own reference is left.
Concurrent dials for one key may race; the loser disconnects its own
connection and adopts the winner's. Dialing runs host-key verification and
the auth chain (doc 11); because prompts block until answered, the UI runs
the dial on tokio's blocking pool.

## SSHTransport

`connect` opens a session channel, requests an `xterm-256color` PTY with
the terminal's **measured** grid (never a hardcoded 80×24), then starts
either a shell or tmux (§Tmux Session Persistence), and splits the channel:
the reader task owns the read half, `write`/`resize` use the write half.
`disconnect` cancels the reader and drops the channel; the shared
connection stays up.

## Transport Factory

`create_transport(config, connection, event_tx, rows, cols)` maps a session
type to a transport. SSH needs a connection; local and serial return an
error until Phase 5; RDP/VNC are not byte streams (doc 08). Unsupported
types are errors, never panics — a malformed row must not crash the app.

## Wiring

`TerminalTab` (`src/transport/tab.rs`) wires one transport to one backend
with three tasks:

- **pump** (tokio): connects (reporting a failure as `Error` on the state
  channel), forwards `DataOut` and `SizeChange` to the transport, and
  disconnects as its last act;
- **bridge** (tokio): sends output to the drain and every other event to
  the state channel, which `TerminalTab::new` returns to the UI;
- **drain** (main context): calls `backend.write`.

The bridge is separate from the pump on purpose: folded in, a `write`
blocked on a full SSH window would stop output draining, the server's
window would close in turn, and both sides would deadlock under
full-duplex load. `close()` cancels all three and waits (the pump gets 5 s
to disconnect); dropping the tab cancels them too, which matters because
the tasks keep each other alive.

## Lifecycle & Cancellation

```
Disconnected → Connecting → VerifyingHostKey → Authenticating → Connected
      ▲                 any state: cancel / close                    │
      └──────────────── ConnectionLost ─► Reconnecting { attempt } ◄─┘
```

- **Tab close** cancels the tab's token; the pump disconnects. If the
  transport is live and `confirm_close` is on, the user is asked first.
- **Connection drop:** the reader emits `ConnectionLost`. With
  `reconnect_on_drop` on, the reconnect driver retries on its own;
  otherwise it asks before each attempt. Every attempt is a fresh connect
  through the supervisor, so tmux reattaches.
- **Backoff:** 1 s, 2 s, 5 s, 10 s, then 60 s, applied before every attempt
  (the first included); unlimited attempts while the tab is open; each
  attempt publishes `Reconnecting { attempt }`. A connect that fails with
  `Cancelled`, or a cancelled token, ends the loop.

`transport::reconnect` holds the display-free policy and driver;
`ui::tab_resilience` holds the dialogs and the `reconnect`/`disconnect`
window actions, which act on tabs registered as live. (Registration from
the connect flow is still to be wired; doc 10.)

## Tmux Session Persistence

A per-session toggle (`sessions.tmux_enabled`, doc 06). When on,
`SSHTransport` runs `tmux new -A -s wayterm-<name>` instead of a shell:
`-A` attaches when the session exists, so a dropped connection reconnects
to the same live session. The `wayterm-` prefix keeps clear of the user's
own sessions, and the name is shell-quoted (`;` is legal in tmux names).

`sessions.tmux_session_name` stores the bare suffix (the transport adds the
prefix); NULL means the session's own name. The dialog shows the full
remote name, as `tmux list-sessions` prints it, strips one leading
`wayterm-` on save, and follows the Name field until the user edits it.
`tests/tmux_persistence.rs` proves the same tmux session id and its
environment survive dropping the transport *and* the connection. tmux
scrollback persists on the remote host; that data at rest is out of scope.

## Local and serial transports (Phase 5)

- **LocalTransport:** `forkpty` the user's shell; an `AsyncFd` reader with
  the same push model; `TIOCSWINSZ` for resize; `EIO` on the master means
  the child exited (reap it); `TERM=xterm-256color`, `COLORTERM=truecolor`.
- **SerialTransport:** `serialport` with its blocking reads on
  `spawn_blocking`; `resize` is a no-op.
