# Terminal Backend Abstraction

## Problem

The app needs to support multiple terminal backends (VTE, libghostty-vt,
potentially others) with different capabilities and integration models.
The rest of the app (tabs, sessions, hotkeys, SFTP) must be unaware of
which backend is active.

## Two Backend Categories

### Widget backends (drop-in GtkWidget)
- VTE (the primary choice)
- Own their own rendering surface (GTK/Cairo-based)
- Own their own scrollback, selection, fonts, color schemes
- You embed their `gtk::Widget` in a tab — done

### Engine backends (state only, you render)
- libghostty-vt, alacritty_terminal
- Provide terminal state (cursor, character grid, colors, scrollback)
- No rendering surface — you build it
- You create a custom `gtk::Widget` subclass that reads engine state in
  its render/draw handler and draws via Cairo or OpenGL
- Full control over rendering but significant implementation effort

Both categories implement the same Rust trait. The key: `widget()` always
returns a `gtk::Widget` — for widget backends it's their surface, for engine
backends it's your custom render widget.

## Trait Definition

```rust
use gtk4::prelude::*;
use gtk4::Widget;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Capabilities a backend supports — used for feature gating in the UI.
#[derive(Clone, Debug, Default)]
pub struct TerminalFeatures {
    pub search: bool,
    pub color_schemes: bool,
    pub scrollback_search: bool,
    pub url_detection: bool,
    pub sixel_graphics: bool,
    pub kitty_graphics: bool,
    pub custom_fonts: bool,
    pub reflow: bool,             // text reflow on resize
    pub alternate_screen: bool,   // alternate screen buffer support
    pub scrollback_config: bool,  // backend exposes scrollback limit config
}

/// Data flowing out of the terminal (user keystrokes → transport).
/// Sent via channel, not return value, to keep the backend decoupled
/// from the transport.
pub enum TerminalEvent {
    DataOut(Vec<u8>),
    SizeChange { rows: u32, cols: u32 },
    TitleChanged(String),
    Bell,
    Closed,
}

/// The terminal backend trait. Implementations:
/// - VteBackend (built-in, uses vte4 crate)
/// - GhosttyBackend (future, uses libghostty-vt C API)
/// - FakeBackend (test-only: records writes, no widget) — see Testing
///
/// All methods are &self. The backend owns its widget.
/// Data flows in via write(). Data flows out via the event channel
/// created at construction time.
///
/// **Note (2026-10 revision):** there is deliberately NO `Send` supertrait.
/// Widget backends hold GTK widgets, which are `!Send` by design — the
/// backend lives on the GTK main thread for its entire lifetime (doc 04
/// threading model); the transport side never touches the backend, they
/// communicate via channels. FakeBackend remains trivially testable. The
/// original sketch had `TerminalBackend: Send`, which widget backends
/// cannot satisfy; removed at Phase 2 implementation (commit 97e55ce).
pub trait TerminalBackend {
    /// The GTK widget to embed in a tab.
    /// Always returns a valid gtk::Widget — the tab embeds this.
    /// Widget backends: their own rendering surface.
    /// Engine backends: custom render widget reading engine state.
    fn widget(&self) -> Widget;

    /// Bytes from transport (SSH/PTY/serial) → terminal.
    /// Called from the GTK main thread only (the glib channel receiver
    /// in doc 04 invokes this). Never called from tokio directly.
    fn write(&self, data: &[u8]);

    /// Terminal size in rows/cols.
    fn set_size(&self, rows: u32, cols: u32);

    /// Feature discovery — called once at tab creation.
    fn features(&self) -> TerminalFeatures;

    // ── Appearance ──────────────────────────────────────
    /// No-op if backend doesn't support it.
    fn set_color_scheme(&self, name: &str);
    fn set_font(&self, font_desc: &pango::FontDescription);
    fn set_opacity(&self, opacity: f64);
    fn set_scrollback_limit(&self, lines: u32);

    // ── Interaction ─────────────────────────────────────
    fn selected_text(&self) -> Option<String>;
    fn copy_selection(&self);
    fn paste(&self, text: &str);
    fn select_all(&self);
    fn clear(&self);

    // ── Scrollback ──────────────────────────────────────
    /// Returns None if unsupported.
    fn search(&self, text: &str, reverse: bool);
    fn scrollback_text(&self) -> Option<String>;
    fn scroll_to_top(&self);
    fn scroll_to_bottom(&self);

    // ── Lifecycle ───────────────────────────────────────
    fn reset(&self);  // reset terminal state

    // ── Identity ────────────────────────────────────────
    fn name(&self) -> &str;        // "vte", "ghostty-vt"
    fn version(&self) -> &str;
    fn description(&self) -> &str;
}
```

