# Remote Desktop: RDP and VNC

RDP and VNC sessions are tabs in the same notebook as SSH, embedded in the
window rather than opened separately. Neither is a byte stream, so neither
implements `Transport` (doc 03 §The Transport trait): each has its own
connect flow, engine and widget, and publishes the same `TransportState`s
to the status bar.

Code: `src/rdp/` (xfreerdp process, certificate trust; display-free),
`src/ui/rdp_tab.rs` and `src/ui/rdp_tab/` (embedding, dialogs);
`src/vnc/` (RFB engine, framebuffer; display-free), `src/ui/vnc_tab.rs`.

## Tabs

The notebook does not care what a tab is. RDP and VNC tabs have no
terminal and no shortcut context of their own, so only global shortcuts
intercept keys; everything else reaches the remote desktop (doc 11
§Dispatch). They have no SFTP sidebar and no macro tap. Both go through
the shared connect machinery (generation guard, connecting / error /
disconnected views, Retry, close, doc 03 §Connect flow); dropped sessions
show the error view with Retry rather than reconnecting automatically.

## RDP

### Embedding

GTK 4 has no `GtkSocket`/`GtkPlug`, so Wayterm runs `xfreerdp` as a
subprocess and reparents its window into the tab (X11 or XWayland only):

1. The tab realizes an X11-backed `DrawingArea` and polls (every 50 ms)
   until its surface downcasts to `gdk4_x11::X11Surface` and has an XID.
2. xfreerdp is spawned with `/parent-window:<xid>` (the flag is
   `/parent-window:`, not `/parent-window-id:`).
3. One x11rb connection on the GTK thread snapshots the host window's
   children *before* the spawn, then discovery (after about 1 s, again at
   about 3 s) tracks only the newcomer past that baseline. A pre-existing
   child is never ours: GTK keeps its own children under the toplevel, and
   an orphan from a killed earlier run may linger.
4. `ConfigureWindow` keeps the tracked child at the area's origin and
   size, on every resize plus a 250 ms poll fallback; it is unmapped while
   the tab is hidden.
5. X input focus is handed to the child when focus sits on the parent —
   without the handoff there is no keyboard under a window manager.

Only the tracked window is configured or focused, and teardown (close,
Retry, a superseded attempt) terminates only our own process group:
`SIGTERM`, then `SIGKILL` after 2 s.

The remote desktop's resolution is fixed at connect time (`rdp_width` ×
`rdp_height`, default 1024×768); resizing the tab moves and resizes the
embedded window but does not change the remote resolution.

### Command line

`rdp::process::build_args` emits, in order: `/v:<host>:<port>`,
`/u:<username>`, `/d:<domain>` (if set), `/w:` and `/h:`, `/bpp:` (if
set; 16, 24 or 32), `/from-stdin:force` and `/sec:rdp:off` (always), the
feature toggles, `/parent-window:<xid>` (when embedded), and the
certificate token (`/cert:fingerprint:sha256:<hex>`, when resolved).
`/cert:ignore` and `/p:` are never emitted; a test pins both.

- **Toggles** come from `rdp_options` (default: clipboard, fonts and
  printer on, audio off) and `rdp_compression` (default on). Clipboard,
  fonts and compression are always spelled out (`+x`/`-x`) since they
  default on upstream; `/audio` and `/printer` are passed only when
  enabled.
- **`/sec:rdp:off`** keeps the certificate pin meaningful. Legacy
  "Standard RDP Security" has no TLS handshake, so a pin is never
  consulted: a peer refusing TLS would get a session — and the auto-logon
  password — despite a pin that cannot match. With the flag the client
  refuses (`ERRCONNECT_SECURITY_NEGO_CONNECT_FAILED`); TLS and NLA servers
  are unaffected.
- **Environment:** the child gets only `DISPLAY`, `XAUTHORITY`, `PATH`,
  `HOME` and `XDG_RUNTIME_DIR` (the audio socket) — all paths, no secrets.
- **stderr** goes into a 200-line ring used only for the error view, never
  logged.

### Credentials

The password resolves through the credential chain as `rdp_password`
(doc 05) and goes to xfreerdp on **stdin**, never argv (world-readable in
`/proc/<pid>/cmdline`), the environment, or disk:

- The username travels on argv (`/u`, not a secret); with
  `/from-stdin:force`, stdin then carries exactly one line, the password.
  Without `/u`, `:force` reads the password *first*, then the username — a
  trap avoided by always passing `/u`.
- Lines are bare (`\r` or `\n` ends one), so a password is truncated at its
  first newline rather than sending a second line.
- The pipe stays open for the whole session: closing stdin early aborts
  the connect with `ERRCONNECT_CONNECT_CANCELLED`. Only the in-memory copy
  of the password is dropped (zeroized) after the write.
- `FREERDP_ASKPASS` is not used.

### Certificates

Wayterm manages RDP server trust itself, SSH-style (TOFU), in the
`rdp_trust` table (host, port, SHA-256 fingerprint, first seen;
doc 09 §Schema). In fingerprint mode xfreerdp writes nothing to its own
known-hosts store, so the table is the single source of truth.

| Stored | Seen | Outcome |
|---|---|---|
| none | fp | prompt (fingerprint shown); Accept records it and connects |
| fp | same fp | connect with the pinned token; xfreerdp enforces it |
| fp | other | warning dialog: Disconnect (default) or Update and Connect |
| any | none | error: no server certificate to check |

