# Tabs and Connections

Every open session is a tab in the window's notebook. This doc covers what
all tabs share: the byte-stream `Transport` trait, how a terminal tab is
wired to its transport, the connect flow, the lifecycle (close, drop,
reconnect, session end), tab labels, the New Tab picker, restoring tabs,
the status bar and the tray. SSH specifics are in doc 04, local shell and
serial in doc 06, RDP and VNC in doc 08.

Code: `src/transport/types.rs`, `factory.rs`, `reconnect.rs` (display-free);
`src/ui/tabs/` — `mod.rs` (the page core), the per-type connect flows,
`terminal_tab.rs`, `resilience.rs`, `ended.rs`, `label.rs`, `actions.rs`,
`state.rs`, `restore.rs`; `src/ui/dialogs/host_picker.rs`,
`src/ui/status_bar.rs`, `src/ui/tray.rs`.

## Tab kinds

The notebook does not care what a tab is. Each page holds its session's
area:

| Session type | Page | Connect flow |
|---|---|---|
| SSH | terminal + SFTP sidebar (shown in the side dock, doc 07) | `ui::tabs::ssh`: dial through the supervisor, then the terminal |
| Local shell, serial | terminal | `ui::tabs::local`: nothing to dial |
| VNC | framebuffer view | `ui::tabs::vnc` (doc 08 §VNC) |
| RDP | embedded xfreerdp window | `ui::tabs::rdp` (doc 08 §RDP) |

The first page is **Home** ("Welcome to Wayterm"). It is shown only while
it is the sole page, is never closed or renamed, and no tab action counts
it.

Opening a session that already has a tab adds a second, independent tab.
Each page gets a unique tab id, so the resilience registry, reconnects and
disconnects work per tab, not per session. A duplicate SSH tab connects
without tmux (doc 04 §Tmux session persistence).

## The Transport trait

A transport moves bytes between a remote host (or a local PTY or serial
device) and a terminal backend, and knows nothing about rendering.

```rust
#[async_trait]
pub trait Transport: Send {
    async fn connect(&mut self) -> Result<(), TransportError>;    // channel, PTY, shell/tmux, reader
    async fn disconnect(&mut self) -> Result<(), TransportError>; // idempotent
    fn state(&self) -> TransportState;
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;
    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError>;
    fn set_tmux_enabled(&mut self, enabled: bool, session_name: String); // before connect
    fn transport_type(&self) -> TransportType;                    // SSH | LocalPTY | Serial
    fn description(&self) -> String;                              // "user@host:22"
}

pub enum TransportState { Disconnected, Connecting, VerifyingHostKey, Authenticating,
                          Connected, Reconnecting { attempt: u32 }, Error(String) }
pub enum TransportEvent { DataReceived(Vec<u8>), StateChanged(TransportState),
                          Error(String), ConnectionLost, SessionEnded }
pub enum TransportError { ConnectionFailed(String), HostKeyRejected(String),
                          AuthFailed(String), ChannelError(String), IoError(io::Error),
                          Disconnected, Cancelled }
```

- There is no `read()`. Each transport's **reader task** pushes
  `DataReceived` for every chunk and, on close, `SessionEnded` if the
  shell exited or `ConnectionLost` if not.
- A transport owns one channel, never the SSH session: that is a shared
  `SshConnection` (doc 04 §Connection supervisor).
- `HostKeyRejected` is distinct from `AuthFailed`. `Cancelled` means the
  user dismissed a prompt or closed the tab, and is never retried.
- State changes are announced as `StateChanged`, best effort.

The trait is a module boundary (AGENTS.md). RDP and VNC are not byte
streams and never implement it.

## Transport factory

`create_transport(config, connection, event_tx, rows, cols)`
(`src/transport/factory.rs`) maps a session type to a transport. SSH needs
a connection; local and serial build theirs directly (`connection` is
`None`). RDP/VNC rows are errors, never panics: a malformed row must not
crash the app.

## Wiring

`TerminalTab` (`src/ui/tabs/terminal_tab.rs`) wires one transport to one
backend with three tasks, each crossing the GTK/tokio boundary once
(doc 01 §Threading model):

