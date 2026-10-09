# Phase 5 — Local shell and serial

Status: in flight · 2026-10-09 · Orchestrator: Hermes (manager profile) ·
Fleet: Claude Code CLI via herdr panes, Sonnet (resolves to
claude-sonnet-5-5 on this account)

## Goal

Fill the `create_transport` seam (`src/transport/tab.rs`) with
`LocalTransport` and `SerialTransport` (doc 04 §Local and serial
transports) and connect local/serial sessions from the tab open path. A
local shell session runs commands; a serial session shows output and
takes input.

## Verified ground (orchestrator, 2026-10-09)

- `SessionType::LocalShell` / `SessionType::Serial` exist in the store
  model; `serial_*` and `local_shell` fields exist (doc 06); the dialog
  already collects and stores them (`src/ui/session_dialog.rs`).
- `create_transport` (`src/transport/tab.rs:37`) returns the Phase-5
  placeholder error for Local/Serial — the lane seam.
- `SessionTab::open` (`src/ui/tab.rs:83`) auto-connects only SSH tabs —
  the UI seam.
- `serialport` 4.10.1 is current. libudev is NOT installed on this host
  (pkg-config probe failed) — the crate must be added with
  `default-features = false` (no port enumeration; doc 04 opens by path).
- No USB serial hardware on this host. Serial verification uses an
  `openpty` PTY pair exposed as `/dev/pts/N` (real serialport open, real
  TTY device); `scripts/serial-loopback.sh` is the optional
  two-port USB-serial path for hardware testing on Alex's machine.
- socat is not installed; do not require it.
- Fleet: herdr 0.9.3, claude 2.1.295 (Pro, sonnet), worktrees have
  ~20G free each lane. Two lanes in parallel is safe.
- GTK system package on this host satisfies the v4_14 feature (Phases
  1–4 CI runs confirm); lanes change no GTK features.

## Lanes (disjoint by file ownership)

### P5-1 — LocalTransport: `agent/phase5-localpty`

Owns: `src/transport/local.rs` (new), `Cargo.toml`/`Cargo.lock`,
`src/transport/mod.rs` (one line), `src/transport/types.rs` (doc-only,
if anything), `tests/local_pty.rs` (new), `src/transport/local/tests.rs`
(if splitting unit tests).

Fills the factory arm for `SessionType::LocalShell` in
`src/transport/tab.rs` (one match arm change in an otherwise unowned
file — flag it in the report per AGENTS.md).

- `forkpty(3)` the user's shell: `local_shell` if set, else `$SHELL`,
  else `/bin/sh`; `TERM=xterm-256color`, `COLORTERM=truecolor` (doc 04).
- `local_working_dir` becomes the child's cwd when set.
- `AsyncFd` reader with the same push model as ssh.rs; `EIO` on the
  master ⇒ child exited: reap it (waitpid via
  `libc::waitpid(pid, WNOHANG)`), emit `ConnectionLost`, then
  `Disconnected`.
- Resize via `TIOCSWINSZ` ioctl (+ `SIGWINCH` reaches the child).
- `write` writes to the master fd; `disconnect` closes the master (child
  gets SIGHUP), reaps via a waitpid task so no zombie persists.
- `set_tmux_enabled` is accepted but unused — local sessions are not
  persisted remotely (document it in the impl).
- Unit: env/cwd/shell selection; `disconnect` idempotence; state
  machine. Integration (`tests/local_pty.rs`): real `forkpty` + bash,
  run `echo wayterm-p5-local`, assert echoed output and that resize
  (TIOCSWINSZ) + `stty size` reports the new size; close the shell and
  assert `ConnectionLost` + exit status reaped.
- Verify (doc 10): a local shell runs commands.

### P5-2 — SerialTransport: `agent/phase5-serial`

Owns: `src/transport/serial.rs` (new), `Cargo.toml`/`Cargo.lock`,
`src/transport/mod.rs` (one line), `tests/serial_pty.rs` (new),
`scripts/serial-loopback.sh` (new).

Fills the factory arm for `SessionType::Serial` in
`src/transport/tab.rs` — same file as P5-1; resolved at wave merge as a
union of two independent match arms (orthogonal, disjoint-intent).

- `serialport` 4.10.1, `default-features = false`; map doc 06 vocabulary
  (`none|odd|even|mark|space` → `Parity`, `none|hardware|software` →
  `FlowControl`, data bits 5–8, stop bits 1–2); reject unknown values as
  `ConnectionFailed` (never silently default).
- Blocking writes/reads on `spawn_blocking`; reader loop with a short
  read timeout pushing `DataReceived`; EOF/error ⇒ `ConnectionLost`
  (serial TTYs do not EOF; a read error like
  `ErrorKind::PermissionDenied` or device removal does).
- `resize` is a no-op returning `Ok(())` (doc 04); `set_tmux_enabled`
  accepted but unused.
