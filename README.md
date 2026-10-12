# Wayterm

An open source, multi-protocol connection manager and terminal for Linux,
in the spirit of MobaXterm but built for a desktop that already has the
tools: saved sessions in a tree, tabbed SSH terminals with an SFTP
sidebar, local shell and serial tabs, embedded RDP and VNC tabs, and
configurable hotkeys.

App id `com.alex-eckstein.wayterm` · [github.com/AlexEckstein/Wayterm](https://github.com/AlexEckstein/Wayterm) · Apache-2.0

## Status

Phases 1–9 and 11 of the [build order](docs/10-build-order.md) are done:
session management, SSH terminal tabs with host-key verification and the
full auth chain, the SFTP sidebar, hotkeys, tab restore and a tray icon,
local shell and serial tabs, VNC and RDP tabs, tmux persistence,
import/export and packaging, and jump hosts with local port forwarding.
Doc 10 lists the known gaps in each phase and what comes next.

## Build and test

Needs GTK 4, VTE for GTK 4 (≥ 0.72), SQLite headers, and
`glib-compile-resources` for the built-in icons (Debian/Ubuntu:
`libgtk-4-dev libvte-2.91-gtk4-dev libsqlite3-dev`; the first pulls in
`libglib2.0-dev-bin`, which has the tool).

```sh
cargo run
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
scripts/run-sshd-tests.sh        # suites against a throwaway local sshd (needs sudo)
xvfb-run -a cargo test -- --ignored   # tests that need a display
```

## Packages

- `.deb`: `cargo deb` (metadata in `Cargo.toml`, output in `target/debian/`).
- Flatpak: `flatpak-builder --user --install build-dir packaging/flatpak/com.alex-eckstein.wayterm.json`.
- AppImage: pkg2appimage recipe `packaging/appimage/wayterm.yml`, built from the `.deb` in CI.
- Releases: pushing a `v*` tag runs `.github/workflows/release.yml`, which attaches the `.deb` and AppImage to the GitHub Release.

## Stack

| | Choice | Why |
|---|---|---|
| Language, UI | Rust, GTK 4 | memory safety for a network-facing app; native Linux toolkit |
| Terminal | VTE behind a `TerminalBackend` trait | the standard GTK terminal widget; an engine backend can follow |
| Async | tokio | SSH, SFTP, and reconnects off the UI thread |
| SSH, SFTP | russh, russh-sftp | pure Rust; SFTP reuses the terminal's SSH session |
| Store | SQLite (rusqlite) | sessions, settings, hotkey overrides |
| Secrets | OS keyring (libsecret) | never in the database |
| RDP, VNC | xfreerdp 3 + X11 reparenting; vnc-rs | a crash stays in xfreerdp; pure Rust, tokio-native RFB (doc 08) |
| Tray | StatusNotifierItem via ksni | no GTK3 dependency |

## Design documents

1. [Architecture](docs/01-architecture.md)
2. [Terminal backend abstraction](docs/02-terminal-backend-abstraction.md)
3. [Terminal widget comparison](docs/03-terminal-widget-comparison.md)
4. [Transport layer](docs/04-transport-layer.md)
5. [Backend plugins](docs/05-plugin-system.md)
6. [Session store](docs/06-session-store.md)
7. [Hotkey engine](docs/07-hotkey-engine.md)
8. [RDP and VNC embedding](docs/08-rdp-vnc-embedding.md)
9. [SFTP sidebar](docs/09-sftp-sidebar.md)
10. [Build order](docs/10-build-order.md)
11. [SSH authentication and security](docs/11-ssh-auth-security.md)

Contributors and agents: read [AGENTS.md](AGENTS.md) first.
