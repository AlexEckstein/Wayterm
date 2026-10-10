# RDP and VNC Embedding

RDP and VNC sessions are to be tabs in the same notebook as SSH, embedded
in-window rather than in separate windows. VNC is Phase 6 and shipped
(as built below); RDP is Phase 7 (doc 10).

## RDP

### Approach: xfreerdp subprocess + X11 reparenting

GTK 4 dropped `GtkSocket`/`GtkPlug`, so embedding is manual:

1. realize a container widget and take its X11 window id
   (`gdk4_x11::X11Surface::xid`);
2. spawn `xfreerdp /parent-window-id:<xid> /v:host:port /w: /h: /bpp:`
   (defaults 1024×768, 32 bpp);
3. find xfreerdp's child window with x11rb `query_tree` and resize it with
   `configure_window` when the container resizes.

**This is the project's highest-risk item.** No mainstream GTK 4 app embeds
a foreign X11 window this way — Remmina links libfreerdp and renders into
its own widget. Prototype the reparenting standalone before building any
UI; if it proves fragile, fall back to an un-embedded xfreerdp window and
plan the libfreerdp route.

| | Subprocess (MVP) | libfreerdp (likely successor) |
|---|---|---|
| Effort | small | large, under-documented C API |
| Isolation | a crash stays in xfreerdp | a crash takes the app down |
| Credentials | via stdin (below) | set in-process, never exposed |
| Rendering | reparented X11 window | our own GTK surface, Wayland-native |
| Cost | X11/XWayland only | follows FreeRDP's ABI churn |

### Credentials

Never on argv (`/proc/<pid>/cmdline` is world-readable), never in the
environment, never on disk. A spike on Debian 13 (FreeRDP 3.15, against
xrdp) established:

- `/from-stdin` exists in every FreeRDP since **2.0.0** (2017); every
  distro checked ships it. There is no "2.x without `/from-stdin`" case.
- **The binary name varies:** `xfreerdp3` on Debian 13, Ubuntu 24.04+,
  Arch; `xfreerdp` on Fedora, openSUSE, Ubuntu 22.04, Debian 12, RHEL 8.
  Probe for both.
- Pass the username on argv (`/u:user`, not a secret) and the password on
  stdin with `/from-stdin:force`: stdin then carries exactly one line, the
  password. (Without `/u`, `:force` reads the password *first*, then the
  username — an ordering trap avoided entirely by passing `/u`.)
- Lines are bare (no quoting; `\r` or `\n` ends a line, so a password
  cannot contain a newline). FreeRDP reads non-blockingly: write the line
  and keep the pipe open until the session is connected — closing early
  aborts with `ERRCONNECT_CONNECT_CANCELLED`.
- Do not set `FREERDP_ASKPASS` (environment-based, 3.10+ only).
- `/from-stdin` was buggy around 3.5–3.17 (fixed in 3.20); require
  **FreeRDP ≥ 3.20** or test on each target.

**Open design question — certificates.** With `/from-stdin`, xfreerdp
silently *rejects* any untrusted server certificate (even with
`/cert:tofu` and an empty `known_hosts`); only `/cert:ignore` or a
pre-populated FreeRDP `known_hosts` connects. Phase 7 must decide whether
Wayterm manages FreeRDP's `known_hosts` (its own TOFU policy, separate from
SSH's file) or accepts the MITM exposure of `/cert:ignore`.

### Wayland

Reparenting needs X11 or XWayland. Check which backend GDK actually
picked (`WAYLAND_DISPLAY` set and `GDK_BACKEND` not x11 means
Wayland-native); there, ask the user whether to relaunch under
`GDK_BACKEND=x11` or open xfreerdp un-embedded — never silently open a
second window.

## VNC (Phase 6, as built)

VNC renders natively: a `gtk::DrawingArea` blits a `cairo::ImageSurface`
that holds the framebuffer.

**Path B shipped.** The Path B condition ("a pure-Rust client if it
matures by Phase 6") was met by `vnc-rs` 0.6.0 (2026-09-21, tokio-native,
MIT/Apache-2.0): no FFI, and the whole client is a tokio task feeding the
same kind of channel as SSH output. Path A (libvncclient FFI) was never
needed. Layout (`src/vnc/`, `src/ui/vnc_tab.rs`):

