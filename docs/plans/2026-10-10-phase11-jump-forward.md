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
   is an error. Fallback for exotic setups: `ssh -W` as the stream (an
   explicit per-session opt-in, see §Design decisions).
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
  yet. (Doc 10 §Phase 9 gaps does not record this; P11-2 closes it, so
  no doc 10 gap line is needed.)
- The connect flow is UI-side: `start_connect` → supervisor dial →
  `finish_connect` in `src/ui/tab.rs`; the reconnect driver redials
  through `redial_fn` → `redial` → `start_connect` in the same file
  (`src/transport/reconnect.rs` only drives the `ConnectFn`).
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

- **Pool key stays `ConnectParams { host, port, username }`.** Jumped
  connections never enter the pool (next item), so a direct and a
  via-jump connection to the same `user@host:port` cannot collide and no
  key change is needed. Dedupe of jumped connections (same target via
  the same bastion) is lost; two tabs = two connections, accepted for v1.
- **Supervisor dial path divergence.** Direct dials keep
  `get_or_connect`; a jumped session does NOT go through the supervisor —
  the connect flow, when `jump_session_id` is set, runs a dedicated
  `dial_via_jump` path: connect to the jump session (supervisor, recursive
  flow), then open direct-tcpip to the target, then `connect_stream` +
  auth chain on that stream. Rationale: the pool would hold a jumped
  connection whose lifetime is bound to the jump channel; eviction of one
  side would strand the other. A separate direct path keeps the supervisor
  semantics untouched; the wave review can revisit. **This diverges from
  doc 04 §Connection Supervisor** ("shares one connection per (host,
  port, username)"), so per AGENTS rule 1 it is an open decision for
  Alex (below), and if accepted the docs lane adds the exception to
  doc 04.
- **A jumped connection holds its bastion.** The target's
  `SshConnection` keeps the jump host's `Arc<SshConnection>` for its whole
  life, not just the `ChannelStream`: the supervisor's monitor evicts a
  pooled connection once only the pool's own reference is left (doc 04),
  and a stream alone does not count, so without the `Arc` the bastion
  would be disconnected under a live target. Dropping the target drops
  the hold.
- **`ssh -W` fallback is an explicit mode, not an automatic retry.**
  `ssh -W` opens a direct-tcpip channel itself, so a bastion that
  refuses ours (`AllowTcpForwarding no`, `PermitOpen`) refuses it too:
  retrying on `ChannelError` would only repeat the failure. What it does
  add is OpenSSH's own client for the jump hop — `~/.ssh/config`,
  ControlMaster, GSSAPI, FIDO keys, an agent — i.e. the exotic setups
  doc 11 means. So `SshOptions.ssh_w_fallback: bool` (default false)
  *replaces* the russh jump hop when set: the lane spawns
  `ssh -W <target>:<port> <jump>` and runs the target's russh session
  over its stdio. Security consequences, stated in the dialog and in
  doc 11 by the docs lane: on that hop Wayterm's host-key policy and
  keyring auth do not apply (OpenSSH's `known_hosts`, agent and config
  do); the target hop is still verified and authenticated by Wayterm.
  `BatchMode=yes` is passed so OpenSSH never prompts on a tty Wayterm
  does not own; its stderr becomes the connect error.
- **SFTP over jump**: SFTP through a jumped session stays out of
  scope; the SFTP sidebar shows
  "unavailable for jumped sessions" in v1. Doc 11 note in the wave PR.
- **Listeners are transport-owned, tunnels live with the tab (v1).** The
  supervisor owns connections, not forwards; the forward engine task is
  spawned by `finish_connect` per SSH tab and holds `Arc<SshConnection>`;
  it dies with the tab's connection (close, cancel, or the
  `release_connection` before a redial), and because every redial goes
  back through `finish_connect`, listeners return with the re-established
  connection without a separate reconnect hook. Consequences, accepted
  for v1 and visible in the UI: closing the terminal tab closes its
  tunnels (no tunnel-only sessions — open decision 4); a bind conflict
  (another session, another window's supervisor, an outside process)
  fails *that rule* with `EADDRINUSE` as its Last error, never the
  connect or the other rules.
- **Loopback by default.** A new rule's bind address is `127.0.0.1`
  (doc 06's example). A non-loopback bind (`0.0.0.0`, `::`, a LAN
  address) exposes the tunnel to the network: the rule editor shows an
  inline warning on such a rule, and saving it needs a confirmation.
  The engine binds exactly what the rule says; validation is UI-side.
- **One place edits rules.** Forward rules and the jump host are
  session configuration and are edited only in the session dialog.
  The Tunnels dialog is the live view of the current tab: status per
  rule plus a runtime on/off switch that starts or stops that rule's
  listener for this connection only — it does not write the store and
  is labelled as such — and an "Edit rules…" button that opens the
  session dialog. Two editors for one column would race and confuse.

## Lanes

### P11-1 `agent/p11-transport` — direct-tcpip, jump dial, forward engine (Rust lane, claude)

Files (owned): `src/transport/connection.rs` (fill the stub),
`src/transport/forward.rs` (new, engine), `src/transport/jump.rs` (new,
dial_via_jump + ssh -W fallback), `src/transport/mod.rs`,
`src/ui/tab.rs` — **connect-flow wiring only** (the `jump_session_id`
branch in `start_connect`, spawning the forward engine in
`finish_connect` and stopping it in `release_connection`/close, and
publishing the engine's control handle and `ForwardEvent` receiver on
the page for P11-2; no widgets, dialogs or menus),
`src/transport/reconnect.rs` (only if the wiring above proves to need
it),
`src/transport/supervisor.rs` (only if a pool-key change is wave-approved —
NOT this lane), `src/store/model.rs` (add `ssh_w_fallback` to `SshOptions`
default), tests: `tests/forward_sshd.rs` (new), `tests/jump_sshd.rs` (new).

Task text lives in `<worktree>/task-spec.md` (written by the orchestrator
before dispatch; untracked; delete at closeout).

Acceptance:
- `open_direct_tcpip` implemented per the pinned russh API; unit coverage
  in the module + doc 04/11 references kept honest.
- Jump dial: one level, independent auth per hop, depth>1 error, `ssh -W`
  mode behind the flag (replaces the russh hop, `BatchMode=yes`, never an
  automatic retry), Cancelled propagates cleanly from either hop, the
  target holds the jump host's `Arc<SshConnection>` (unit test: the
  bastion stays pooled while the target lives and is evicted after).
