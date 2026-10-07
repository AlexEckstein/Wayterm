# Build Order / MVP Roadmap

## Philosophy

Build in vertical slices — each phase produces a working, runnable app
that's better than the last. Never have a phase that doesn't run.

---

## Phase 1: Skeleton (Weekend 1)

**Goal:** Empty app window with tab framework and session tree sidebar.

- [ ] Cargo project scaffold (GTK4, vte4, tokio, rusqlite, serde)
- [ ] Main window with `GtkNotebook` + `GtkPaned` (tree on left, tabs on right)
- [ ] Session tree (`GtkTreeView` + `GtkTreeStore`) with folder hierarchy
- [ ] SQLite session store via rusqlite (schema from 06-session-store.md)
- [ ] Add/edit/delete folders and sessions in the tree (CRUD UI)
- [ ] Session config dialog (host, port, username, type)
- [ ] Basic app lifecycle — open, close, save state

**Deliverable:** An app that launches, shows a session tree, lets you
create/edit/delete sessions, and has empty tabs. No connections yet.

**Verify:** `cargo run` → window appears, can add sessions, sessions
persist across restart.

---

## Phase 2: SSH Terminal (Weekend 2-3)

**Goal:** Working SSH terminal tabs. The first real functionality.

- [x] `TerminalBackend` trait (02-terminal-backend-abstraction.md) — `efd1d5a`
- [x] `VteBackend` implementation — `97e55ce`
  - Create VTE terminal widget, wire data flow
  - Connect VTE signals to event channel (try_send + idle-flush — the
    blocking_send variant is a UI-freezing bug, see doc 02)
- [x] `Transport` trait (04-transport-layer.md) — `ae73f23`
- [x] `SSHTransport` implementation — `ae73f23`
  - russh: connect, authenticate, channel, PTY, shell
  - **Push-based reader task** — `TransportEvent::DataReceived` to a glib
    channel; no `read()` polling (doc 04)
  - PTY initial size from the actual terminal, not 80×24
- [x] **Host-key verification** (11-ssh-auth-security.md) — Phase 2 is
  not done without this: `ae73f23`
  - `check_known_hosts_path` / `learn_known_hosts_path` on
    `~/.ssh/known_hosts`
  - changed-key hard fail; unknown-host prompt (fingerprint dialog)
    — **dialog not built**
  - `strict` default policy, per-session + global override
- [x] **Keyboard-interactive auth** (11-ssh-auth-security.md) — same
  milestone. `afd122e`
  - Challenge-response dialog (prompts with echo flags) — **not** built;
    the protocol side (challenge → response, echo flags) is
  - Full method fallback chain: agent → key → password →
    keyboard-interactive
