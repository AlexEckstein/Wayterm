# Phase 8 — tmux persistence: completion

Status: lanes dispatching · 2026-10-09 · Orchestrator: Hermes (manager profile) ·
Fleet: Claude Code one-shot lanes (this phase's standing instruction).

## Goal

Finish doc 10 Phase 8: a transport drop is noticed, the tab reconnects
(policy-driven), and — with tmux enabled — the same remote session
reattaches with its running programs intact. The exec/dialog/sshd core
(`tmux new -A -s wayterm-<name>` + `tests/tmux_persistence.rs`) is already
merged; what remains is the connection-lost → reconnect wiring that doc 10
deferred to the Phase 4 gaps, plus the hotkey install that gives
`reconnect`/`disconnect` (and the rest of doc 07's table) their keys.

## Current state (verified on main 17900aa)

- `SSHTransport` execs `tmux new -A -s <shell_quoted name>` when
  `tmux_session_suffix()` yields a name (src/transport/ssh.rs).
- Store carries `tmux_enabled` / `tmux_session_name`; model helpers
  (`tmux_remote_name`, `tmux_name_for_dialog`, `strip_tmux_prefix`) exist;
  the session dialog edits both.
- `tests/tmux_persistence.rs` (real sshd) proves: first connect runs
  inside the named session; dropping transport + connection leaves the
  session alive; a fresh connection reattaches (same `#{session_id}` +
  session env); the off-path creates no session; the serial flag is inert.
- `ConnectionSupervisor` shares one connection per (host, port, username);
  `ReconnectDriver` + `tab_resilience` (registry, actions, prompt,
  confirm-close) are built and unit-tested but wired to nothing: no tab
  ever `register_live_tab`s, `forward_disconnect_alerts` has no caller,
  and `HotkeyEngine::install()` does not exist (doc 07).

## Design decisions (orchestrator, settled 2026-10-09 — do not relitigate)

**1. The `LiveTab` connect closure is the redial path.** At registration
the tab captures a closure that redials fresh through the supervisor
(`get_or_connect`) and rewires the page through the existing
`start_connect`/`finish_connect` machinery with a new generation. The
driver stays a policy engine: backoff, attempts, prompt, `Reconnecting
{ attempt }` on the state channel. No `Transport`/`TerminalBackend` trait
changes. After a driver-driven reconnect the page must reflect the new
attempt: fresh backend in `TERMINAL_BACKEND_KEY`, status drain attached,
sidebar reattached (same shared connection when the supervisor still holds
one; fresh dial when the TCP session died — the tmux case).

**2. Connection-loss handling, per event.** `spawn_state_drain` maps:

- `StateChanged` → `set_tab_state` (registry follows);
- `ConnectionLost` → registry state, tray alert
  (`forward_disconnect_alerts` on the state channel), and — when
  `reconnect_on_drop` is on — start the driver through
  `activate_reconnect`. Prompt mode keeps the existing manual
  `reconnect` action; no new dialog.

**3. Kill-tmux-on-graceful-disconnect: parked, recommend not building.**
Doc 10 marks it optional; persistence is the feature's point, and killing
the session on a user "Disconnect" defeats it unless the user asked.
Park it unless Alex wants it (open decision below).

**4. Tab teardown keeps its contract.** Close paths go through
`request_close`/`teardown_tab` so no reconnect loop outlives its tab.

## Lanes

### P8-1 — Reconnect driven by ConnectionLost (branch `agent/p8-reconnect-wiring`)

Files: `src/ui/tab.rs`, `src/ui/tab_resilience.rs` (+ tests), `src/ui/tray.rs`
(call sites), `src/ui/main_window.rs` (install call), `tests/tmux_persistence.rs`
or `tests/reconnect_sshd.rs`. Nothing else; do not touch `src/hotkeys/` or
`docs/07-hotkey-engine.md`.

Tasks:

1. Register every streaming transport tab as a `LiveTab` in
   `finish_connect` (per design decision 1); `unregister`/
   `teardown_tab` on close; keep `SESSION_ID_KEY` as the key.
2. `spawn_state_drain` updates the registry and, on `ConnectionLost`,
   alerts the tray and starts the driver per policy (decision 2).
3. Wire `install_connection_actions` at window build; attach the alert
   forwarder.
4. Tests: registry/state unit tests; an sshd test proving a dropped
   connection **auto-reconnects** and, with tmux on, a program started in
   the session (a long-running command — the assertable stand-in for doc
   10's "run vim") is still running after reattach.

Acceptance: gates pass; sshd suites skip cleanly without env; no GTK calls
from tokio threads, no `blocking_send` on the UI path, logic stays in
`transport/`/`tab_resilience` (UI thin).

### P8-2 — HotkeyEngine::install() (branch `agent/p8-hotkeys-install`)

Files: `src/hotkeys/engine.rs`, `src/hotkeys/defaults.rs`,
`src/ui/main_window.rs`, `docs/07-hotkey-engine.md` (drop the "Not built
yet" note). Nothing else; do not touch `src/ui/tab.rs`,
`tab_resilience.rs`, `tray.rs`.

Tasks:

1. `install()`: bind Tier-1 accels from the registry (copy, paste,
   select-all, clear, search, toggle_sftp, reconnect, disconnect, and the
   rest of the default table) and Tier-2 terminal-context controllers;
   unbind+rebind one action on `rebind`/`reset`/`set_enabled`.
2. Install point: window build, alongside `install_toggle_sftp`.
3. Tests: engine-level install/rebind tests (accel wiring asserted where
   GTK allows; registry mutations unit-tested as today).

Acceptance: keys activate the window actions; a rebind takes effect
without reinstalling everything; doc 07 matches the code.

`main_window.rs` is the one shared file between the lanes (different
hunks: actions install vs accel install flagging) — reconciled at
integration as disjoint-intent.

### P8-3 — docs refresh (after the wave, branch `docs/phase8-refresh`)

Small `docs:` lane: AGENTS.md "Current state" (still says Phases 1–4) and
its repo-layout block (missing `src/vnc/`, `src/rdp/`); doc 10 Phase 4/8
trail sync. Must land after or with the wave so it describes reality.

## Verification plan

- Each lane: `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test` in its worktree.
- Wave (orchestrator, not lanes): all three gates on the integrated
  branch, then `scripts/run-sshd-tests.sh` — `tests/tmux_persistence.rs`,
  `tests/reconnect_sshd.rs`, `tests/transport_sshd.rs` must be reported
  as *run*, not skipped ("a green skip proves nothing"). The suite drops
  a whole TCP session, which is doc 10's "kill the network" for
  practical purposes; the program-survives test added by P8-1 is the
  standing assertable proof.
- Doc cross-links stay green (CI docs job).

## Open decisions for Alex

1. **Merge**: wave PR to `main` opens after verification; lands on your
   explicit "merge".
2. **Optional kill-on-disconnect**: parked (decision 3). Say the word if
   you want it as a per-session choice; default is not to build it.