- **pump** (tokio): connects (reporting a failure as `Error` on the event
  channel), forwards `DataOut` and `SizeChange` to `write`/`resize`, and
  disconnects as its last act;
- **bridge** (tokio): sends output to the drain and every other event to
  the page's state channel;
- **drain** (main context): calls `backend.write`, through a bounded
  channel, so a stalled UI backs up into the transport.

The bridge is separate from the pump on purpose: folded in, a `write`
blocked on a full SSH window would stop output draining, the server's
window would close in turn, and both sides would deadlock under
full-duplex load.

`close()` cancels the tasks and waits, giving the pump 5 s to disconnect
before it is aborted. Dropping the tab cancels them too, which matters
because the tasks keep each other alive (drain → backend → widget signals
→ pump → transport → bridge → drain).

The transport opens with the terminal's **measured** grid: the connect
flow embeds the widget and waits up to about 2 s for it to be allocated
before reading its rows and columns, never a hardcoded 80×24.

## Connect flow

`start_connect` (`src/ui/tabs/mod.rs`) runs for a new tab, Retry, and every
reconnect:

1. It bumps the page's **generation**. Each attempt is tagged with it, so
   a cancelled or superseded attempt (Cancel, Retry, close) drops its
   result.
2. It stops the old forward engine and closes the old terminal.
3. It hands the page to its type's flow (§Tab kinds).

A terminal flow ends in `finish_connect` (`src/ui/tabs/terminal.rs`):
create the backend (doc 02 §Backend selection) and apply its color scheme,
splice the macro input tap between terminal and pump (doc 12
§Recording), wire the `TerminalTab`, register the tab as live with
`tabs::resilience`, start the page's state drain, hand an SSH connection to
the forward engine and the SFTP sidebar, follow the terminal title, and
take focus.

Focus: a connect the user started takes focus; a reconnect only takes it
back if nothing else holds it, so it never pulls focus out of the SFTP
sidebar or the session tree.

Every page shows one of the shared status views (`src/ui/tabs/views.rs`):
connecting (with Cancel), disconnected, or an error with Retry and Close
Tab.

## Lifecycle

```
Disconnected → Connecting → VerifyingHostKey → Authenticating → Connected
      ▲                 any state: cancel / close                    │
      └──────────────── ConnectionLost ─► Reconnecting { attempt } ◄─┘
                                SessionEnded ─► Disconnected (r / q prompt)
```

One state drain per page feeds the resilience registry, the status bar
and the disconnect alert, and acts on the two endings below.

### Closing a tab

The close button, `close_tab` (doc 11 §Handlers) and closing by any other
path cancel the tab's token: the pump disconnects and any reconnect loop
sharing the token stops. If the transport is live (anything but
`Disconnected` or `Error`) and `confirm_close` is on, the user is asked
first ("Processes may be running — close anyway?") unless they once
ticked that prompt's "Do not show me again" (`confirm_close_notice`,
doc 13 §Settings rows). Removing a page tears down its registry entry and
forward engine, and focus moves to the tab left current.

### Reconnect

