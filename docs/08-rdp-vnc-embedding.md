# RDP-VNC Embedding

## Goal

RDP and VNC sessions appear as tabs in the same `GtkNotebook` as SSH
terminal tabs. True in-window embedding, not launching external windows.

> **Revision note (2026-08):** three corrections. (1) The previous
> revision claimed "Remmina uses the same approach [xfreerdp subprocess
> + reparenting]" — that is false: Remmina links **libfreerdp as a
> library** and renders into its own GTK widget (its `rdp_plugin.c` is
> full of direct FreeRDP internals). There is no mainstream GTK4
> precedent for foreign-X11-window embedding — the risk here is *higher*
> than the old text implied, and the prototype-early advice matters more.
> (2) RDP password handling now uses FreeRDP 3's `/from-stdin` (verified
> in current manpages) — details below. (3) The VNC widget code had
> GTK-thread-safety bugs (`queue_draw()` and clipboard calls from the
> poll thread) and poked fields through an opaque struct — fixed.

## RDP Embedding

### Approach: X11 Window Reparenting via GDK X11

GTK4 removed `GtkSocket`/`GtkPlug` (the old X11 embedding widgets from
GTK3). The replacement is lower-level: use `gdk_x11` API to obtain the
X11 window ID of a container widget, pass it to `xfreerdp` via
`/parent-window-id`, then manage the foreign X11 window manually.

### The GTK4 X11 Embedding Challenge

Unlike Qt's clean `QWindow::fromWinId()` → `QWidget::createWindowContainer()`
(3 lines, documented), GTK4 requires manual X11 work:

1. Get the container widget's X11 window ID via
   `gdk_x11_surface_get_xid()`
2. Pass it to `xfreerdp /parent-window-id:<id>`
3. `xfreerdp` creates its X11 window as a child of the container
4. The foreign window is managed by monitoring X11 events (via `x11rb`
   or raw Xlib)
5. Resize the child window when the container resizes

This is more manual than Qt's approach, and there is no mainstream GTK4
application doing it (Remmina — the obvious candidate — embeds
libfreerdp directly instead of subprocess+reparent). It works in
principle (the X11 mechanisms are standard), but treat it as the
highest-risk item in the project and prototype it standalone before
building any UI around it.

### Flow

```
1. User opens an RDP session from the session tree
2. App creates a container GtkBox (or GtkFixed) for the tab
3. App gets the container's X11 window ID:
     let surface = widget.surface();
     let x11_surface = surface.downcast_ref::<gdk4_x11::X11Surface>().unwrap();
     let xid = x11_surface.xid();
4. App spawns xfreerdp:
     xfreerdp /v:server /u:user /from-stdin /parent-window-id:<xid>
              /w:1024 /h:768 /bpp:24
     (credentials written to stdin — see Password Security)
5. xfreerdp creates its X11 window as a child of the container
6. App monitors for the child window via X11 QueryTree
7. App resizes the child window when the container resizes
```

### Implementation

```rust
use std::process::{Child, Command, Stdio};
use std::io::Write;
use gtk4::prelude::*;
use gtk4::Box as GtkBox;

pub struct RdpTab {
    container: GtkBox,
    process: Option<Child>,
    child_xid: Option<u32>,
}

impl RdpTab {
    pub fn new(config: &SessionConfig, creds: Option<&RdpCredentials>) -> Self {
        let container = GtkBox::new(gtk4::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.realize();

        let container_clone = container.clone();
        glib::idle_add_local(clone!(@weak config => move || {
            if let Some(surface) = container_clone.surface() {
                if let Ok(x11_surface) = surface.downcast::<gdk4_x11::X11Surface>() {
                    let xid = x11_surface.xid() as u64;

                    // NEVER put the password on argv — /proc/<pid>/cmdline
                    // is world-readable. /from-stdin instead (below).
                    let mut child = Command::new("xfreerdp")
                        .args([
                            format!("/v:{}:{}", config.host, config.port),
                            // username MAY go on argv; password never.
                            // /from-stdin sends both:
                            "/from-stdin".to_string(),
                            format!("/parent-window-id:{}", xid),
                            format!("/w:{}", config.rdp_width.unwrap_or(1024)),
                            format!("/h:{}", config.rdp_height.unwrap_or(768)),
                            format!("/bpp:{}", config.rdp_color_depth.unwrap_or(32)),
                            // turn off xfreerdp's own cert dialogs — we
                            // gate them ourselves against the session store:
                            "/cert:ignore-tofu".to_string(),
                        ])
                        .stdin(Stdio::piped())
                        .spawn()
                        .expect("Failed to start xfreerdp");

                    if let Some(stdin) = child.stdin.as_mut() {
                        // one credential per line, per /from-stdin docs:
                        // username then password
                        let _ = writeln!(stdin, "{}", creds.username);
                        let _ = writeln!(stdin, "{}", creds.password);
                        drop(stdin); // close so xfreerdp can read EOF
                    }
                    // ... store child, poll for child window (below)
                }
            }
            glib::ControlFlow::Break
        }));
        // ...
    }
}
```

