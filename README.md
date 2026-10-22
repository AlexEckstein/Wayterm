# Wayterm

An open source, multi-protocol connection manager and terminal for Linux,
in the spirit of MobaXterm but built for a desktop that already has the
tools: saved sessions in a tree, tabbed SSH terminals with an SFTP
sidebar, local shell and serial tabs, embedded RDP and VNC tabs,
configurable hotkeys, and keystroke macros.

App id `com.alex-eckstein.wayterm` · [github.com/AlexEckstein/Wayterm](https://github.com/AlexEckstein/Wayterm) · Apache-2.0

## Features

- Saved sessions in a folder tree, with icons, drag and drop, and import
  from `~/.ssh/config`, Remmina, MobaXterm and Asbru-CM.
- SSH terminal tabs: `known_hosts` verification, agent/key/password/
  keyboard-interactive auth, credentials from the OS keyring or Bitwarden,
  tmux persistence, jump hosts and local port forwards.
- An SFTP sidebar over the same SSH connection that follows the shell's
  working directory.
- Local shell and serial tabs; embedded RDP (xfreerdp) and VNC tabs.
- Automatic reconnect, restored tabs, a tray icon, rebindable hotkeys,
  and keystroke macros.

Each design doc ends with a **Limitations** section listing that
feature's known gaps.

## Build and test

Needs GTK 4, VTE for GTK 4 (≥ 0.72), SQLite headers, and
`glib-compile-resources` for the built-in icons (Debian/Ubuntu:
`libgtk-4-dev libvte-2.91-gtk4-dev libsqlite3-dev`; the first pulls in
`libglib2.0-dev-bin`, which has the tool).

```sh
cargo run
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
scripts/run-sshd-tests.sh        # suites against a throwaway local sshd (needs sudo)
# tests that need a display; force X11 so a Wayland session can't capture them
env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test -- --ignored
```
### Build with Ghostty

Needs libghostty-vt built locally first (docs/02-terminal.md, "libghostty-vt pin").

```sh
cargo build --release --features backend-ghostty
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
| Terminal | VTE behind a `TerminalBackend` trait | the standard GTK terminal widget; libghostty-vt as an opt-in second backend |
| Async | tokio | SSH, SFTP, and reconnects off the UI thread |
| SSH, SFTP | russh, russh-sftp | pure Rust; SFTP reuses the terminal's SSH session |
| Store | SQLite (rusqlite) | sessions, settings, hotkey overrides, macros |
| Secrets | OS keyring (libsecret) | never in the database |
| RDP, VNC | xfreerdp 3 + X11 reparenting; vnc-rs | a crash stays in xfreerdp; pure Rust, tokio-native RFB (doc 08) |
| Tray | StatusNotifierItem via ksni | no GTK3 dependency |

## Design documents

1. [Architecture](docs/01-architecture.md)
2. [Terminal](docs/02-terminal.md)
3. [Tabs and connections](docs/03-tabs.md)
4. [SSH](docs/04-ssh.md)
5. [Credentials](docs/05-credentials.md)
6. [Local shell and serial](docs/06-local-and-serial.md)
7. [SFTP sidebar](docs/07-sftp.md)
8. [Remote desktop](docs/08-remote-desktop.md)
9. [Sessions](docs/09-sessions.md)
10. [Import and export](docs/10-import-export.md)
11. [Hotkeys](docs/11-hotkeys.md)
12. [Keystroke macros](docs/12-macros.md)
13. [Settings](docs/13-settings.md)
14. [Building and packaging](docs/14-packaging.md)
15. [Testing](docs/15-testing.md)

Contributors and agents: read [AGENTS.md](AGENTS.md) first.