- **Engine, display-free (`src/vnc/`).** `VncSession::connect` does the
  RFB handshake + auth (bounded at 15 s — the crate has no handshake
  timeout of its own), then `spawn()` runs the engine task: it owns an
  `Arc<Mutex<VncPixels>>` framebuffer (plain BGRA32 `Vec<u32>`, not a
  cairo type — cairo surface types are not `Send`), drains the input
  channel, maps `poll_event` results into framebuffer writes and UI
  events (`Dirtied`/`Resolved`/`Bell`/`Clipboard`/`Ended`), and closes
  the client on cancel/error (doc 04 §Threading model).
- **GTK side.** The tab's draw handler snapshots `VncPixels` into a
  cached `cairo::ImageSurface` (BGRA bytes; scaled draw pads the source
  so widget-edge blending doesn't darken border pixels) and paints.
  Input: `EventControllerKey` forwards keyvals as X11 keysyms (no table
  needed — GDK keyvals *are* keysyms), `EventControllerMotion` +
  `GestureClick` + scroll wheel build pointer events; view-only mode
  suppresses all input. VNC tabs attach no Tier-2 shortcut controller
  (§Tabs).
- **Credentials.** The VNC password resolves through the credential
  chain (`vnc_password` secret type, doc 06) before the engine starts;
  it is `Zeroizing` up to the crate boundary (the crate copies it to a
  `String` it gives no way to zeroize).

vnc-rs 0.6.0 behavior the code depends on (verified from its source;
verify again before a version bump — see `src/vnc/client.rs` doc
comments for the full list):

- Updates are **pull-only**: the crate requests one non-incremental
  frame after the handshake and never again. The engine sends
  `X11Event::Refresh` every 30 ms — without it the screen freezes after
  the first frame.
- `poll_event` never blocks (a `try_recv` on a 2-slot channel); the
  engine polls rather than blocking on the client mutex.
- Pseudo-encodings must be requested explicitly or their rects are
  fatal decode errors; `DesktopSizePseudo`/`CursorPseudo` are in the
  default set.
- Server hang-up is silent (no error event; the event channel closes).
- Non-ASCII passwords don't work (the crate uses the first 8 UTF-8
  bytes as the DES key). No quality/compression knob exists.

Wire verification: `tests/common/vnc.rs` is a hand-rolled RFB 3.8 mock
server (auth both paths, Raw/CopyRect/DesktopSize updates, client-event
recording, failure modes) driving `tests/vnc_wire.rs` (21 tests) and a
skip-guarded `tests/vnc_live.rs` (`VNC_TEST_*` env). The GTK widget
checks run under xvfb (`gtk_tests`).

## Tabs

The notebook does not care what a tab is: SSH, local, and serial build a
terminal tab; RDP and VNC build their own widget. RDP/VNC tabs attach no
Tier-2 shortcut controller, so only global accels intercept keys and the
rest reach the remote desktop (doc 07).

## v1 limitations

- RDP: X11/XWayland only; single monitor; FreeRDP 3 or later — the argv
  uses 3.x syntax (`/sec:rdp:off`), so a 2.x binary is refused at probe
  time with an error naming its version; credentials via stdin needs
  FreeRDP ≥ 3.20.
- VNC: plain VNC (tunnel it over SSH); VeNCrypt/TLS later. As built
  (Phase 6): `vnc_options.quality` is stored but not applied (the crate
  has no quality/compression knob in 0.6.0); `vnc_color_depth` is
  stored, not applied — the client always requests BGRA32;
  clipboard is receive-only (server → GTK; `CopyText` out is a
  follow-up); the remote cursor shape is not drawn (CursorPseudo is
  requested, the engine ignores the shape); Bell logs to the status
  bar only; the keyring source is the documented Phase 4 stub, so VNC
  prompts for the password on every connect (save-to-keyring is the
  app-wide follow-up, not VNC-specific); non-ASCII passwords do not
  authenticate (see the vnc-rs notes above).
