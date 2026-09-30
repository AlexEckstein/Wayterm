# Architecture Overview

## High-Level Diagram

```
┌─ Application (Rust + GTK 4) ──────────────────────────────────┐
│                                                                │
│  ┌────────────┐  ┌───────────────────────────────────────────┐│
│  │  Session   │  │  Tab Manager (Gtk.Notebook)                ││
│  │  Tree      │  │                                           ││
│  │  (Gtk.TreeView)│  ┌─ TerminalTab ───────────────────────┐ ││
│  │            │  │  │                                      │ ││
│  │  ├─ prod-db│  │  │  Transport        TerminalBackend     │ ││
│  │  │  SSH    │  │  │  ┌────────┐      ┌───────────────┐   │ ││
│  │  │  tmux:☑ │  │  │  │russh   │ ←→  │ VTE           │   │ ││
│  │  ├─ prod-rd│  │  │  │(async) │bytes│ (trait impl)  │   │ ││
│  │  │  RDP    │  │  │  │or PTY  │ ←→  │ or GhosttyVT  │   │ ││
│  │  ├─ lab-vnc│  │  │  └────────┘      └───────┬───────┘   │ ││
│  │  │  VNC    │  │  │                          │           │ ││
│  │  └─ dev-web│  │  │  ┌───────────────────────▼─────────┐ │ ││
│  │   SSH+SFTP │  │  │  │ gtk::Widget (embedded in tab)  │ │ ││
│  │            │  │  │  │ - Terminal (VTE)              │ │ ││
│  │  ├─────────┤  │  │  │ - OR: RDP (xfreerdp reparented) │ │ ││
│  │  │SFTP Side│  │  │  │ - OR: VNC (Cairo framebuffer)   │ │ ││
│  │  │(russh-  │  │  │  └────────────────────────────────┘ │ ││
│  │  │ sftp)   │  │  └──────────────────────────────────────┘ ││
│  │  └─────────┤  └───────────────────────────────────────────┘│
│  │            │                                                │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Hotkey Engine (Gtk.ShortcutController + Gtk.Shortcut)   │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                                │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Session Store (SQLite via rusqlite + libsecret)         │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                                │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Backend Registry (compile-time features + runtime trait)│ │
│  └──────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────┘
```

## Key Principles

### 1. Transport ↔ Terminal separation
The SSH/PTY/Serial transport layer is fully separated from the terminal
backend. The terminal widget does not own the connection. Data flows
through a clean trait boundary:

```
Transport::data_received  →  TerminalBackend::write(data)
TerminalBackend::data_out  →  Transport::write(data)
TerminalBackend::size_change →  Transport::resize(rows, cols)
```

In Rust, the transport runs as an async task (tokio) and communicates
with the GTK main loop via channels (e.g., `glib::MainContext` channels
or `tokio::sync::mpsc` bridged to GLib). This is a key advantage: the
async runtime handles concurrent connections cleanly, and the compiler
ensures no data races across the channel boundary.

### 2. Pluggable terminal backends (Rust trait + feature flags)
Terminal backends implement a common `TerminalBackend` trait. The
primary backend (VTE) is built-in. Future backends (libghostty-vt) are
added as Cargo features or C ABI plugins. The app uses trait objects
(`Box<dyn TerminalBackend>`) for runtime dispatch.

See: [Terminal Backend Abstraction](02-terminal-backend-abstraction.md)
and [Plugin System](05-plugin-system.md).

### 3. Feature discovery / graceful degradation
Different backends support different features. The app queries
`TerminalFeatures` from each backend and shows/hides UI accordingly.
Users never see buttons that do nothing.

### 4. Protocol tabs are first-class
SSH, RDP, and VNC are all tabs in the same `GtkNotebook`. RDP uses X11
window reparenting via GDK's X11 API. VNC renders to a Cairo surface
in a custom widget.

See: [RDP-VNC Embedding](08-rdp-vnc-embedding.md).

### 5. Session persistence via tmux (optional)
Each SSH session can optionally use tmux as a backend for disconnect
resilience. On reconnect, the app re-attaches to the existing tmux
session, restoring full state.

### 6. Async-first I/O
All network I/O (SSH, SFTP, VNC polling) runs on tokio. The GTK UI
runs on the GLib main loop. Data crosses between them via channel
bridges — no manual locking, no data races (compiler-enforced).

## Component Summary

| Component | Technology | Document |
|---|---|---|
| Language / framework | Rust, GTK 4 | this |
| Terminal backend | VTE (built-in), trait-pluggable | 02 |
| Terminal widgets comparison | VTE, libghostty-vt, alacritty_terminal, others | 03 |
| Transport (SSH/PTY/Serial) | russh (tokio), forkpty, serial | 04 |
| SSH auth & host keys | known_hosts, keyboard-interactive, keyring | 11 |
| Backend system | Rust trait + Cargo features, optional C ABI | 05 |
| Session store | rusqlite (SQLite) + libsecret | 06 |
| Hotkey engine | GtkShortcutController + app actions | 07 |
| RDP/VNC embedding | xfreerdp + GDK X11 reparenting, libvncclient + Cairo | 08 |
| SFTP sidebar | russh-sftp (same SSH channel) | 09 |
| Build order | Phased MVP → full feature set | 10 |

## Crate Dependencies (Core)

```toml
[dependencies]
gtk4 = "0.11"              # GTK4 Rust bindings
vte4 = "0.10"              # VTE terminal widget (GTK4)
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"         # CancellationToken (transport lifecycle)
russh = "0.63"             # Pure Rust SSH client
russh-sftp = "2.4"         # SFTP subsystem for russh
russh-config = "0.58"      # ~/.ssh/config parsing (Phase 9 import)
rusqlite = { version = "0.40", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
libsecret = "0.9"          # OS keyring (libsecret bindings)
x11rb = "0.14"             # X11 protocol (for RDP reparenting)
pango = "0.22"             # Font handling
cairo = "0.22"             # VNC framebuffer rendering (crate name is
                           # "cairo", the -rs suffix is only in git)
libloading = "0.9"         # Dynamic library loading (plugins)
```

Versions checked against crates.io 2026-08-28. russh in particular
moves fast — re-verify at Phase 1 kickoff (doc 11 keeps "version
anchors" for the security-sensitive API surface).

## Known Constraints

- **RDP on Wayland-native**: X11 window reparenting doesn't work on
  Wayland. XWayland is the fallback. True Wayland RDP embedding would
  require a protocol-level RDP client rendered to a GTK surface.
  Flagged as v1 limitation.
- **VNC encryption**: libvncclient supports VeNCrypt/TLS but setup is
  complex. MVP: plain VNC + SSH tunnel. Future: direct encrypted VNC.
- **GTK4 removed GtkSocket/GtkPlug**: X11 embedding is more manual than
  GTK3. We use `gdk_x11_surface_get_xid()` + raw X11 reparenting via
  x11rb. See 08-rdp-vnc-embedding.md for details.
- **VTE is the only production-ready GTK4 terminal widget**: No second
  built-in option like Qt's QTermWidget. The trait abstraction still
  allows future libghostty-vt integration.