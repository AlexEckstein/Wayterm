# Architecture Overview

Wayterm is a GTK 4 application in Rust: a side dock beside a notebook of
tabs, with a status bar below. The dock's icon tabs switch its panel
between the session tree, macros, and the current tab's SFTP sidebar;
SSH tabs pair a terminal with that sidebar; RDP and VNC tabs are planned
(doc 08).

```
┌ Main window ─────────────────────────────────────────────────────┐
│ ┌─────────┐┌ Panel ────────┐ ┌ Notebook ───────────────────────┐ │
│ │ Sessions││ session tree, │ │ SSH tab: ┌ Terminal (VTE) ────┐ │ │
│ │ Macros  ││ macros, or the│ │          │ TerminalBackend    │ │ │
│ │ SFTP    ││ tab's SFTP    │ │          └────────────────────┘ │ │
│ │         ││ sidebar       │ │                                 │ │
│ └─────────┘└───────────────┘ └─────────────────────────────────┘ │
│ Status bar: state · host · time connected                        │
└──────────────────────────────────────────────────────────────────┘
 GTK main thread ── channels ── tokio runtime (SSH, SFTP, reconnect)
```

## Side dock

A column of icon-only tabs on the window's left edge
(`src/ui/side_dock.rs`), MobaXterm style. Each tab names itself in a
tooltip and as its accessible label, and picks the panel beside the
notebook:

| Tab | Panel |
|---|---|
| Sessions | the session tree (doc 06) |
| Macros | saved keystroke macros: record, edit, play into the current terminal tab (doc 12) |
| SFTP | the current tab's SFTP sidebar, or a placeholder on tabs without one (doc 09) |

The strip sits outside the paned, so dragging the panel shut never hides
the tabs; `toggle_session_tree` hides the panel the same way, and picking
a tab brings it back (doc 07 §Handlers). Its icons ship in the binary beside the tree icons (doc 06
§Icons), so a tab never draws blank.

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
| `macros` | macro model, recorder, input tap, key notation, playback | 12 |
| `ui` | window, side dock, tabs, dialogs, session tree, sidebar, macros panel and editor, settings, tray | 01, 07, 09, 10, 12 |

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