- Forward engine: listener per rule, one direct-tcpip channel + one
  copy_bidirectional per accepted conn, all enabled Local rules at connect,
  listeners re-established after a dropped connection (reconnect flow),
  CloseDrop on tab close; per-rule live status (Listening / Active conns
  count / Last error) observable by the UI through the established
  event pattern (an mpsc channel of ForwardEvent, module-level type); a
  per-rule start/stop control for the Tunnels switch; a bind failure
  (`EADDRINUSE`) is that rule's Last error and leaves the connect and the
  other rules up (unit test).
- sshd tests: `tests/forward_sshd.rs` (curl a forwarded port through the
  CI/local sshd; listener accept + bidirectional copy asserted), 
  `tests/jump_sshd.rs` (reach a target only visible from a bastion, from
  the run-sshd-tests rig: target = the sshd loopback side bound to
  127.0.0.1 only... the lane may skip what the local rig can't express —
  in CI it's one sshd; the lane must state exactly what the rig expresses
  and what it cannot).
- Gates: fmt, clippy -D warnings, cargo test all green, sshd suites run
  real (not skipped) for the suites the rig can express.

Explicit non-goals: no widgets or dialogs (the `tab.rs` wiring above is
the only UI file), no settings-page changes, no pool key change, no
dynamic/remote/agent/X11, no SFTP-over-jump.

### P11-2 `agent/p11-dialog` — Tunnels dialog + dialog-form fields (Rust lane, claude)

Files (owned): `src/ui/tunnels_dialog/` (new module dir: dialog +
tests), `src/ui/session_dialog.rs` + `src/ui/session_dialog/tests.rs` (add
Jump host + Forward rules editors), `src/ui/sftp_sidebar.rs` (the
"unavailable for jumped sessions" state), `src/ui/mod.rs` (module decls),
`src/ui/main_window.rs` (action + menu wiring for the Tunnels dialog).
Not `src/ui/tab.rs`: the dialog reads the handle P11-1 publishes on the
page.
Depends on: P11-1's `forward::ForwardEvent` type and jump availability
(type-only dependency; see API pin below).

Acceptance:
- Tunnels dialog (current tab, live view only): one row per rule —
  bind, remote, live status (Listening / N connections / last error),
  and a switch that starts or stops that rule's listener for this
  connection only (labelled "until reconnect"; the store is untouched);
  an "Edit rules…" button opens the session dialog. No rule or jump
  editing here. Rules the engine skipped (disabled in the store, or a
  non-Local kind from an import) are listed greyed with the reason.
- Session dialog: Jump host picker (SSH sessions only; the session
  itself and any session that already has a jump host are excluded,
  with a note that one hop is the limit) + forward-rules editor. The
  editor offers Local only — no disabled Remote/Dynamic choices; a
  non-Local rule loaded from the store or an import keeps its row,
  read-only, marked "not supported yet", so it roundtrips untouched.
  New rules default to bind `127.0.0.1`; a non-loopback bind shows an
  inline warning and saving asks for confirmation.
- SFTP sidebar on a jumped session: an "SFTP is not available for
  jumped sessions yet" state instead of an attach attempt.
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
the doc 11 limitation line ("dynamic and remote forwards are v2" stays);
doc 11 §Jump hosts gains the `ssh -W` mode's trust note (OpenSSH, not
Wayterm, verifies and authenticates that hop) and the "no SFTP over a
jumped session" limitation; doc 04 §Connection Supervisor gains the
jumped-connection exception (outside the pool, holds its bastion's
`Arc`) if Alex accepts decision 1; doc 10 §Phase 11 records "tunnels
close with their tab" as a v1 limitation. CI's transport_sshd step
unchanged.

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

1. Jumped sessions bypass the supervisor (a doc 04 divergence) →
   recommend accepting it for v1 with the doc 04 exception, rather than
   teaching the pool about connections that depend on another pooled
   connection. Consequence: no dedupe of jumped connections.
2. `ssh -W` mode: recommend OFF by default, opt-in per session, and only
   as a replacement for the russh hop (not a retry, which cannot help:
   it uses direct-tcpip too). The trust trade-off (OpenSSH owns that
   hop's host keys and auth) is shown next to the switch.
3. SFTP through a jumped session: recommend out of scope for v1 (doc 11
   doesn't require it); jump + SFTP is a follow-up doc note.
4. Tunnels without a terminal (tunnels that outlive their tab, or a
   tunnel-only session): recommend v2. v1 ties tunnels to the tab, which
   matches doc 11 ("listeners close with the connection") and keeps the
   engine's lifetime simple; the UI says so.