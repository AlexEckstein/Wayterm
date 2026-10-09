# Architecture Overview

Wayterm is a GTK 4 application in Rust: a session tree beside a notebook of
tabs, with a status bar below. SSH tabs pair a terminal with an SFTP
sidebar; RDP and VNC tabs are planned (doc 08).

```
┌ Main window ─────────────────────────────────────────────────────┐
│ ┌ Session tree ┐ ┌ Notebook ───────────────────────────────────┐ │
│ │ folders      │ │ SSH tab: ┌ SFTP sidebar ┐┌ Terminal (VTE) ┐ │ │
│ │ sessions     │ │          │ russh-sftp   ││ TerminalBackend│ │ │
│ │ (SQLite)     │ │          └──────────────┘└────────────────┘ │ │
│ └──────────────┘ └─────────────────────────────────────────────┘ │
│ Status bar: state · host · time connected                        │
└──────────────────────────────────────────────────────────────────┘
 GTK main thread ── channels ── tokio runtime (SSH, SFTP, reconnect)
```

## Principles

1. **Transport and terminal are separate.** A `Transport` (doc 04) moves
   bytes; a `TerminalBackend` (doc 02) renders them. Neither knows the
   other — they meet in `TerminalTab` through channels.
2. **One thread for GTK, tokio for I/O.** Widgets are touched only on the
   main thread; network work runs on a process-wide tokio runtime. Every
   data path crosses the boundary once, through a bounded channel, and the
   GTK side never blocks on it (doc 02 §Backpressure).
3. **One SSH session, many consumers.** An authenticated `SshConnection`
   is shared by the terminal, the SFTP sidebar, and future tunnels; the
   connection supervisor releases it after the last consumer (doc 04).
4. **Features degrade, buttons never lie.** Backends report
   `TerminalFeatures`; the UI hides what is unsupported. A missing tray
   host means close quits; a missing keyring means the user is prompted.
5. **Secrets stay in the keyring.** Never in SQLite, logs, argv, or the
   environment (docs 06, 11).
6. **The traits are the seams.** `Transport`, `TerminalBackend`,
   `SftpCommand`, and `SshConnection` are module boundaries; changing them
   is an orchestration decision (AGENTS.md).

## Modules

| Module | Role | Doc |
|---|---|---|
| `terminal` | `TerminalBackend`, `VteBackend`, `FakeBackend` | 02, 03, 05 |
| `backpressure` | GTK-side try-send queue | 02 |
| `transport` | `Transport`, `SSHTransport`, `SshConnection`, supervisor, auth chain, host keys, credentials, reconnect, `TerminalTab` | 04, 11 |
| `sftp` | SFTP task and commands | 09 |
| `store` | SQLite store and session model | 06 |
| `hotkeys` | action registry, conflicts, defaults, persistence | 07 |
| `ui` | window, tabs, dialogs, session tree, sidebar, settings, tray | 07, 09, 10 |

The tray is a StatusNotifierItem over D-Bus via `ksni` (pure Rust, no GTK3
bindings); disconnect alerts use `gio::Notification`. Without a tray host
(headless, GNOME without the AppIndicator extension) init fails fast and
closing the window quits.

## Known constraints

- **RDP embedding needs X11 or XWayland.** GTK 4 has no GtkSocket; doc 08
  reparents the xfreerdp window and is the highest-risk item.
- **VTE is the only production GTK 4 terminal widget.** The trait keeps an
  engine backend (libghostty-vt) possible (doc 03).
- **VNC:** plain VNC over an SSH tunnel first; VeNCrypt later.
