# Phase 11 — Jump hosts and port forwarding (lane plan)

Date: 2026-10-10. Roadmap: docs/10 §Phase 11; contract: docs/11 §Jump hosts
and port forwarding; seams: docs/04 §Connection Supervisor.
Status line for doc 10 after merge: "Done" with the v2 exclusions in the
limitation notes.

## Goal

Two features over `SshConnection::open_direct_tcpip` (the seam doc 04
reserves for this phase):

1. **Jump hosts** — one level of ProxyJump: connect to the jump host, open a
   direct-tcpip channel to the target from there, and run the target's SSH
   session over that stream. Each hop authenticates on its own. Depth > 1
   is an error. Fallback for exotic setups: `ssh -W` as the stream.
2. **Local port forwards** — for each enabled `ForwardRule { kind: Local }`
   on an SSH session: a listener task per rule; one direct-tcpip channel per
   accepted connection; `copy_bidirectional`; listeners close with the
   connection and are re-established on reconnect. A Tunnels dialog shows
   live status.

Out of scope (doc 11 §Limitations): dynamic (SOCKS), remote, agent and X11
forwarding; jump depth > 1; host certificates.

## Verified current state (2026-10-10, main @ be3968f)

- Store: `sessions.jump_session_id` (FK, ON DELETE SET NULL) and
  `sessions.forward_rules` JSON exist and roundtrip (store unit tests).
- `ForwardRule { kind, bind_addr, bind_port, remote_host, remote_port,
  enabled }` in `src/store/model.rs`, wire format per doc 06.
- Import maps ssh-config `ProxyJump`/`ProxyCommand` to `jump_session_id`
  and drops depth>1 refs; the UI does not expose jump or forward fields
  yet ("exposes jump/forward fields" listed in doc 10 §Phase 9 gaps).
- `SshConnection::open_direct_tcpip` is a Phase-11 stub that returns
  `ChannelError` (src/transport/connection.rs:85).
- russh 0.64.1 pinned; verified APIs below transcribe
  `~/.cargo/registry/src/.../russh-0.64.1/src/client/mod.rs`.

## Verified russh 0.64.1 APIs (transcribed from the registry source)

- `Handle::channel_open_direct_tcpip(host_to_connect: impl Into<String>,
  port_to_connect: u32, originator_address: impl Into<String>, originator_port: u32)
  -> Result<Channel<Msg>, russh::Error>`
- `Channel::into_stream() -> ChannelStream<Msg>` (AsyncRead + AsyncWrite +
  Unpin + Send)
- `russh::client::connect_stream(config, stream, handler) -> Handle<H>`
  for `H: Handler + Send + 'static`, `R: AsyncRead + AsyncWrite + Unpin +
  Send + 'static` — the jump stream feeds a real russh client.
- `tokio::net::TcpListener`, `tokio::io::copy_bidirectional` per accepted
  connection.

## Design decisions (fixed; lanes implement, not relitigate)

- **Pool key stays `ConnectParams { host, port, username }`.** A
  jumped session dials the target over the jump stream; adding a bool
  would make direct and via-jump connections of the same host collide in
  the pool. Same target via two different bastions: the wave review can
  extend the pool key if a real use case needs it.
- **Supervisor dial path divergence.** Direct dials keep
  `get_or_connect`; a jumped session does NOT go through the supervisor —
  the connect flow, when `jump_session_id` is set, runs a dedicated
  `dial_via_jump` path: connect to the jump session (supervisor, recursive
  flow), then open direct-tcpip to the target, then `connect_stream` +
  auth chain on that stream. Rationale: the pool would hold a jumped
  connection whose lifetime is bound to the jump channel; eviction of one
  side would strand the other. A separate direct path keeps the supervisor
  semantics untouched; the wave review can revisit.
- **`ssh -W` fallback**: accepted at the tail of `dial_via_jump` when the
  direct-tcpip path errors with `ChannelError` AND the fallback is enabled
  by a `ssh_w_fallback: bool` (default false) in `SshOptions`; the lane
  runs the external `ssh -W` process and wraps it the same way.
- **SFTP over jump**: SFTP through a jumped session stays out of
  scope; the SFTP sidebar shows
  "unavailable for jumped sessions" in v1. Doc 11 note in the wave PR.
- **Listeners are transport-owned.** The supervisor owns connections, not
  forwards; the forward engine task is spawned by the connect flow per
  SSH tab and holds `Arc<SshConnection>`; it dies with the tab's cancel
  token, and the reconnect driver re-establishes listeners with the
  re-established connection.

## Lanes

### P11-1 `agent/p11-transport` — direct-tcpip, jump dial, forward engine (Rust lane, claude)

Files (owned): `src/transport/connection.rs` (fill the stub),
`src/transport/forward.rs` (new, engine), `src/transport/jump.rs` (new,
dial_via_jump + ssh -W fallback), `src/transport/mod.rs`,
`src/transport/supervisor.rs` (only if a pool-key change is wave-approved —
NOT this lane), `src/store/model.rs` (add `ssh_w_fallback` to `SshOptions`
default), tests: `tests/forward_sshd.rs` (new), `tests/jump_sshd.rs` (new).