## Event Channel

The backend communicates with the transport via a channel, not direct
references. This keeps them decoupled and is idiomatic Rust:

```rust
pub struct TerminalHandle {
    pub backend: Arc<dyn TerminalBackend>,
    pub events: mpsc::Receiver<TerminalEvent>,
}

// Construction:
pub fn create_terminal(
    backend_name: &str,
    event_tx: mpsc::Sender<TerminalEvent>,
) -> TerminalHandle {
    let (tx, rx) = mpsc::channel(256);

    let backend: Arc<dyn TerminalBackend> = match backend_name {
        "vte" => Arc::new(VteBackend::new(tx)),
        #[cfg(feature = "backend-ghostty")]
        "ghostty" => Arc::new(GhosttyBackend::new(tx)),
        _ => Arc::new(VteBackend::new(tx)),  // default fallback
    };

    TerminalHandle { backend, events: rx }
}
```

## Data Flow with the Transport

> **Revision note (2026-08):** the original `transport_loop` in this doc
> called `transport.read()` — a method the `Transport` trait never
> defined. The real data flow is push-based and lives in doc 04
> §"Wiring"; this section describes the backend's half of it.

The backend never talks to the transport directly. Each tab has exactly
two channels, crossing the GTK/tokio boundary once each way:

```
 keystrokes/resize:  VTE commit/resize signals (GTK main thread)
                         → mpsc::Sender<TerminalEvent> (try_send, bounded)
                         → [tokio] pump task → Transport::write/resize

 terminal output:    [tokio] reader task → TransportEvent::DataReceived
                         → glib::MainContext::channel
                         → [GTK] backend.write(&chunk)
```

The backend's construction wires its widget signals to `event_tx` — the
sender half of the terminal-event channel the pump task consumes. The
backend only sends; receivers are owned elsewhere (doc 04).

### Backpressure: the try_send + idle-flush pattern **[required of all backends]**

VTE's `commit` signal fires on the GTK main thread. The original sketch
used `tx.blocking_send(...)` — **that is a bug**: blocking the GTK thread
on a bounded channel stalls the entire UI and can deadlock against the
pump task. The required pattern for every backend implementation:

```rust
// Wire commit to the event channel WITHOUT blocking the GTK thread:
let tx = event_tx.clone();
let pending: Rc<RefCell<VecDeque<Vec<u8>>>> = Rc::new(RefCell::new(VecDeque::new()));
let idle_scheduled = Rc::new(Cell::new(false));

terminal.connect_commit(move |data: &[u8]| {
    match tx.try_send(TerminalEvent::DataOut(data.to_vec())) {
        Ok(()) => {}  // fast path — the 99% case
        Err(TrySendError::Full(chunk)) => {
            // Slow path: park bytes, flush from an idle callback
            pending.borrow_mut().push_back(chunk);
            if !idle_scheduled.get() {
                idle_scheduled.set(true);
                let pending2 = pending.clone();
                let tx2 = tx.clone();
                glib::idle_add_local(move || {
                    let mut q = pending2.borrow_mut();
                    while let Some(chunk) = q.pop_front() {
                        match tx2.try_send(TerminalEvent::DataOut(chunk)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(chunk)) => {
                                q.push_front(chunk);
                                return glib::ControlFlow::Continue; // retry next idle
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                q.clear();
                                return glib::ControlFlow::Break;
                            }
                        }
                    }
                    idle_scheduled.set(false);
                    glib::ControlFlow::Continue
                });
            }
        }
        Err(TrySendError::Disconnected(_)) => {}  // tab closed; drop
    }
});
```

