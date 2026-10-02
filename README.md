# Wayterm

An open source, multi-protocol connection manager and terminal for Linux.

Inspired by MobaXterm on Windows, but built for Linux where most of MobaXterm's
bundled tools already exist natively. The focus is on what's genuinely missing
in the open source Linux landscape:

- **Session management** — saved connections, credential vault, organized tree
- **SSH tabs** — tabbed terminal sessions with full VT emulation
- **Custom hotkeys** — configurable, per-action keybinding system
- **RDP/VNC** — remote desktop protocols embedded in-window as tabs
- **SFTP sidebar** — integrated remote file browser alongside the terminal

Project identity: **Wayterm** — app id `com.alex-eckstein.wayterm`, repo
`github.com/AlexEckstein/Wayterm`.

## Tech Stack (Chosen)

| Component | Choice | Rationale |
|---|---|---|
| Language | Rust | Memory safety for network-facing app, compiler-enforced concurrency, async I/O, contributor enthusiasm |
| Framework | GTK 4 | Native Linux toolkit, VTE integration, Flatpak-native, Remmina reference |
| Terminal backend | VTE (libvte), pluggable trait | Gold standard terminal widget — powers GNOME Terminal, Tilix, and dozens more |
| Async runtime | tokio | Unified async for SSH sessions, SFTP transfers, network I/O |
| SSH transport | russh (pure Rust) | No C dependency, tokio-integrated, actively maintained |
| SSH auth/host keys | russh known_hosts + keyboard-interactive | Spec in 11-ssh-auth-security.md — TOFU/strict policies, 2FA support |
| RDP | FreeRDP (xfreerdp subprocess + X11 reparent) | Rock-solid, X11 foreign surface via GDK X11 API; credentials via `/from-stdin` |
| VNC | libvncclient via FFI, or pure Rust vnc-rs | Native framebuffer rendering via Cairo |
| SFTP | russh-sftp (over existing SSH channel) | Same connection as terminal, no separate process |
| Session store | SQLite via rusqlite | Lightweight, portable, well-supported in Rust |
| Credentials | libsecret via gsecret/secret-rs | OS keyring integration |
| Session persistence | Optional tmux backend | Per-session toggle for disconnect resilience |

## Why Rust + GTK4 (Not Qt/C++)

The original analysis favored Qt for Konsole KPart and KIO SFTP. Subsequent
design decisions removed those advantages:

1. **KIO SFTP rejected** — chose libssh/russh SFTP to avoid KDE dependency
   lock-in. Both frameworks now use the same SFTP library.
2. **Plugin abstraction designed** — terminal widget is swappable, so the
   widget shouldn't drive the framework choice.
3. **VTE is not a downgrade** — it powers more terminal apps than any other
   widget. LGPL, lighter dependencies than Konsole KPart + KDE Frameworks.
4. **Rust advantages are structural** — memory safety for a network-facing
   app processing untrusted data (escape sequences, SSH packets, SFTP
   listings, VNC framebuffers). Compiler-enforced thread safety for
   concurrent SSH/SFTP/VNC sessions. tokio async for managing dozens of
   connections. These are not aesthetic preferences.
5. **Contributor ecosystem** — alacritty, wezterm, helix, zellij, bottom,
   ripgrep — the developers building modern terminal tools are writing
   them in Rust. A Rust connection manager attracts contributors.
6. **Build system** — Cargo is simpler than CMake + Qt MOC. Lower
   friction for open source contributors building from source.

## Design Documents

- [Architecture Overview](01-architecture.md)
- [Terminal Backend Abstraction](02-terminal-backend-abstraction.md)
- [Terminal Widget Comparison](03-terminal-widget-comparison.md)
- [Transport Layer Design](04-transport-layer.md)
- [Plugin System](05-plugin-system.md)
- [Session Store Schema](06-session-store.md)
- [Hotkey Engine](07-hotkey-engine.md)
- [RDP-VNC Embedding](08-rdp-vnc-embedding.md)
- [SFTP Sidebar](09-sftp-sidebar.md)
- [Build Order / MVP Roadmap](10-build-order.md)
- [SSH Authentication & Security](11-ssh-auth-security.md)

## Status

Design phase. No code yet. Docs revised 2026-08 after a design review:
push-based transport read path, shared SSH connection model,
host-key/auth spec (doc 11), hotkey table de-confliction, corrected
Remmina/SIXEL/FreeRDP facts, crate versions refreshed.