(Structure as before: `glib::timeout_add_local` polling via x11rb
`query_tree` until the child window appears, then `configure_window`
resizes. Sketch unchanged apart from the credential path.)

### Using x11rb for Window Management

```rust
use x11rb::protocol::xproto::*;
use x11rb::connection::Connection;

fn find_child_window(conn: &impl Connection, parent: u32) -> Option<u32> {
    let tree = conn.query_tree(parent).ok()?;
    let reply = tree.reply().ok()?;
    reply.children.first().copied()
}

fn resize_child_window(conn: &impl Connection, window: u32, w: u16, h: u16) {
    let _ = conn.configure_window(
        window,
        &ConfigureWindowAux::new().width(w).height(h),
    );
    let _ = conn.flush();
}
```

### Subprocess vs. Library

**Subprocess (xfreerdp CLI):**
- Simpler — spawn process, reparent window
- Process isolation — xfreerdp crash doesn't crash your app
- **No mainstream precedent** — Remmina embeds libfreerdp directly
  (library path), it does not subprocess+reparent. We'd be the first
  GTK4 app doing this at scale.
- Password handling needs stdin (`/from-stdin`) — argv leaks via /proc
- Less runtime control over RDP options
- This is what we start with (MVP), prototype-first

**Library (libfreerdp3 C API):**
- Direct API — no subprocess, no window reparenting (render to a GTK
  surface like Remmina does)
- Password set in-process, never visible to other local users
- More complex — FreeRDP's C API is large and under-documented
- Crash in FreeRDP can crash your app
- This is Remmina's actual approach, and the likely migration target
- ABI churn is the tax: every distro FreeRDP soname bump breaks Remmina
  builds — we'd wear that too

**Recommendation:** subprocess for MVP, with the standalone
reparenting prototype de-risking Phase 7 before any UI work. Migrate to
libfreerdp if (when) reparenting proves fragile.

### Password Security

Never on the command line. FreeRDP 3 supports stdin credentials:

```
xfreerdp /v:host /from-stdin[:force] ...
```

- Reads username and password from stdin (one per line; force re-reads
  even if `/u:` was given). Verified present in current FreeRDP 3.x
  manpages — **[Phase 2 spike]** confirm the exact line order and
  behavior on each target distro, since distros ship different FreeRDP
  versions.