Task text lives in `<worktree>/task-spec.md` (written by the orchestrator
before dispatch; untracked; delete at closeout).

Acceptance:
- `open_direct_tcpip` implemented per the pinned russh API; unit coverage
  in the module + doc 04/11 references kept honest.
- Jump dial: one level, independent auth per hop, depth>1 error, `ssh -W`
  fallback behind the flag, Cancelled propagates cleanly from either hop.
- Forward engine: listener per rule, one direct-tcpip channel + one
  copy_bidirectional per accepted conn, all enabled Local rules at connect,
  listeners re-established after a dropped connection (reconnect flow),
  CloseDrop on tab close; per-rule live status (Listening / Active conns
  count / Last error) observable by the UI through the established
  event pattern (an mpsc channel of ForwardEvent, module-level type).
- sshd tests: `tests/forward_sshd.rs` (curl a forwarded port through the
  CI/local sshd; listener accept + bidirectional copy asserted), 
  `tests/jump_sshd.rs` (reach a target only visible from a bastion, from
  the run-sshd-tests rig: target = the sshd loopback side bound to
  127.0.0.1 only... the lane may skip what the local rig can't express —
  in CI it's one sshd; the lane must state exactly what the rig expresses
  and what it cannot).
- Gates: fmt, clippy -D warnings, cargo test all green, sshd suites run
  real (not skipped) for the suites the rig can express.

Explicit non-goals: no UI, no dialog, no settings-page changes, no pool
key change, no dynamic/remote/agent/X11, no SFTP-over-jump.

### P11-2 `agent/p11-dialog` — Tunnels dialog + dialog-form fields (Rust lane, claude)

Files (owned): `src/ui/tunnels_dialog/` (new module dir: dialog +
tests), `src/ui/session_dialog.rs` + `src/ui/session_dialog/tests.rs` (add
Jump host + Forward rules editors), `src/ui/mod.rs` (module decls),
`src/ui/main_window.rs` (action + menu wiring for the Tunnels dialog).
Depends on: P11-1's `forward::ForwardEvent` type and jump availability
(type-only dependency; see API pin below).

Acceptance:
- Tunnels dialog: one row per rule — kind, bind, remote, live status
  (Listening/N connections/last error), enable toggle live-applies
  (starts/stops that rule's listener via the forward engine), rules
  editable per session (forward_rules edit + jump_session_id picker
  limited to SSH sessions, depth>1 chains rejected in the picker with a
  clear message).
- Session dialog: Jump host picker (SSH sessions, self/square excluded,
  depth>1 rejected) + forward-rules editor (kind Local only — Remote and
  Dynamic rows render but are disabled with a "v2" note, per wire-format
  stability).
- GTK/tokio discipline (docs 02/04): the dialog reads status through the
  channels the engine already publishes; no tokio on the GTK side, no
  locks across .await, no blocking_send on the UI path.
- Display-free suite kept display-free; xvfb smoke for the two dialogs
  (construct under xvfb, assert rows/dialog behave, delete the harness).
- Gates: fmt, clippy -D warnings, cargo test green; the forward-engine
  integration suite is NOT this lane's gate (P11-1's), but the lane pins
  P11-1's delivered forward-engine API (the real `ForwardEvent` type and
  whatever control surface P11-1 ships — transcribe from the merged
  source, never from this plan).

### Docs lane (in the wave, not dispatched — orchestrator + review)

After the lanes merge into the wave: status rows in doc 10 §Phase 11 and
the doc 11 limitation line ("dynamic and remote forwards are v2" stays),
plus "exposes jump/forward fields" removed from doc 10 §Phase 9 gaps when
the dialog lane lands; CI's transport_sshd step unchanged.

## Wave & PR (orchestrator)

- Wave branch `agent/phase11-wave` from origin/main, lanes merged in
  dependency order (P11-1 then P11-2),
- Full gates + real-sshd suites on the wave (curl the forwarded port via a
  real sshd, jump via the script rig if the sshd config allows it), CI must
  be green, then PR to main. Merge only on Alex's word.

## Verification rig

Local: `scripts/run-sshd-tests.sh` (throwaway sshd on 2422, sudo), which
runs transport_sshd, tmux_persistence, connect_flow, reconnect_sshd.
Add `--test forward_sshd` and `--test jump_sshd` to TEST_CMD in the
transport lane (script change owned by P11-1) — the script parses
per-test lines generically, so new targets are picked up by the summary.
Auth matrix only if auth semantics change — they don't here.

## Open decisions for Alex (recommendations inline)

1. Superseded-by-context pool-key question: same target via two different
   bastions → recommend leaving the pool key as-is for v1 (the dedupe is
   a nicety; two tabs = two connections already) and revisiting if a real
   use case needs it.
2. `ssh -W` fallback default: recommend OFF (default false), opt-in per
   session for exotic setups, matching "fallback for exotic setups, not a
   default path".
3. SFTP through a jumped session: recommend out of scope for v1 (doc 11
   doesn't require it); jump + SFTP is a follow-up doc note.