- [x] `CredentialManager` with libsecret (keyring → prompt → store) — `afd122e`
- [ ] `TerminalTab`: wire transport ↔ backend (pump task, cancellation
  token — doc 04's Lifecycle section)
- [ ] Tab management: open, close, switch, reorder
- [ ] Status bar: connection state, host, duration
- [ ] **CI green** (11-ssh-auth-security.md §Testing): cargo
  fmt/clippy/test against a containerized sshd; FakeBackend transport
  tests for the pump/reader path — fmt/clippy/test are wired and the
  real-sshd suite exists, but the **auth-method matrix is not
  server-gated** and the FakeBackend pump/reader tests need the tab
  wiring above
- [ ] Spike (half-day, parallel): xfreerdp `/from-stdin` behavior on
  target distros — knowledge for Phase 7, cheap to do now

**Known doc corrections during Phase 2:** doc 02 `Send` bound removed
(`eb7bcad`); doc 11 known_hosts semantics for russh 0.64 (`a869541`,
`46b18f7`).

**Deliverable:** Click an SSH session → terminal tab opens → working SSH
terminal with verified host keys and full auth support. Multiple tabs.

**Verify:** Connect to a real host, run commands, output displays
correctly. Unknown host triggers the fingerprint prompt. Wrong password
retries. 2FA (keyboard-interactive) server works. Close tab, reopen.

**What "done" means here:** a MITM'd connection FAILS. Test it: put a
different key in known_hosts for the target host and confirm the app
refuses to connect. If that test doesn't exist, Phase 2 isn't done.

**Key Rust advantage in this phase:** The tokio async transport and
GTK main thread communicate via channels. No manual thread locking.
The compiler verifies no data races across the transport/backend
boundary. Concurrency bugs that would be runtime crashes in C++ are
compile errors here.

---

## Phase 3: SFTP Sidebar (Weekend 4)

**Goal:** SFTP file browser beside the terminal.

- [ ] `SftpSidebar` widget (09-sftp-sidebar.md)
  - russh-sftp: open SFTP channel over existing SSH connection
  - Directory listing (`GtkTreeView` + `GtkTreeStore`)
  - Navigation (double-click dir, path bar, back/forward)
  - Upload (button + drag-drop from local file manager)
  - Download (button + drag-drop to local file manager)
  - Delete, rename, mkdir
- [ ] Transfer progress dialog
- [ ] Toggle sidebar visibility (Ctrl+Shift+F)
- [ ] `GtkPaned` between sidebar and terminal (resizable)

**Deliverable:** SSH tab with SFTP sidebar. Browse remote files, upload/
download, drag-drop. The MobaXterm experience for SSH.

**Verify:** Open SSH session, sidebar shows remote files. Drag a file
from desktop → uploads. Right-click → download. Navigate directories.

**Key Rust advantage:** SFTP operations are async tasks on tokio,
sharing the same SSH session. No separate process (unlike KIO's SFTP
slave). No additional connection. Progress updates flow to the UI via
channels — no callback hell, no manual locking.

---

## Phase 4: Hotkeys & Polish (Weekend 5)

**Goal:** Configurable hotkeys and quality-of-life features.

- [ ] `ActionRegistry` + `HotkeyEngine` (07-hotkey-engine.md)
  - Register all actions with defaults
  - `Gtk.ShortcutController` + `Gtk.Shortcut` integration
  - Context switching (terminal vs tree focus)
- [ ] Settings UI for hotkey rebinding
  - List all actions, capture key, conflict detection
- [ ] Copy/paste in terminal
- [ ] Search in terminal scrollback (VTE built-in)
- [ ] Tab rename (double-click tab label)
- [ ] Reconnect on connection drop
- [ ] Confirm-before-close for active sessions
- [ ] Remember open tabs on exit, restore on launch
- [ ] Tray icon (minimize to tray, notifications for disconnects)

**Deliverable:** A polished SSH+SFTP client with customizable hotkeys.
Daily-driver quality for SSH work. This is the first publishable release.

**Verify:** Rebind a hotkey, it works. Close app with tabs open, reopen,
tabs restored. Connection drops, reconnect works.

---

## Phase 5: Local Shell + Serial (Weekend 6)

**Goal:** Support local terminal and serial connections.

- [ ] `LocalTransport` implementation (forkpty via nix crate)
  - forkpty, exec shell, async read/write via tokio::io::unix::AsyncFd
  - Working directory, environment variables
- [ ] `SerialTransport` implementation (serialport crate)
  - Configure baud, data bits, parity, stop bits, flow control
  - Async read/write bridge
- [ ] Session config UI for serial connections
- [ ] Test with real serial devices (USB-to-TTL, console cables)

**Deliverable:** Can open a local shell tab or a serial connection tab
alongside SSH tabs.

**Verify:** Open local shell, run commands. Open serial connection to
a device, see output, type commands.

---

## Phase 6: VNC (Weekend 7)

**Goal:** VNC tabs in-window.

- [ ] `VncTab` widget (08-rdp-vnc-embedding.md)
  - libvncclient FFI bindings (or pure Rust vnc-rs if mature enough)
  - Connect, framebuffer polling thread
  - GDK key/mouse → VNC events (GDK keyvals ARE X11 keysyms — direct mapping)
  - `GtkDrawingArea` + Cairo for framebuffer rendering
- [ ] VNC session config dialog (host, port, password, color depth)
- [ ] VNC password in keyring
- [ ] Resize handling (client-side scaling or server-side resize)
- [ ] VNC context hotkeys (most keys pass through, only tab mgmt intercepted)

**Deliverable:** Click a VNC session → tab opens → remote desktop visible,
keyboard/mouse work.

**Verify:** Connect to a VNC server (e.g. x11vnc or TigerVNC), see
desktop, click, type, interact.

---

## Phase 7: RDP (Weekend 8-9)

**Goal:** RDP tabs in-window via xfreerdp subprocess + X11 reparenting.

- [ ] `RdpTab` widget (08-rdp-vnc-embedding.md)
  - `gdk_x11_surface_get_xid()` to get container X11 window ID
  - Spawn xfreerdp with `/parent-window-id:<id>`
  - x11rb for child window discovery and resize
  - Password via stdin (not CLI args)
- [ ] RDP session config dialog (host, port, domain, resolution, color depth)
- [ ] RDP password handling (stdin auth)
- [ ] RDP context hotkeys (pass-through except tab management)
- [ ] Connection error handling (auth failure, host unreachable)
- [ ] XWayland detection and warning on Wayland-native
- [ ] De-risk: prototype X11 reparenting early in the weekend

**Deliverable:** Click an RDP session → tab opens → Windows desktop
visible, keyboard/mouse work.

**Verify:** Connect to a Windows RDP server or xrdp, see desktop,
interact, resize works.

**Note:** This is the highest-risk phase. GTK4's X11 embedding is more
manual than Qt's (no GtkSocket). Prototype the reparenting mechanism
*before* building the full RDP config UI. If it proves too fragile,
fall back to launching xfreerdp in a separate window (still functional,
just not in-window) while planning migration to libfreerdp C API.

---

## Phase 8: Tmux Persistence (Weekend 10)

**Goal:** Optional session persistence via tmux.

- [x] SSHTransport tmux integration (04-transport-layer.md)
  - `tmux new -A -s wayterm-<name>` instead of plain shell
  - Per-session toggle in session config — dialog checkbox + remote
    session name in the SSH group; stored in `tmux_enabled` /
    `tmux_session_name` (doc 06), consumed by `create_transport`
- [ ] Reconnect logic: detect drop, show "Reconnect?" prompt
  - the persistence half is done and proven
    (`tests/tmux_persistence.rs`: drop the transport *and* the
    connection, reconnect, same tmux session id + session state); the
    prompt/backoff loop itself is still to do
- [ ] Auto-reconnect option (configurable per-session)
- [ ] tmux session cleanup on graceful disconnect (optional)

**Deliverable:** SSH session with tmux enabled survives connection drops.
Reconnect → full state restored.

**Verify:** Enable tmux for a session, run vim, kill WiFi, reconnect,
vim is still there.

---

## Phase 9: Import/Export & Packaging (Weekend 11)

**Goal:** Shareable configs and installable packages.

- [ ] Import from `~/.ssh/config` (russh-config) — **the adoption driver,
  do this first**: first-run flow offers it, one saved session per Host
  block, wildcard blocks become templates
- [ ] Export sessions to JSON (without credentials)
- [ ] Import sessions from JSON
- [ ] Import from Remmina (.remmina files)
- [ ] Import from MobaXterm (.mxtpro) — if format is documented
- [ ] Import from Asbru (PAC YAML)
- [ ] Debian/Ubuntu packaging (.deb)
- [ ] Flatpak manifest (GTK4 apps are Flatpak-native)
- [ ] AppImage build
- [ ] GitHub Releases CI (build on tag push, cargo build --release)

**Deliverable:** Users can install via apt, AppImage, or Flatpak. Can
import sessions from other tools — most importantly, from their existing
`~/.ssh/config` on first run.

**Verify:** `dpkg -i wayterm.deb` → app works. Flatpak install → app
works. `~/.ssh/config` import creates the expected sessions. Import from
MobaXterm creates sessions.

---

## Phase 11: Jump Hosts & Port Forwarding (Weekend 13)

**Goal:** The MobaXterm use cases the v1 set lacked — tunnels and bastion
hops. (Design already exists: doc 11 §Jump Hosts and Port Forwarding,
schema in doc 06.)

- [ ] `SshConnection::open_direct_tcpip` (doc 04)
- [ ] Local port forwards: listener task per rule, one
  `copy_bidirectional` task per connection, per-session `forward_rules`
- [ ] Jump hosts: single-level ProxyJump via direct-tcpip through the
  jump session (`jump_session_id`, doc 06); independent auth per hop
- [ ] Tunnels sub-dialog: list rules with live status (listening /
  error / closed)
- [ ] Reconnect semantics: listeners re-established on reconnect
- [ ] Dynamic (SOCKS) and remote forwards: v2 — not this phase (doc 11
  marks the API coupling that makes remote forwards a deliberate v2 item)

**Deliverable:** A session can expose `localhost:8080 → remote-host:80`,
and connect through a bastion to reach hosts behind it.

**Verify:** Forward a port, curl it locally. Configure a jump session to
a host only reachable via the bastion, connect, works. Kill the
connection — listener drops; reconnect — listener returns.

---

## Phase 12: Testing Hardening (continuous, starts Phase 2)

**Goal:** Keep the trait-split advantage honest. The transports are
testable without a GUI; the CI proves it.

- [ ] Containerized sshd service in GitHub Actions (password, key, and
  keyboard-interactive auth modes — doc 11 §Testing)
- [ ] FakeBackend-based transport tests: pump path, reader path,
  cancellation, resize propagation
- [ ] Host-key decision-table tests (unknown/mismatch/accept-new/off,
  hashed entries)
- [ ] SFTP tests against russh-sftp's test server (list/download/upload)
- [ ] cargo-fuzz targets for known_hosts parsing and SFTP path joining
  (Phase 5+)
- [ ] `cargo fmt --check`, `clippy -- -D warnings`, `cargo audit` (nightly)

**Deliverable:** Green CI that actually exercises connections, not just
compilation.

**Verify:** Break the reader task deliberately (remove a `cancelled()`
branch) → a test fails. If not, the tests are theater.

---

## Phase 10: libghostty-vt Backend (Future, when API stabilizes)

**Goal:** Zero-dependency, high-performance terminal backend.

- [ ] Monitor libghostty-vt C API stability
- [ ] FFI bindings to libghostty-vt (via libloading or bindgen)
- [ ] `GhosttyBackend` trait implementation
  - Custom `GhosttyRenderWidget` (gtk4::Widget subclass)
  - Read ghostty render state → draw via Cairo (or OpenGL via gtk4 GL)
  - Key/mouse input → ghostty_input()
- [ ] Cargo feature: `backend-ghostty`
- [ ] Benchmark: compare performance vs VTE
- [ ] Bundle as zero-dependency plugin (statically link libghostty-vt)

**Deliverable:** Users can choose libghostty-vt as backend. No VTE
dependency needed for this mode. Best-in-class VT parsing performance.

---

## Summary

| Phase | What | Weekend | Working App? |
|---|---|---|---|
| 1 | Skeleton: window, tree, session store | 1 | Yes (empty) |
| 2 | SSH terminal tabs (+ host keys, full auth) | 2-3 | Yes (SSH client) |
| 3 | SFTP sidebar | 4 | Yes (SSH + file browser) |
| 4 | Hotkeys, polish | 5 | Yes (daily-driver SSH) |
| 5 | Local shell + serial | 6 | Yes (multi-transport) |
| 6 | VNC tabs | 7 | Yes (SSH + VNC) |
| 7 | RDP tabs | 8-9 | Yes (SSH + VNC + RDP) |
| 8 | Tmux persistence | 10 | Yes (resilient SSH) |
| 9 | Import/export (~/.ssh/config first), packaging | 11 | Yes (distributable) |
| 10 | libghostty-vt backend | future | Yes (zero-dep option) |
| 11 | Jump hosts + port forwarding | 13 | Yes (bastion/tunnels) |
| 12 | Testing hardening | continuous | (quality gate) |

After Phase 4, you have a genuinely useful, publishable open source tool.
Phases 5-9 make it a full MobaXterm alternative. Phase 11 closes the
tunneling gap that MobaXterm users cite most. Phase 10 is the
performance frontier.

## Risk Register

| Risk | Phase | Mitigation |
|---|---|---|
| GTK4 X11 embedding (RDP) has no mainstream precedent (Remmina embeds libfreerdp, not subprocess+reparent) | 7 | Standalone prototype BEFORE building UI. Fallback: separate window. Long-term: libfreerdp C API. |
| russh API instability (0.50 → 0.63 since these docs were first drafted) | 2 | Pin version; doc 11 keeps version anchors. Alternative: libssh via FFI. |
| FreeRDP 2.x on some distros lacks `/from-stdin` | 7 | Phase 2 spike verifies; fallback is xfreerdp's own GUI prompt (no embedded creds). Never argv/env. |
| Host-key UX friction (prompts) vs security | 2 | Defaults match OpenSSH norms (strict/TOFU); power users set `accept-new`. Never a silent accept. |
| vte4 crate API gaps | 2 | vte4 is well-maintained by gtk-rs team. File issues if bindings are missing. |
| libvncclient FFI complexity | 6 | Wrap in safe Rust types. Alternatively: use pure-Rust VNC client if one matures. |
| Flatpak portal permissions | 9 | Test early. SSH keys, keyring, and file access need portal permissions. |
| `~/.ssh/config` import surprises (wildcards, Includes, Match blocks) | 9 | russh-config handles the common cases; import is read-only and non-destructive; skip what we can't map safely. |

## Why This Roadmap Works in Rust

Each phase builds on the previous one with minimal rework because:

1. **Channels, not locks** — The transport/backend separation via
   tokio channels means adding new transports (serial, local) in Phase 5
   doesn't touch the terminal backend code at all.

2. **Trait objects** — The `TerminalBackend` trait means adding a new
   backend (ghostty) in Phase 10 is a new impl, not a refactor.

3. **Async by default** — All transports are async from day one (Phase 2).
   No retrofitting async into a sync codebase. SFTP (Phase 3) and VNC
   polling (Phase 6) are just more tokio tasks.

4. **Cargo features** — Backend selection is a Cargo feature flag. Users
   compile with `--features backend-ghostty` when that backend is ready.
   No plugin runtime, no dlopen, no packaging complexity.