- **First contact** learns the fingerprint without trusting it: a probe
  run pins the all-zero fingerprint, which no certificate can match, so
  xfreerdp refuses at TLS — after printing what it saw on stderr, before
  any credential could leave — and exits. The probe gets an empty password
  regardless, and times out after 5 s.
- **A changed certificate** is caught when a pinned connect ends before it
  connected and stderr shows a different fingerprint. The warning shows
  both fingerprints and the MITM caution; only an explicit Update and
  Connect records the new one and retries.
- An unreadable trust store fails the connect: it is not treated as first
  contact, which would let the user re-pin a host whose pin merely failed
  to load.
- Fingerprints are 64 lowercase hex characters, no colons.

### Connected state

xfreerdp has no machine-readable "connected" signal, so the tab reports
`Connected` once the child has lived 15 s without exiting and stderr holds
no `ERRCONNECT_` line; until then the connecting overlay covers the area.
When the child exits, the error view shows the exit code or signal and the
stderr tail.

### FreeRDP version

The binary is probed once per process, as `xfreerdp3` then `xfreerdp` on
`PATH` (`xfreerdp3` on Debian 13, Ubuntu 24.04+, Arch; `xfreerdp` on
Fedora, openSUSE and older releases).

- The argv speaks FreeRDP 3 only (`/sec:rdp:off`; 2.x spelled it
  `-sec-rdp`), so a 2.x binary — or an unreadable version — is refused
  with an error view naming the version and saying to install FreeRDP 3.
- `/from-stdin` was buggy around 3.5–3.17 (fixed in 3.20). Below 3.20 the
  connecting overlay carries a warning; the connect still proceeds.

### Wayland

Reparenting needs X11. When the area's surface is not an X11 surface (GDK
picked Wayland), the tab offers a choice instead of embedding: relaunch
Wayterm under `GDK_BACKEND=x11` (the current instance exits after
respawning itself), or open xfreerdp as a separate top-level window.
Wayterm never silently opens a second window.

## VNC

VNC renders natively: a `gtk::DrawingArea` paints the framebuffer through
cairo. The client is `vnc-rs` 0.6.0 (pure Rust, tokio-native,
MIT/Apache-2.0), so no FFI.

### Engine

`src/vnc/` is display-free:

- `VncSession::connect` dials, does the RFB handshake and auth, bounded at
  15 s (the crate has no handshake timeout of its own), then `spawn()`
  starts the engine task.
- The engine owns an `Arc<Mutex<VncPixels>>` framebuffer — a plain BGRA32
  `Vec<u32>`, since cairo surfaces are not `Send` — with every pixel forced
  opaque. It drains the input channel, maps the client's events into
  framebuffer writes and UI events (`Dirtied`, `Resolved`, `Bell`,
  `Clipboard`, `Ended`), and closes the client on cancel or error.
- The default port is 5900.

### Drawing and input

- The draw handler snapshots `VncPixels` into a cached cairo `RGB24`
  surface, row by row (strides may exceed `width × 4`), and paints it.
  With `vnc_options.scaling` (the default) the picture fits the tab,
  aspect-preserving and centered, padding the source so edge pixels do not
  darken; without it, it is centered 1:1.
- Keys: GDK keyvals are X11 keysyms, so they are forwarded unchanged;
  modifiers are key events of their own. Pointer: motion, buttons and the
  scroll wheel build pointer events, mapped back through the placement.
- `vnc_options.view_only` suppresses all input.

### Credentials

The VNC password resolves through the credential chain as `vnc_password`
(doc 05) before the engine starts. It stays `Zeroizing` up to the crate
boundary; the crate copies it into a `String` it gives no way to zeroize.

### vnc-rs 0.6.0 behavior

Verified from the crate's source (the full list is in `src/vnc/client.rs`;
re-verify before a version bump):

- Updates are **pull-only**: the crate requests one frame after the
  handshake and never again, so the engine sends an incremental refresh
  every 30 ms — without it the screen freezes after the first frame.
- `poll_event` never blocks (a `try_recv` on a 2-slot channel); the engine
  polls rather than holding the client's mutex while waiting.
- Pseudo-encodings must be requested or their rects are fatal decode
  errors; `DesktopSizePseudo` and `CursorPseudo` are in the default set.
- A server hang-up is silent (the event channel closes); the engine
  reports "server closed".
- Auth failure is reported inconsistently (RFB 3.8: `General`; 3.3/3.7:
  `WrongPassword`); both map to an auth failure.
- `SetCursor` carries cursor-shape pixels; the engine ignores them rather
  than painting the cursor into the desktop.

## Limitations

- **RDP:** X11/XWayland only; single monitor; no RDP gateway; the remote
  resolution does not follow the tab size; FreeRDP 3 required, with stdin
  credentials reliable from 3.20. "Connected" is a 15 s heuristic. The
  Wayland branch has only been code-reviewed, not run.
- **VNC:** plain VNC only (no VeNCrypt/TLS) — tunnel it over SSH.
  `vnc_options.quality` and `vnc_color_depth` are stored but not applied
  (the crate has no quality knob; the client always requests BGRA32).
  Clipboard is receive-only (server → Wayterm). The remote cursor shape is
  not drawn. Bell only rings GTK's error bell. Non-ASCII passwords do not
  authenticate (the crate uses the first 8 UTF-8 bytes as the DES key).
- Neither protocol has a save-to-keyring control (doc 05 §Limitations), a
  macro tap, or automatic reconnect. Their tabs are not registered as
  live (doc 03 §Lifecycle), so closing one never asks first and the
  `reconnect` / `disconnect` actions do nothing on them.