The reader emits `ConnectionLost` and a tray alert is posted
(§Tray). With `reconnect_on_drop` on (the default) the reconnect driver
retries on its own; off, it asks before each attempt ("Reconnect to
`host`? (attempt N)", No / Reconnect). Every attempt is a fresh connect
through the supervisor, so a tmux session reattaches.

- **Backoff:** 1 s, 2 s, 5 s, 10 s, then every 60 s, applied before every
  attempt, the first included. Attempts are unlimited while the tab is
  open; each publishes `Reconnecting { attempt }`.
- A connect that fails with `Cancelled`, a declined prompt, or a cancelled
  token ends the run; a declined or exhausted run leaves the tab
  `Disconnected`.
- The `reconnect` action starts a run on the current tab (a second press
  while one runs does nothing); `disconnect` cancels its token.

`transport::reconnect` holds the policy and driver, display-free and
tested against paused time; `ui::tabs::resilience` holds the dialogs, the
window actions and the registry of live tabs.

### Session end

When the shell exits on its own (`exit`, logout, a detached tmux) the
reader emits `SessionEnded`. Nothing reconnects and no alert is posted.
The tab reads as disconnected (closing it does not ask), and the terminal
keeps its scrollback and adds a notice: `r` reconnects (a fresh connect,
as Retry), `q` closes the tab (`ui::tabs::ended`).

## Tab labels

A label is the session name plus a close button (`src/ui/tabs/label.rs`).
Its title follows a state machine: placeholder (the session name) → set
(the terminal's window title) → renamed (by the user). Once renamed,
terminal titles no longer change it; a blank terminal title falls back to
the session name. Double-clicking the name, or `rename_tab`, opens a
"Rename Tab" prompt.

## Tab bar

The tab bar itself is part of the window layout (doc 01 §Tab bar): a "+"
tab after the last tab opens a local shell, and overflowing tabs scroll.
The tab actions (`src/ui/tabs/actions.rs`, doc 11 §Handlers) are
`new_tab`, `close_tab`, `next_tab`/`prev_tab` (wrapping round),
`goto_tab_1` … `goto_tab_9`, and `rename_tab`; Home never counts.

## New Tab

`new_tab` opens a modal host picker (`src/ui/dialogs/host_picker.rs`). "Local
shell" comes first and is selected; every saved session follows in the
session tree's order, under its folders, all expanded, with the tree's
icons. Up and Down move between hosts (folders are headings, never
selected), Return or a double-click opens the host in a new tab, Escape
cancels. Folders with no session below them are left out.

"Local shell" is an unsaved local session: the login shell (`$SHELL`) in
the home directory. Its tab is saved under session id 0, which SQLite
never assigns, and restores as a local shell.

The tab bar's "+" skips the picker and opens a local shell at once.

## Restoring tabs

The open tabs are saved as one JSON array in the `open_tabs` settings row
(`src/ui/tabs/state.rs`): per tab its session id (or none), label,
creation time and position. Only identity is stored, never a credential
or connection detail. Missing fields default and unknown ones are
ignored, so lists from older and newer builds both restore.

`TabPersistence` (`src/ui/tabs/restore.rs`) saves on tab open, close and
rename, debounced by 2 s so a rename is one write, and once more on exit.
At launch it reopens the saved tabs in order (by position, then age, then
session id) and focuses the last one. Restored session tabs connect as
they open, like new ones but without the duplicate-tmux notice; a tab
whose session no longer exists reopens detached (label and close button
only). A corrupt list costs one launch's layout, never the launch.

## Status bar

`src/ui/status_bar.rs` shows the current tab's state: a colored dot (green
connected, yellow connecting/verifying/authenticating/reconnecting, red
disconnected or failed), the state text with the endpoint ("Connected to
example.com:22"), and the time connected (`m:ss`, `h:mm:ss` from an hour
on) while connected. Local and serial tabs show the transport's
description (shell path, `device@baud`) instead of a host. Each page keeps
its last published state; the window's status router shows only the
current page's updates, and replays the current page's state whenever the
current page changes or a tab closes (Home reads Disconnected).

## Tray

`src/ui/tray.rs`: a StatusNotifierItem over D-Bus via `ksni` (pure Rust,
on its own small tokio runtime), with the app icon sent as pixels so it
shows even when Wayterm is not installed. Left-click restores the window;
the menu offers Open Wayterm, New Session and Quit. Menu clicks are queued
and run on the GTK main thread.

When the main window closes and `minimize_to_tray` is on (the default), it
hides instead of quitting. Without a session bus or a tray host (headless,
GNOME without the AppIndicator extension) the tray fails to start, which
is never fatal: closing the window quits.

A dropped connection posts a desktop notification through
`gio::Notification`: "Wayterm" / "`session` connection lost".

## Limitations

- Only global shortcuts reach RDP and VNC tabs; there is no per-tab
  shortcut context for them (doc 11 §Dispatch).
- Reconnect is never automatic after `SessionEnded`, by design.
- The Ghostty backend's grid is not measured before connecting: its
  transport opens at 24×80 and resizes on the first allocation.
