# RDP and VNC Embedding

RDP and VNC sessions are to be tabs in the same notebook as SSH, embedded
in-window rather than in separate windows. Not started: VNC is Phase 6,
RDP Phase 7 (doc 10).

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

## VNC

VNC renders natively: a `gtk::DrawingArea` blits a `cairo::ImageSurface`
that holds the framebuffer.

- **Path A — libvncclient via FFI.** A poll thread runs
  `WaitForMessage`/`HandleRFBServerMessage` and writes updated rects into
  the shared surface under a lock. Every widget call (`queue_draw`, the
  clipboard) hops to the main thread with `MainContext::invoke`.
  Configure the client through libvncclient's API, not by poking its
  struct. Send input to the poll thread over a channel rather than calling
  into libvncclient from GTK callbacks.
- **Path B — a pure-Rust client (vnc-rs)** if it matures by Phase 6: no
  FFI, and the poll loop becomes a tokio task feeding the same kind of
  channel as SSH output.

Keys need no table: GDK keyvals *are* X11 keysyms. Pointer input comes from
`EventControllerMotion` and `GestureClick`. The VNC password lives in the
keyring (doc 06).

## Tabs

The notebook does not care what a tab is: SSH, local, and serial build a
terminal tab; RDP and VNC build their own widget. RDP/VNC tabs attach no
Tier-2 shortcut controller, so only global accels intercept keys and the
rest reach the remote desktop (doc 07).

## v1 limitations

- RDP: X11/XWayland only; single monitor; credentials via stdin needs
  FreeRDP ≥ 3.20.
- VNC: plain VNC (tunnel it over SSH); VeNCrypt/TLS later.
