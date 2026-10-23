# Architecture

Wayterm is a GTK 4 application in Rust: a side dock beside a notebook of
session tabs, a header bar above and a status bar below. This doc is the
map; each feature has its own doc (§Documents).

## Window layout

```
┌ Main window ───────────────────────────────── [Tunnels…] [Settings] ┐
│ ┌──────┐┌ Panel ─────────┐ ┌ Notebook ────────────────────────────┐ │
│ │ Sess.││ session tree,  │ │ [Home] [prod ×] [db ×] [+]            │ │
│ │ Macro││ macros, or the │ │ SSH tab:   terminal (TerminalBackend) │ │
│ │ SFTP ││ tab's SFTP     │ │ RDP tab:   embedded xfreerdp window   │ │
│ │      ││ sidebar        │ │ VNC tab:   framebuffer view           │ │
│ └──────┘└────────────────┘ └──────────────────────────────────────┘ │
│ Status bar: ● state · host · time connected                         │
└─────────────────────────────────────────────────────────────────────┘
 GTK main thread ── bounded channels ── tokio runtime (SSH, SFTP, VNC, RDP, reconnect)
```

`src/ui/main_window.rs` builds it and owns the window-wide connection
supervisor (doc 04 §Connection supervisor) and the router that decides
which tab drives the status bar (doc 03 §Status bar). The header bar holds
**Tunnels…** (doc 04 §Tunnels dialog) and **Settings** (doc 13).

### Side dock

A column of icon-only tabs on the window's left edge
(`src/ui/panels/side_dock.rs`), MobaXterm style. Each names itself in a tooltip
and as its accessible label, and picks the panel beside the notebook:

| Tab | Panel |
|---|---|
| Sessions | the session tree (doc 09 §Session tree) |
| Macros | saved keystroke macros: record, edit, play into the current terminal tab (doc 12 §Panel) |
| SFTP | the current tab's SFTP sidebar, or a placeholder on tabs without one (doc 07 §Layout) |

The strip sits outside the paned, so dragging the panel shut never hides
the tabs; `toggle_session_tree` hides the panel the same way, and picking
a tab brings it back (doc 11 §Handlers). Its icons ship in the binary
beside the tree icons (doc 09 §Icons), so a tab never draws blank.

### Tab bar

The notebook's tab bar (`src/ui/tabs/bar.rs`) ends in a "+" drawn as a
tab, right after the last tab, that opens a local shell (doc 03 §New Tab).
GtkNotebook stretches its tab strip over the whole header, so the bar has
two layouts, picked after every frame: while the tabs fit, the strip takes
its natural width with "+" after it; once they overflow, the strip
scrolls, with arrows at both ends, and "+" stays pinned at the bar's end.

## Threading model

GTK widgets are touched on the main thread only. A process-wide,
multi-threaded tokio runtime is entered on the main thread at startup
(`src/app.rs`), so GTK callbacks can spawn onto it; network and process
I/O runs there. Every data path crosses the boundary once, through a
bounded channel:

```
keystrokes  terminal ─IdleSender─► pump task ─► Transport::write / resize
output      reader task ─► bridge task ─► drain (main context) ─► backend.write
```

- The GTK side never blocks: overflow is parked and drained by a
  main-context future that waits for channel capacity (doc 02
  §Backpressure).
- Output channels are bounded, so a stalled UI backs up into the reader
  and then into SSH flow control: the server slows down instead of memory
  growing.
- Anything that waits on the user — host-key and credential prompts —
  runs the dial on tokio's blocking pool and hops each prompt to the main
  thread (doc 04 §Connection supervisor).

## Principles

1. **Transport and terminal are separate.** A `Transport` moves bytes; a
   `TerminalBackend` renders them. Neither knows the other — they meet in
   `TerminalTab` through channels (doc 03 §Wiring).
2. **One thread for GTK, tokio for I/O** (§Threading model).
3. **One SSH session, many consumers.** An authenticated `SshConnection`
   is shared by the terminal, the SFTP sidebar and port forwards; the
   supervisor releases it after the last consumer.
4. **Features degrade, buttons never lie.** Backends report
   `TerminalFeatures` and the UI hides what is unsupported. A missing tray
   host means close quits; a missing keyring means the user is prompted.
5. **Secrets stay in the keyring.** Never in SQLite, logs, argv, or the
   environment of a child that does not need them (doc 05).
6. **Logic is display-free.** Each widget has a plain-data model beside it
   (`*/model.rs`, `status_view_from`, `Draft`, …), so behavior is unit
   tested without a display (doc 15).

## Seams

These are module boundaries; changing one is an orchestration decision
(AGENTS.md):

| Seam | Doc |
|---|---|
| `TerminalBackend` (`src/terminal/backend.rs`) | doc 02 §The trait |
| `Transport` (`src/transport/types.rs`) | doc 03 §The Transport trait |
| `SshConnection` (`src/transport/connection.rs`) | doc 04 §Connection supervisor |
| `SftpCommand` (`src/sftp/commands.rs`) | doc 07 §Commands |

## Modules

| Module | Role | Doc |
|---|---|---|
| `app` | startup: tokio runtime, data paths, main window, tray | 01 |
| `terminal` | `TerminalBackend`, `VteBackend`, `GhosttyBackend`, `FakeBackend`, color schemes | 02 |
| `backpressure` | the GTK-side never-blocking sender | 02 |
| `transport` | `Transport`, SSH (connection, supervisor, host keys, auth, jump, forward), local, serial, reconnect, credentials, factory | 03, 04, 05, 06 |
| `sftp` | the SFTP task, its commands, the shell-directory probe | 07 |
| `rdp` | the xfreerdp process and certificate trust | 08 |
| `vnc` | the RFB engine and framebuffer | 08 |
| `store` | the SQLite store and session model | 09 |
| `transfer` | session import and export | 10 |
| `hotkeys` | action registry, conflicts, defaults, triggers, persistence | 11 |
| `macros` | macro model, recorder, input tap, key notation, playback | 12 |
| `ui` | the window, settings, status bar, tray, appearance; `ui::tabs` (tab pages, connect flows, lifecycle, labels, restore), `ui::dialogs`, `ui::panels` (side dock, session tree, SFTP sidebar, macros panel and editor) | 01, 03, 07, 09, 11, 12, 13 |

## Documents

1. [Architecture](01-architecture.md) — this map
2. [Terminal](02-terminal.md) — backends, backpressure, color schemes, libghostty-vt
3. [Tabs and connections](03-tabs.md) — the Transport trait, wiring, lifecycle, reconnect, restore, status bar, tray
4. [SSH](04-ssh.md) — supervisor, host keys, authentication, tmux, jump hosts, port forwarding
5. [Credentials](05-credentials.md) — keyring, Bitwarden, prompts
6. [Local shell and serial](06-local-and-serial.md)
7. [SFTP sidebar](07-sftp.md)
8. [Remote desktop](08-remote-desktop.md) — RDP and VNC
9. [Sessions](09-sessions.md) — the store, session tree, session dialog, icons
10. [Import and export](10-import-export.md)
11. [Hotkeys](11-hotkeys.md)
12. [Keystroke macros](12-macros.md)
13. [Settings](13-settings.md) — the Settings window, settings rows, window theme
14. [Building and packaging](14-packaging.md)
15. [Testing](15-testing.md)

Each feature doc ends with its **Limitations**: what is not supported or
not yet wired, in one place per feature.
