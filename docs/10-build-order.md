# Build Order

Vertical slices: every phase ends with an app that runs and is better than
the last. Phase numbers are referenced from code and other docs.

| Phase | Scope | Status |
|---|---|---|
| 1 | Skeleton: window, session tree, SQLite store, session dialog | done |
| 2 | SSH terminal tabs, host keys, full auth chain | done |
| 3 | SFTP sidebar | done |
| 4 | Hotkeys and polish | done, with gaps below |
| 5 | Local shell and serial | done |
| 6 | VNC tabs | done, with gaps below |
| 7 | RDP tabs | done |
| 8 | tmux persistence | done |
| 9 | Import/export, packaging | done, with gaps below |
| 10 | libghostty-vt backend | when its C API stabilizes |
| 11 | Jump hosts and port forwarding | not started |
| 12 | Testing hardening | continuous |

## Phase 1: Skeleton — done

Main window with the session tree beside the notebook; SQLite store
(doc 06); folder and session CRUD from the tree's context menu; the
session dialog with per-type groups.

## Phase 2: SSH terminal — done

`TerminalBackend` + `VteBackend` (doc 02); `Transport` + `SSHTransport`
with the push-based reader and the measured PTY size (doc 04); host-key
verification against `~/.ssh/known_hosts` with the unknown-host prompt
(doc 11); the agent → key → password → keyboard-interactive chain with the
challenge dialog; `TerminalTab` wiring; status bar; CI with a real sshd.

**Done means a MITM'd connection fails:** a changed key is rejected without
a prompt (tested). The changed-key *warning* dialog exists but is not shown
yet — the tab shows the rejection as a connection error.

## Phase 3: SFTP sidebar — done

Doc 09: per-tab sidebar over the shared connection, navigation, upload
(button and drag-drop), download (button), delete, rename, mkdir, inline
progress with cancel. Not done: a right-click menu and dragging files out.

## Phase 4: Hotkeys and polish — done, with gaps

Done: the action registry, conflict rules, default table, persistence, and
the settings window (doc 07); copy/paste/select-all/clear/search actions
and the search popover; double-click tab rename; the reconnect driver and
confirm-before-close policy (doc 04); open tabs remembered and restored;
tray icon with minimize-to-tray.

Gaps, each a wiring task rather than new design:

- ~~`HotkeyEngine::install()` does not exist yet~~ — done in Phase 8: the
  engine binds both tiers from the registry and rebuilds one binding on
  rebind (doc 07). Actions without a backing action (`new_tab`,
  `goto_tab_N`, splits, session-tree actions) still have keys but do
  nothing; their actions are open Phase 4+ work.
- ~~Connected tabs are not registered as live~~ — done in Phase 8: the
  connect flow registers every streaming tab, and `ConnectionLost` drives
  reconnect (auto, or prompt via the `reconnect` action).
- ~~The disconnect-notification forwarder is not attached to tabs~~ —
  done in Phase 8 (`tray::attach_disconnect_alerts`, per tab).
- The keyring credential source is a stub; secrets are prompted for.

## Phase 5: Local shell and serial — done

`LocalTransport` (`forkpty`, an `AsyncFd` reader, `TIOCSWINSZ` resize,
reap-on-exit) and `SerialTransport` (`serialport` opened by path,
blocking reads on `spawn_blocking`, no-op resize), both behind the
existing `create_transport` factory (doc 04 §Local and serial
transports). The connect flow opens local and serial sessions from the
tree: no dial, no sidebar, the transport's `description()` in the
status bar. Known gaps, each small:

- `mark`/`space` parity is stored but not representable by
  `serialport`; connecting with either fails clearly (doc 04 has the
  note). Linux `CMSPAR` support would lift it.
- The `SERIAL_TEST_PEER` hardware test (two cabled USB-serial ports)
  has not run against real hardware — the PTY-pair suite is the
  exercised path.
- Reconnect policy for a vanished device is untested.

## Phase 6: VNC — done, with gaps

Path B (`vnc-rs` 0.6.0, doc 08): `VncSession::connect` + engine task
(`src/vnc/`) owning a `VncPixels` framebuffer, `VncTab`
(`src/ui/vnc_tab.rs`) with the cairo blit, key/pointer input, view-only
mode, no Tier-2 shortcut controller; the password resolves through the
credential chain (`vnc_password`); tab open/close/retry goes through the
same session-tab flow as the other types. Protocol correctness comes
from a hand-rolled RFB 3.8 mock server (`tests/common/vnc.rs` →
`tests/vnc_wire.rs`, deterministic in CI); the real-server check ran
against x11vnc on this host (`tests/vnc_live.rs`, `VNC_TEST_*`).

Gaps, each small:

- `vnc_options.quality` and `vnc_color_depth` are stored but not
  applied (the crate has no quality knob; the client requests BGRA32).
- Clipboard is receive-only; the remote cursor shape is not drawn;
  Bell is status-bar-only.
- The keyring source is the documented Phase 4 stub — VNC prompts every
  connect (app-wide follow-up, not VNC-specific).
