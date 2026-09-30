# Terminal Widget Comparison

Full landscape of embeddable terminal libraries/widgets, evaluated for
a Rust + GTK4 application. Categorized by integration model.

## Tier 1: Full Widgets (drop-in, own rendering)

### VTE / libvte (GNOME) — PRIMARY CHOICE
- **Language:** C (GTK4 bindings via `vte4` Rust crate)
- **Model:** GTK widget — `vte4::Terminal`, drop into any GTK4 container
- **Features:** Full VT emulation, Unicode, scrollback, search, color
  schemes, URL detection, drag-drop, mouse tracking, hyperlinks, text
  reflow, alternate screen. The most widely tested terminal emulation
  code on Linux.
- **Maturity:** Gold standard — more terminal apps use VTE than any other
  widget. Maintained by GNOME project. Extensive real-world exposure.
- **Dependencies:** GTK4 / GNOME platform (already our framework)
- **License:** LGPL
- **Used by:** GNOME Terminal, Tilix, Terminator, XFCE Terminal, Guake,
  Ptyxis, and dozens more
- **Fit:** ★★★★★ — native GTK4 widget, zero framework friction, the
  obvious choice for a Rust+GTK4 app
- **Rust bindings:** `vte4` crate (gtk-rs project), stable and well-maintained
- **Limitations:** No Kitty graphics protocol. SIXEL **is** supported
  since VTE 0.62, but opt-in (`vte_terminal_set_enable_sixel`) —
  the terminal reports SIXEL capability to applications only when
  enabled, and distros ship varying VTE versions, so expose it as a
  per-session setting with a runtime version check
  (`vte_get_startup_id`/feature check), not a hardcoded yes/no. Kitty
  graphics would need a fork or the ghostty engine backend.

### QTermWidget (LXQt)
- **Language:** C++ / Qt
- **Model:** Qt widget — not usable from GTK4 without cross-toolkit
  embedding (fragile, not recommended)
- **Fit:** ✗ — Qt widget, wrong toolkit for this stack. Listed for
  completeness.

### Konsole KPart (KDE)
- **Language:** C++ / Qt / KDE Frameworks
- **Model:** KDE KPart component — requires Qt/KDE application shell
- **Fit:** ✗ — requires Qt/KDE, wrong toolkit for this stack. Listed
  for completeness.

### xterm.js
- **Language:** JavaScript/TypeScript
- **Model:** Browser canvas/DOM renderer
- **Fit:** ✗ — browser-only, irrelevant for a native GTK4 app

## Tier 2: Engine Libraries (state only, you build rendering)

### libghostty-vt (Ghostty)
- **Language:** C API (core written in Zig)
- **Model:** Zero-dependency C library. Provides VT parsing, terminal
  state (cursor, styles, scrollback, reflow), and render state API.
  No rendering — you build it. WASM-compatible.
- **Features:** SIMD-optimized parsing, Kitty Graphics Protocol, Tmux
  Control Mode, excellent Unicode, fuzzed and Valgrind-tested.
- **Maturity:** Early — announced September 2025. C API "not ready for
  general usage" yet. Ghostling demo proves the concept works.
- **Dependencies:** Zero (not even libc)
- **License:** MIT
- **Reference:** Ghostling (github.com/ghostty-org/ghostling) — minimal
  terminal using libghostty-vt + Raylib. 1.1k stars, single main.c file.
- **Fit:** ★★★★ (future) — the technically superior engine. When C API
  stabilizes, wrap it as a Rust backend that renders to a Cairo surface
  in a custom GTK4 widget. The `TerminalBackend` trait makes this a new
  implementation, not an architecture change.
- **Rust integration:** C API called via `libloading` or direct FFI
  bindings. Zero dependencies means it could even be vendored.
- **Key insight:** libghostty has no opinion about the renderer. You
  provide a rendering surface (Cairo, OpenGL, anything) and it gives
  you terminal state to draw. In GTK4, you'd create a custom widget
  subclass that reads ghostty render state in its snapshot/draw handler
  and renders via Cairo or gtk4's OpenGL integration.

