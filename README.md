# Wayterm

An open source, multi-protocol connection manager and terminal for Linux,
in the spirit of MobaXterm but built for a desktop that already has the
tools: saved sessions in a tree, tabbed SSH terminals with an SFTP
sidebar, configurable hotkeys, and (planned) RDP and VNC tabs.

App id `com.alex-eckstein.wayterm` · [github.com/AlexEckstein/Wayterm](https://github.com/AlexEckstein/Wayterm) · Apache-2.0

## Status

Phases 1–4 of the [build order](docs/10-build-order.md) are done: session
management, SSH terminal tabs with host-key verification and the full auth
chain, the SFTP sidebar, hotkey settings, tab restore, and a tray icon.
Doc 10 lists what is still to be wired (hotkey accelerators, reconnect)
and what comes next (local shell and serial, VNC, RDP).

## Build and test

Needs GTK 4, VTE for GTK 4 (≥ 0.72) and SQLite headers (Debian/Ubuntu:
`libgtk-4-dev libvte-2.91-gtk4-dev libsqlite3-dev`).

```sh
cargo run
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
scripts/run-sshd-tests.sh        # suites against a throwaway local sshd (needs sudo)
xvfb-run -a cargo test -- --ignored   # tests that need a display
```

## Stack

| | Choice | Why |
|---|---|---|
| Language, UI | Rust, GTK 4 | memory safety for a network-facing app; native Linux toolkit |
| Terminal | VTE behind a `TerminalBackend` trait | the standard GTK terminal widget; an engine backend can follow |
| Async | tokio | SSH, SFTP, and reconnects off the UI thread |
| SSH, SFTP | russh, russh-sftp | pure Rust; SFTP reuses the terminal's SSH session |
| Store | SQLite (rusqlite) | sessions, settings, hotkey overrides |
| Secrets | OS keyring (libsecret) | never in the database |
| RDP, VNC | xfreerdp + X11 reparenting; libvncclient or vnc-rs | planned (doc 08) |
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
