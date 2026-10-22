# Local Shell and Serial

Two byte-stream transports with nothing to dial or authenticate: no
supervisor, host key or credential machinery. Both open as terminal tabs
through the transport factory (doc 03 §Transport factory); the connect
flow (`src/ui/tab/local.rs`) starts the terminal directly. They have no
SFTP sidebar, and the status bar shows the transport's description
instead of a host.

## Local shell

`LocalTransport` (`src/transport/local.rs`) runs the user's shell on a
`forkpty` pseudo-terminal.

- **Shell:** the session's `local_shell`, else `$SHELL`, else `/bin/sh`.
  A bare name (`zsh`) is looked up in `$PATH` before the fork, so the
  child only calls `execve`. A shell that is set but missing or not
  executable is a connect error, never a silent fallback.
- **Working directory:** `local_working_dir` when set; a missing
  directory is a connect error. Unset, the child inherits Wayterm's.
- **Environment:** Wayterm's own, with `TERM=xterm-256color` and
  `COLORTERM=truecolor` on top. The child closes every inherited
  descriptor above stderr (`close_range`, or one by one on kernels before
  5.9).
- **I/O:** an `AsyncFd` reader on the master pushes `DataReceived`, the
  same push model as SSH. `resize` sets `TIOCSWINSZ`; the kernel delivers
  `SIGWINCH` to the child's foreground process group. The PTY opens at the
  terminal's measured grid.
- **Ending:** `EIO` on the master means the child exited: it is reaped
  and reported as `SessionEnded` (doc 03 §Session end). `disconnect`
  closes the master, which hangs up the PTY (`SIGHUP`); the transport
  reaps the child on disconnect and on drop, sending `SIGKILL` after 5 s
  if a hung-up shell lingers, so no zombie remains.
- **tmux** does not apply locally: the flag is accepted and ignored.

The New Tab picker's "Local shell" and the tab bar's "+" open an unsaved
local session: the login shell in the home directory (doc 03 §New Tab).

## Serial

`SerialTransport` (`src/transport/serial.rs`) opens a serial device with
the `serialport` crate (4.x, by device path; its default `libudev` feature
is off).

- **Settings:** `serial_device`, `serial_baud_rate` (the session dialog
  starts at 115200), `serial_data_bits` (5–8, default 8), `serial_parity`
  (`none`, `odd`, `even`, `mark`, `space`; default `none`),
  `serial_stop_bits` (1–2, default 1) and `serial_flow_control` (`none`,
  `hardware`, `software`; default `none`). They are mapped once; an
  unknown value fails `connect` rather than opening the port with the
  wrong frame.
- **Reads** block, so the reader runs on tokio's blocking pool, waking
  every 100 ms to check its cancel token (a serial line never sends EOF).
  A read error — device unplugged, permission revoked — is
  `ConnectionLost`, so the reconnect policy applies (doc 03 §Reconnect).
- **Writes** held off by flow control retry for up to 5 s in total.
- **`resize`** is a no-op: a serial line has no window size.
- **Description:** `device@baud`, shown in the status bar.

## Limitations

- `mark` and `space` parity are stored but not representable by
  `serialport`; connecting with either fails with "not supported by this
  build" rather than opening with other framing. Linux `CMSPAR` support
  would lift this.
- Reconnecting to a device that vanished and came back is untested; the
  hardware test (`SERIAL_TEST_PEER`, two cabled USB-serial ports) has not
  run against real hardware (doc 15 §Serial).
- No file browser for local tabs.