### alacritty_terminal (Rust crate)
- **Language:** Rust
- **Model:** Rust crate — VT engine, PTY handling, grid model without
  Alacritty's OpenGL rendering layer.
- **Features:** Fast, well-tested VT emulation, scrollback, Unicode,
  ANSI color. Used by Zed editor (forked).
- **Maturity:** Beta per Alacritty's readme, but widely used as dependency.
  Crate API unstable between versions — it's Alacritty's internal code
  published for reuse, not a carefully designed embedding API.
- **Dependencies:** Rust toolchain (already have it)
- **License:** Apache 2.0
- **Used by:** Zed editor (forked), Alacritty itself
- **Fit:** ★★★ — pure Rust, no FFI needed. But it's an engine, not a
  widget — you'd build a custom GTK4 widget that renders alacritty's
  grid model via Cairo. The crate API isn't designed as a stable external
  library, so you'd be depending on Alacritty's internal API stability.
  Could work as an intermediate backend before libghostty-vt is ready,
  but the API instability is a real concern for an open source project
  that wants to be a stable dependency target.
- **Rust integration:** Native — it's a Rust crate. `alacritty_terminal`
  in Cargo.toml. But the rendering layer is entirely on you.

## Tier 3: Other (wrong ecosystem)

### Jediterm (JetBrains)
- **Language:** Java/Kotlin
- **Fit:** ✗ — Java, wrong ecosystem. Known conformance bugs.

## Comparison Matrix

| Library | Widget? | Language | Rendering | Dependencies | Maturity | VT Conformance | License | Rust fit |
|---|---|---|---|---|---|---|---|---|
| **VTE** | ✓ Full | C/GTK4 | Built-in | GTK4 (have it) | ★★★★★ | Excellent | LGPL | ★★★★★ native |
| libghostty-vt | ✗ Engine | C API (Zig) | You build | Zero | ★★★ early | Best-in-class | MIT | ★★★★ via FFI |
| alacritty_terminal | ✗ Engine | Rust | You build | Rust | ★★★ | Very good | Apache 2.0 | ★★★ native crate |
| QTermWidget | ✓ Full | C++/Qt | Built-in | Qt | ★★★★ | Good | GPL | ✗ wrong toolkit |
| Konsole KPart | ✓ Full | C++/Qt/KDE | Built-in | KDE | ★★★★★ | Excellent | GPL | ✗ wrong toolkit |
| xterm.js | ✓ Full | JS | Canvas/DOM | Browser | ★★★★ | Good | MIT | ✗ browser only |
| Jediterm | ✓ Full | Java | Built-in | JVM | ★★★ | Fair | LGPL | ✗ wrong ecosystem |

## Architecture Decision

**Initial backend:** VTE (built-in, always available)
- Native GTK4 widget, zero friction
- Gold-standard terminal emulation
- `vte4` Rust crate provides clean bindings
- Gets to working app fastest

**Future backend:** libghostty-vt + custom GTK4 render widget
- When C API stabilizes, add as a Cargo feature
- Custom widget renders ghostty state via Cairo or OpenGL
- The `TerminalBackend` trait makes this a new impl, not a rewrite
- Zero external dependencies for this backend (libghostty-vt is zero-dep)

**Possible intermediate:** alacritty_terminal crate
- If we need an engine backend before libghostty-vt is ready
- Pure Rust, no FFI — attractive
- Risk: crate API instability, it's not designed as a stable library
- Would need careful version pinning

## Key Insight: Widget vs Engine

**Widgets** give you a drop-in widget that handles everything (rendering,
input, scrollback, selection, fonts, colors). You put it in a tab and
you're done. But you're locked into their rendering model and visual style.

**Engines** give you terminal state. You build the renderer. This means
GPU-accelerated rendering, full visual control — but you must build
rendering, input handling, selection, scrollback UI, font fallback.
Thousands of lines of non-trivial code.

For this project, the differentiated value is in session management, RDP/
VNC embedding, SFTP sidebar, and hotkeys — not terminal rendering. Start
with VTE (widget), keep the engine option open via the trait abstraction.
When libghostty-vt matures, add it as a trait implementation with a custom
Cairo-based render widget.