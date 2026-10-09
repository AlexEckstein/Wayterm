# Build Order

Vertical slices: every phase ends with an app that runs and is better than
the last. Phase numbers are referenced from code and other docs.

| Phase | Scope | Status |
|---|---|---|
| 1 | Skeleton: window, session tree, SQLite store, session dialog | done |
| 2 | SSH terminal tabs, host keys, full auth chain | done |
| 3 | SFTP sidebar | done |
| 4 | Hotkeys and polish | done, with gaps below |
| 5 | Local shell and serial | not started |
| 6 | VNC tabs | not started |
| 7 | RDP tabs | not started |
| 8 | tmux persistence | persistence done; reconnect wiring pending |
| 9 | Import/export, packaging | not started |
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

- `HotkeyEngine::install()` does not exist yet, so only `toggle_sftp` has
  a key; the other actions have none.
- Connected tabs are not registered as live (`register_live_tab`), so the
  `reconnect`/`disconnect` actions and confirm-before-close have nothing to
  act on.
- The disconnect-notification forwarder is not attached to tabs.
- The keyring credential source is a stub; secrets are prompted for.

## Phase 5: Local shell and serial

`LocalTransport` (`forkpty`, `AsyncFd`) and `SerialTransport`
(`serialport`), doc 04. **Verify:** a local shell runs commands; a USB
serial console shows output and takes input.

## Phase 6: VNC

`VncTab` (libvncclient or vnc-rs), Cairo framebuffer, key/pointer input,
password in the keyring, resize handling (doc 08). **Verify:** interact
with x11vnc or TigerVNC.

## Phase 7: RDP

Prototype X11 reparenting *first*, standalone. Then `RdpTab` with xfreerdp
(binary probed as `xfreerdp3`/`xfreerdp`), `/u` on argv and the password
on stdin, the certificate policy decision, XWayland detection (doc 08).
If reparenting is fragile, ship un-embedded and plan libfreerdp.
**Verify:** connect to Windows or xrdp; desktop visible and interactive;
resize works.

## Phase 8: tmux persistence

Done: `tmux new -A -s wayterm-<name>`, the dialog toggle and name, and an
sshd test proving the same session survives a dropped connection
(doc 04 §Tmux Session Persistence). Pending: drive reconnect from
`ConnectionLost` (see the Phase 4 gaps). Optional: kill the tmux session
on a graceful disconnect. **Verify:** run vim, kill the network,
reconnect, vim is still there.

## Phase 9: Import/export and packaging

`~/.ssh/config` import first (doc 06 — the adoption driver, offered on
first run), then JSON export/import, Remmina, MobaXterm, Asbru. Packages:
.deb, Flatpak (test portal access to SSH keys, the keyring, and files
early), AppImage, and a release workflow. Install a Wayterm icon.

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