Why this shape: keystrokes are tiny and rare — the fast path handles
them in one `try_send`. Large pastes can outpace the network; the
pending queue plus idle callback keeps the GTK thread responsive (idle
callbacks run when the main loop is next free) while losing nothing.
The resize signal uses the identical pattern with a `SizeChange`
payload. This pattern is part of the backend contract, not a per-backend
implementation detail.

### FakeBackend (test fixture) **[Phase 2 CI]**

```rust
/// In-memory backend for transport tests — no widget, no GTK.
/// Records every write() call so tests can assert what the remote
/// actually received.
pub struct FakeBackend {
    written: Arc<Mutex<Vec<u8>>>,
    features_override: Option<TerminalFeatures>,
}

impl TerminalBackend for FakeBackend {
    fn widget(&self) -> Widget { unreachable!("no widget in tests") }
    fn write(&self, data: &[u8]) { self.written.lock().unwrap().extend_from_slice(data); }
    // ...trivial impls for the rest; features() returns the override or
    // VTE-parity defaults so feature-gating paths get tested too.
}
```

This enables doc 11 §Testing: transport integration tests against a real
sshd with no GUI. `Arc<dyn TerminalBackend>` (not `Box`) is what makes
the hand-off clean.

## Thread Safety

GTK requires all widget operations on the main thread. The transport
runs on a tokio task. Data crosses via channels:

```rust
// (Construction, GTK side:) glib channel for terminal output
let (gtk_tx, gtk_rx) = glib::MainContext::channel::<Vec<u8>>(glib::Priority::DEFAULT);
gtk_rx.attach(None, glib::clone!(@weak backend => move |chunk: Vec<u8>| {
    backend.write(&chunk);
    glib::ControlFlow::Continue
}));

// (In the reader task, tokio side:) bounded send; if the GTK side
// stalls, SSH flow control (channel window) applies backpressure to
// the server. Memory stays bounded.
```

User types → event channel → pump task reads it. No locking needed —
the channels handle synchronization.

## Backend Implementations

### VteBackend (built-in)

```rust
use vte4::Terminal as VteTerminal;
use vte4::prelude::*;

pub struct VteBackend {
    terminal: VteTerminal,
    event_tx: mpsc::Sender<TerminalEvent>,
}

impl VteBackend {
    pub fn new(event_tx: mpsc::Sender<TerminalEvent>) -> Self {
        let terminal = VteTerminal::new();
        // connect_commit / connect_resize → try_send + idle-flush
        // (exact pattern in Backpressure section above — not repeated here)
        VteBackend { terminal, event_tx }
    }
}

impl TerminalBackend for VteBackend {
    fn widget(&self) -> Widget {
        self.terminal.clone().upcast::<Widget>()
    }

    fn write(&self, data: &[u8]) {
        self.terminal.feed(data);
    }

    fn features(&self) -> TerminalFeatures {
        TerminalFeatures {
            search: true,
            color_schemes: true,
            scrollback_search: true,
            url_detection: true,
            // SIXEL landed in VTE 0.62, opt-in — runtime check, never
            // a hardcode (doc 03 has the details):
            sixel_graphics: vte4::functions::check_version(0, 62, 0),
            kitty_graphics: false,     // no Kitty graphics protocol in VTE
            custom_fonts: true,
            reflow: true,
            alternate_screen: true,
            scrollback_config: true,
        }
    }

    fn set_color_scheme(&self, name: &str) {
        let palette = load_color_scheme(name);
        self.terminal.set_colors(&palette.fg, &palette.bg, &palette.colors);
    }

    fn set_font(&self, font_desc: &pango::FontDescription) {
        self.terminal.set_font(Some(font_desc));
    }

    fn set_opacity(&self, opacity: f64) {
        self.terminal.set_opacity(opacity);
    }

    fn set_scrollback_limit(&self, lines: u32) {
        self.terminal.set_scrollback_lines(lines as i32);
    }

    fn selected_text(&self) -> Option<String> {
        if self.terminal.has_selection() {
            self.terminal.get_text_selected(VteFormat::Text)  // gtk4-rs v0.11+ API
        } else {
            None
        }
    }

    fn copy_selection(&self) {
        if let Some(text) = self.selected_text() {
            self.terminal.clipboard().set_text(&text);
        }
    }

    fn paste(&self, text: &str) {
        self.terminal.paste_text(text);   // vte4 0.10 API name
    }

    // search/set_gregex + search_find — vte4 exposes these; exact names
    // per vte4 0.10 docs. scrollback_text/scroll_to_top/…: trivial impls.
    fn name(&self) -> &str { "vte" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
    fn description(&self) -> &str { "VTE terminal widget (GTK4)" }
}
```