- If a target distro ships FreeRDP 2.x (no `/from-stdin`): fall back to
  launching xfreerdp *without* credentials (its own GUI prompt appears;
  session can't be embedded-credential-free there), and surface a
  settings note recommending FreeRDP 3. Never write credentials to
  argv, environment of a process you didn't start, or disk.
- Long term, the libfreerdp C API sets the password in-process and
  eliminates the issue entirely.

### Wayland Handling

X11 reparenting only works on X11 or XWayland. On Wayland-native:

```rust
fn is_wayland_native() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        && std::env::var("GDK_BACKEND").map(|b| b.contains("x11")).unwrap_or(false) == false
}
```

(Both checks matter: `WAYLAND_DISPLAY` set plus `DISPLAY` set means
XWayland is available and GTK may still run on it; check which backend
GDK actually picked. On failure → warn + launch xfreerdp un-embedded.)

```rust
// In RdpTab::new():
if is_wayland_native() {
    // Warn user: RDP embedding requires X11/XWayland.
    // Options: relaunch app with GDK_BACKEND=x11, or open un-embedded.
    // (Do NOT silently spawn a second window — ask.)
}
```

## VNC Embedding

### Approach: Native GTK4 Widget + libvncclient

Unlike RDP (subprocess + reparenting), VNC renders natively in a
GTK4 widget: the framebuffer is drawn via a `gtk4::DrawingArea`.
GTK4's `DrawingArea::set_draw_func` hands you a `&cairo::Context` in
its snapshot/draw callback — that part of the original sketch was
correct.

Two implementation paths:

#### Path A: libvncclient via FFI

The original sketch had three bugs, all fixed here:

1. **Cross-thread `queue_draw()`** — `drawing_area.queue_draw()` was
   called from the poll thread. GTK4 is not thread-safe; widget calls
   from non-main threads are undefined behavior. Must go through
   `MainContext::invoke`.
2. **Opaque struct field access** — `(*client).programName = …` on a
   `#[repr(C)] pub struct RfbClient { _private: [u8; 0] }` can't even
   compile (the type is opaque). libvncclient config goes through its
   real API surface (`rfbClientInit`-adjacent setters / the
   `MallocFrameBuffer` callback etc.).
3. **Per-frame `fb.data.clone()`** — cloning the whole framebuffer to
   create a Cairo surface every draw is quadratic-ish in practice.
   Keep a `cairo::ImageSurface` as the *primary* framebuffer store
   instead: the GotFrameBufferUpdate callback writes rects into it
   under a lock; the draw callback blits it.

```rust
pub struct VncTab {
    drawing_area: gtk4::DrawingArea,
    client: *mut ffi::RfbClient,          // owned by poll thread
    surface: Arc<Mutex<cairo::ImageSurface>>,  // shared framebuffer
    dirty: Arc<Mutex<Option<Rect>>>,
    poll_thread: Option<std::thread::JoinHandle<()>>,
}

impl VncTab {
    pub fn new(config: &SessionConfig) -> Self {
        let drawing_area = gtk4::DrawingArea::new();
        drawing_area.set_hexpand(true);
        drawing_area.set_vexpand(true);

        let surface = Arc::new(Mutex::new(
            cairo::ImageSurface::create(cairo::Format::ARgb32, 0, 0).unwrap()
        ));
        let dirty: Arc<Mutex<Option<Rect>>> = Arc::new(Mutex::new(None));

        // FIX 1: all widget interaction from the poll thread goes through
        // the main context — queue_draw included:
        let main_ctx = glib::MainContext::default();
        let dirty_clone = dirty.clone();
        let poll_thread = std::thread::spawn(move || {
            unsafe {
                let client = ffi::rfb_get_client(8, 3, 4);
                if client.is_null() { return; }
                // FIX 2: configure host/port/auth through libvncclient's
                // real API surface (SetClientAuth / serverHost+serverPort
                // via its setters, GotFrameBufferUpdate callback) — not
                // by poking the opaque struct.
                if ffi::rfb_init_client(client, &mut 0, ptr::null_mut()) == 0 {
                    return;
                }
                loop {
                    let result = ffi::WaitForMessage(client, 100);
                    if result < 0 { break; }
                    if result > 0 && ffi::HandleRFBServerMessage(client) == 0 {
                        break;
                    }
                    // FIX 1 (cont'd): repaint request must hop to GTK:
                    if dirty_clone.lock().unwrap().is_some() {
                        let ctx = main_ctx.clone();
                        ctx.invoke(move || {
                            drawing_area.queue_draw();  // main thread ✓
                        });
                        *dirty_clone.lock().unwrap() = None;
                    }
                }
            }
        });

        // FIX 3: draw callback blits the shared ImageSurface — no
        // per-frame framebuffer clone:
        let surface_for_draw = surface.clone();
        drawing_area.set_draw_func(move |_da, cr, _w, _h| {
            let surf = surface_for_draw.lock().unwrap();
            if surf.width() == 0 { return; }
            cr.set_source_surface(&surf, 0.0, 0.0).unwrap();
            cr.paint().unwrap();
        });
        // (Input: EventControllerMotion/GestureClick as before; mouse
        // events ALSO must not call libvncclient from the GTK thread
        // without care — prefer a channel to the poll thread, or use
        // libvncclient's documented thread-safe send wrappers.)
        VncTab { drawing_area, client: ptr::null_mut(), surface, dirty, poll_thread: Some(poll_thread) }
    }
}
```

#### Path B: Pure Rust VNC client (vnc-rs or custom)

A pure Rust VNC client avoids FFI entirely. The `vnc-rs` crate exists
but is less mature than libvncclient. If it matures, it becomes the
preferred path — no C dependency, tokio-integrated, memory-safe, and
the poll thread becomes a plain tokio task (data flows through the same
glib channel as SSH output — doc 04's pattern, reused). Track its
development; revisit before Phase 6.

### VNC Key Mapping

```rust
fn gtk_key_to_vnc_keysym(keyval: gdk4::Key, modifiers: gdk4::ModifierType) -> u32 {
    // VNC uses X11 keysyms (same as X11/keysymdef.h)
    // GDK keyvals ARE X11 keysyms — direct mapping!
    let keysym = keyval as u32;

    // Handle shifted characters
    if modifiers.contains(gdk4::ModifierType::SHIFT_MASK) {
        if ('a' as u32..='z' as u32).contains(&keysym) {
            return keysym - 32;  // uppercase
        }
    }
    keysym
}
```

Advantage of GTK4: GDK keyvals are already X11 keysyms. No translation
table needed — direct cast. This remains correct and is cleaner than the
Qt approach.

### VNC Mouse Handling

Same controllers as before (EventControllerMotion + GestureClick on the
drawing area), with the threading caveat above: prefer sending pointer
events through a channel to the poll thread rather than calling FFI from
GTK callbacks directly (libvncclient's send functions are documented as
thread-safe in recent versions — verify, don't assume).

## Tab Type Unification

All three session types (SSH, RDP, VNC) are tabs in the same GtkNotebook.
The tab manager doesn't branch on type except at creation:

```rust
pub fn create_tab(config: &SessionConfig, backend: Arc<dyn TerminalBackend>) -> gtk4::Widget {
    match config.session_type {
        SessionType::SSH | SessionType::LocalShell | SessionType::Serial => {
            let tab = TerminalTab::new(config, backend);
            tab.widget()
        }
        SessionType::RDP => { let tab = RdpTab::new(config); tab.widget() }
        SessionType::VNC => { let tab = VncTab::new(config); tab.widget() }
    }
}
```

Each tab type is a self-contained `gtk::Widget`. The `GtkNotebook` treats
them all as generic widgets. Tab labels show the session name with
optional type-specific icons (terminal, monitor, screen).

The hotkey consequence (doc 07): RDP/VNC tabs have no Tier-2 shortcut
controllers attached, so with focus in them only Tier-1 app accelerators
intercept keys — exactly the intended minimal set. No extra machinery
needed here.

## Known Limitations (v1)

- **RDP on Wayland-native**: X11 reparenting doesn't work. XWayland
  fallback required. True Wayland RDP embedding needs a protocol-level
  client rendered to a GTK surface (i.e., the libfreerdp path).
- **RDP via subprocess**: credentials must go via `/from-stdin`
  (FreeRDP 3.x); distros on FreeRDP 2.x get the no-embedded-credentials
  fallback. Long-term fix is the libfreerdp C API.
- **No precedent for the reparent path**: Remmina embeds libfreerdp; we
  would be the notable GTK4 app doing subprocess+reparent. Prototype
  standalone before committing to it (Phase 7 gate).
- **VNC encryption**: libvncclient supports VeNCrypt and TLS, but setup
  is more complex. MVP: plain VNC + SSH tunnel. Future: direct
  encrypted VNC.
- **Multi-monitor RDP**: FreeRDP supports multi-monitor, but embedding
  each display as separate tabs is complex. v1: single display.
- **GTK4 X11 embedding is manual**: no GtkSocket. Raw X11 via x11rb. If
  the Phase 7 prototype proves too fragile, fall back to separate-window
  xfreerdp immediately (still functional) while planning the libfreerdp
  migration — don't sink weekends into polishing reparenting.