- `description()` = `device@baud` e.g. `/dev/ttyUSB0@115200`.
- Device-missing connect must fail with `ConnectionFailed` and never
  panic.
- Unit: vocabulary mapping (all doc 06 values + unknown values),
  state machine, connect failure on a nonexistent device.
- Integration (`tests/serial_pty.rs`): open a real TTY via
  `libc::openpty` — serialport opens the slave side as a genuine serial
  device from the path; a plain `std::fs::File` writes into the master
  side; assert output arrives (with a short serialport timeout so an
  empty line reads early), input is taken, resize is `Ok`. Hardware
  runners with two USB-serial ports set `SERIAL_TEST_*`; absent env ⇒
  the test SKIPS with a println (existing convention).
- `scripts/serial-loopback.sh`: optional hardware path, skip-guarded
  doc, never run in CI.
- Verify (doc 10): a serial console shows output and takes input.

### P5-3 — Connect-flow wiring: `agent/phase5-connect`

Owns: `src/ui/tab.rs` (connect dispatch), `src/transport/tab.rs`
(factory factory-adjacent glue if needed), `src/ui/tab_state.rs`,
`src/ui/status_bar/*` if state mapping needs it, unit tests near the
consumers, `tests/local_serial_flow.rs` (new).

Depends on P5-1 + P5-2 (needs their constructors). Dispatches to its own
worktree only after both land on the wave branch.

- `SessionTab::open`: auto-connect Local/Serial like SSH (`start_connect`
  gains a per-type transport arm; the supervisor dial is SSH-only —
  local/serial connect through the factory path with `connection: None`,
  no dial, no prompts).
- `finish_connect` generalizes: no `SshConnection` for local/serial;
  sidebar/ssh-only edges stay SSH-gated; status text uses
  `transport.description()` rather than `user@host:port` when the
  session is not SSH.
- SFTP sidebar stays SSH-only (local/serial tabs get the plain terminal
  area — assert this in the flow test).
- Reconnect policy: local/serial tabs follow the same `reconnect` /
  `confirm-close` policy (they participate via the existing state
  events; no new state kinds).
- Flow test (`tests/local_serial_flow.rs`): open a Local session config
  against the FakeBackend wired to a real `LocalTransport`, drive
  echo/resize/exit through the pump/bridge path; the flow test may run
  display-free (pump/bridge are tokio, main-context drain uses
  `glib::MainContext::channel` if needed — keep the suite display-free
  per AGENTS.md; use the existing tab-wiring test as the pattern).
- Verify (doc 10): end-to-end — a local session opens into a running
  shell from the session tree.

### Wave close (orchestrator, no lane)

- Merge P5-1, P5-2 into `agent/phase5-wave`, then P5-3 on top; resolve
  the shared `tab.rs` arm as disjoint-intent (two orthogonal match arms
  — union them).
- Gates on the wave: `cargo fmt --check`, `cargo clippy --all-targets
  -- -D warnings`, `cargo test`, plus `scripts/run-sshd-tests.sh`
  (transport changes are in scope; doc 10).
- `docs/04-transport-layer.md` §Transport Factory says local+serial
  "return an error until Phase 5" — update to as-built, and §Local and
  serial (same doc) to what shipped.
- doc 10 Phase 5 row → done (with any gaps declared the way Phase 4
  did).
- PR to main; CI green; merge only on Alex's call.

## Acceptance (doc 10)

1. A local shell runs commands — proven by `tests/local_pty.rs` (real
   forkpty + bash round-trip).
2. A serial console shows output and takes input — proven by
   `tests/serial_pty.rs` (real TTY device via openpty; hardware path via
   SERIAL_TEST_* skip-guarded).
3. Every doc touched stays consistent (doc 04 §Factory + §Local/serial;
   doc 10 status table).

## Fleet recipe (per lane)

- Worktree FIRST (orchestrator): `git worktree add ../wayterm-<slug> -b
  agent/<slug> <base>`; herdr workspace create --cwd <worktree> --label
  <slug> --no-focus; herdr pane split in that workspace; herdr agent
  start <name> --kind claude --pane <pane_id>.
- Prompt via spec files at /tmp (never inline): `herdr agent prompt
  <name> "$(cat /tmp/spec-<slug>.md)"` — no `--` separator.
- Model: sonnet (resolves to claude-sonnet-5-5, verified by probe).
- Trust dialog: claude pre-trusts the repo dir on first launch (skill
  note) — check pane if it stalls at a dialog; send-keys Down+Enter only
  after reading the pane.
- Gates the lane runs itself (bare, no pipes): `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo
  build`. Report gate output in the lane report.
- Wire-or-delete rule for new items; no `#[allow]` walls; doc 04 is
  SHAPE authority, the crate APIs are NAME authority — verify real crate
  APIs before implementing (libc forkpty/forkpty is in the libc crate; serialport API
  from its 4.10.1 source in ~/.cargo/registry).