API names above follow vte4 **0.10** / gtk4-rs **0.11** (e.g.
`paste_text`, `has_selection`, `set_scrollback_lines` — the older
`get_has_selection`/`paste` GTK3-style names are gone). Verify exact
names at Phase 2 kickoff; the trait is the contract, the signatures
follow the bindings.

### GhosttyBackend (future, when C API stabilizes)

```rust
pub struct GhosttyBackend {
    render_widget: GhosttyRenderWidget,  // custom gtk::Widget subclass
    ghostty: ghostty_t,                  // libghostty-vt handle
    event_tx: mpsc::Sender<TerminalEvent>,
}
```

Same trait impl shape as the earlier design: the render widget reads
ghostty state in its draw handler and draws via Cairo; key/mouse events
go to ghostty_input(); `features()` reports SIXEL/Kitty graphics true.
The commit/resize wiring uses the same try_send + idle-flush pattern.
No change to the trait was needed for this — which is the abstraction
doing its job.

## Feature Gating in UI

```rust
let features = backend.features();

search_button.set_visible(features.scrollback_search);
color_scheme_button.set_sensitive(features.color_schemes);
graphics_protocol_combo.set_sensitive(features.kitty_graphics || features.sixel_graphics);
scrollback_spin.set_sensitive(features.scrollback_config);
```

UI elements that require unsupported features are hidden or disabled,
not shown as broken buttons.

## Design Decisions

### Why a Rust trait (not C++ virtual class)
Rust traits provide static dispatch (generics) and dynamic dispatch
(`Arc<dyn Trait>`) from the same definition. The trait is not FFI-safe,
so C ABI plugins need a wrapper (see 05-plugin-system.md). For built-in
backends (VTE, and eventually Ghostty when compiled with that feature),
the trait is used directly — zero overhead, compiler-enforced safety.

### Why `Arc<dyn TerminalBackend>` (not `Box<dyn …>` or generic)
The backend is shared across the GTK channel receiver closure, the tab
struct, and (in tests) the test harness — `Arc` is the natural owner.
Runtime backend switching (user preference, per-session override) rules
out generics; the virtual-call cost is noise next to rendering.

### Why events via channel (not callback)
The transport runs on a tokio task; the terminal runs on the GTK main
thread. Direct callbacks would require cross-thread references, which
Rust's ownership model makes painful (and rightly so). The channel
pattern is idiomatic: the backend sends events, the pump task consumes
them. No locks, no data races, compiler-verified.

### Why `TerminalFeatures` is a struct (not per-method bools)
A single `features()` call returns all capabilities. The UI queries once
at tab creation and configures itself. `#[derive(Default)]` makes it easy
to add fields without breaking existing backends.

### Why `write()` takes `&[u8]` (not `&str`)
Terminal data is raw bytes — including null bytes, control sequences, and
arbitrary binary from SFTP or serial connections. Rust's `&[u8]` is the
correct type. No encoding assumptions.