- Non-ASCII passwords don't authenticate (the crate uses the first 8
  bytes; doc 08 has the note).
- Tight/ZRLE decode through the mock server is not pinned (Raw/CopyRect
  prove the pipeline; ZRLE/Tight run only against real servers).

## Phase 7: RDP

Prototype X11 reparenting *first*, standalone. Then `RdpTab` with xfreerdp
(binary probed as `xfreerdp3`/`xfreerdp`), `/u` on argv and the password
on stdin, the certificate policy decision, XWayland detection (doc 08).
If reparenting is fragile, ship un-embedded and plan libfreerdp.
**Verify:** connect to Windows or xrdp; desktop visible and interactive;
resize works.

## Phase 8: tmux persistence — done

Done: `tmux new -A -s wayterm-<name>`, the dialog toggle and name, and
sshd tests proving the same session survives a dropped connection *and*
that a program started in it keeps running across the auto-reconnect
(the assertable form of "run vim, kill the network, reconnect")
(doc 04 §Tmux Session Persistence). Reconnect is driven from
`ConnectionLost`: automatic when `reconnect_on_drop` is on, prompt
otherwise; tray alert on every disconnect. Optional: kill the tmux
session on a graceful disconnect — deliberately not built; persistence
is the feature's point. **Verify:** run vim (or any long-running
command), kill the network, reconnect, vim is still there — covered by
the sshd suite.

## Phase 9: Import/export and packaging — done

Import/export (`src/transfer/`, doc 06 §Import and export): importers for
`~/.ssh/config` (Wayterm's own block scanner, `Include` followed),
Remmina, MobaXterm, Asbru-CM, and Wayterm JSON — pure readers, with
`apply_import` the single store writer; JSON export
(`{"version":1,"folders":[…]}`, no secrets). The Import/Export windows
(`src/ui/import_dialog.rs`) open from the session-tree menu and the
`win.import-sessions` / `win.export-sessions` actions, and the import is
offered once on first run when the store is empty. Packaging
(`packaging/`): a `.deb` via `cargo deb` (metadata in `Cargo.toml`), a
Flatpak manifest, a pkg2appimage AppImage recipe built from the `.deb`,
the Wayterm icon (SVG + 64/128/256 PNG) with the desktop entry and
AppStream metainfo, and `.github/workflows/release.yml`, which on a
`v*` tag attaches the `.deb` and AppImage to the GitHub Release.
**Verify:** the `src/transfer/` unit suite — `~/.ssh/config` forms,
templates, `Match`, the `Include` chain (cycles, depth cap, non-regular
files), `ProxyJump`, position remapping, a JSON export → import
roundtrip, newer-version refusal, no secret in an export, one fixture
per foreign format.

Gaps, each small:

- MobaXterm and Remmina field semantics are checked against
  hand-written fixtures only; a check against real exports is pending.
- The Flatpak build runs with `--share=network` so cargo can fetch
  crates — fine for local builds, not for Flathub (needs vendored cargo
  sources). The Flatpak is not built by the release workflow, and its
  sandbox access to SSH keys, the keyring and files is not yet verified.

## Phase 10: libghostty-vt backend

FFI bindings, a custom render widget, the `backend-ghostty` feature,
benchmarks against VTE (docs 03, 05).

## Phase 11: Jump hosts and port forwarding

`open_direct_tcpip`; local forwards (a listener per rule,
`copy_bidirectional` per connection); single-level ProxyJump with
independent auth per hop; a Tunnels dialog with live status; listeners
re-established on reconnect. Dynamic and remote forwards are v2 (doc 11).
**Verify:** curl a forwarded port; reach a host only visible from a
bastion.

## Phase 12: Testing hardening

Done: unit and display-free integration tests; a real-sshd suite in CI;
host-key decision tests; FakeBackend pump/bridge tests; an auth-method
matrix against single-method sshds (run locally, doc 11 §Testing
Strategy). To do: run `connect_flow`, `tmux_persistence` and
`reconnect_sshd` in CI too (they need only the CI sshd); `cargo-fuzz`
targets for known_hosts handling and SFTP path joining.

## Risks

| Risk | Phase | Mitigation |
|---|---|---|
| X11 reparenting of xfreerdp has no GTK 4 precedent | 7 | standalone prototype first; fall back to an un-embedded window; long term libfreerdp |
| xfreerdp rejects untrusted certs under `/from-stdin` | 7 | decide: own FreeRDP's `known_hosts` vs `/cert:ignore` |
| FreeRDP `/from-stdin` regressions before 3.20; binary name varies | 7 | require ≥ 3.20 or test per distro; probe both names |
| russh API churn | all | pinned; version anchors in doc 11 |
| Host-key prompts annoy power users | 2 | OpenSSH-like defaults; `accept-new` per session; never silent accept |
| libvncclient FFI complexity | 6 | safe wrapper, or vnc-rs |
| Flatpak portal permissions | 9 | test early |
| `~/.ssh/config` oddities (Include, Match) | 9 | import what maps safely; read-only |
