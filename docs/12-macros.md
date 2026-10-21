# Keystroke Macros

A macro is what was typed into a terminal tab, pauses included, saved
under a name, editable, and played back into any terminal tab. Code:
`src/macros/` (model, recording, notation, playback) and
`src/ui/macros_panel.rs`, `src/ui/macro_editor.rs` (the side dock's
Macros panel, doc 01, and the editor).

## Model

```rust
pub struct MacroStep { pub delay_ms: u32, pub keys: Vec<u8> }
pub struct Macro     { pub id: i64, pub name: String, pub steps: Vec<MacroStep> }
```

A step waits `delay_ms`, then sends `keys` as if typed. Empty keys make a
pure pause. A macro's duration is the sum of its pauses.

## Recording

`InputTap::wrap` splices a relay between a terminal's event channel and
its transport pump on every connect
(`ui::tab::terminal::finish_connect`). Every `TerminalEvent` passes
through unchanged and in order; the relay awaits room in the pump's
channel, so backpressure still reaches the backend's queue (doc 02
§Backpressure). While a recording is armed, each `DataOut`
chunk is also timestamped into a `Recorder`:

- each chunk becomes a step whose pause is the time since the previous
  chunk, in whole milliseconds (saturating at `u32::MAX`);
- the first chunk gets no pause: the time before it is the user reaching
  the terminal;
- a chunk within the same millisecond as the previous one joins its step;
- mouse and focus reports are skipped — VTE sends them through the same
  channel once an application enables them (tmux, vim, htop), each as a
  chunk of its own: `CSI I`/`CSI O`, and X10, SGR and urxvt mouse
  encodings.

The relay sees whatever the backend sends, so recording needs nothing
from the backend: pastes are recorded as one step, and a macro played
during a recording is recorded too. Timestamps are taken as the relay
receives each chunk — within a main-loop iteration of the keystroke,
unless the transport is applying backpressure, which can bunch pauses
together.

A recording stays on the tab it started in; switching tabs does not move
it. Stop returns what was recorded, even if that tab has since closed.

### Bytes, not keys

Steps hold the bytes the terminal sent, not key names. The same key can
send different bytes depending on the terminal's mode: Up is `CSI A`
normally but `SS3 A` in application cursor mode (vim, less), and an
engine backend (doc 03) could use the kitty keyboard protocol when an
application asks for it. A macro replays the bytes as recorded, whatever
mode the terminal is in by then. Recording logical keys instead would
need the backend to encode synthetic key events, which VTE cannot do.

## Playback

`Playback::start` runs on the GTK main context: for each step it waits
the pause (a GLib timeout, millisecond resolution), then calls
`TerminalBackend::send_input` (doc 02) — input as if typed, through the
same queue as keystrokes. VTE implements it with `feed_child`, which
emits `commit`, so replayed keys reach every `commit` listener (the SFTP
sidebar's directory follower, doc 09) exactly like typed ones. The vte4
`commit` binding reads its text as a NUL-terminated UTF-8 string, so
bytes it cannot carry (invalid UTF-8, NUL) skip `feed_child` and join
the same queue directly, still in order.

Playback holds the terminal weakly and ends early when the tab closes or
reconnects (a redial builds a new terminal). Stop ends it before the
next step. Playback targets the tab that was current when it started.

## Notation

The editor shows each step's keys as text. Text stands for itself;
everything else is a token, names case-insensitive:

| Token | Bytes |
|---|---|
| `<Enter>`, `<Tab>`, `<Esc>`, `<Backspace>` | `0D`, `09`, `1B`, `7F` |
| `<Ctrl-A>` … `<Ctrl-Z>`, `<Ctrl-@>`, `<Ctrl-[>`, `<Ctrl-\>`, `<Ctrl-]>`, `<Ctrl-^>`, `<Ctrl-_>` | the C0 controls |
| `<Up>`, `<Down>`, `<Right>`, `<Left>`, `<Home>`, `<End>` | `CSI A`…`D`, `CSI H`, `CSI F` |
| `<Insert>`, `<Delete>`, `<PageUp>`, `<PageDown>`, `<Shift-Tab>` | `CSI 2~`, `CSI 3~`, `CSI 5~`, `CSI 6~`, `CSI Z` |
| `<F1>` … `<F12>` | `SS3 P`…`S`, `CSI 15~` … `CSI 24~` |
| `<0xNN>` | any byte (invalid UTF-8, C1 controls) |
| `<Space>` | a space; written for one at either end of the keys, where it would be invisible |
| `<lt>` | a literal `<` |

The named sequences are what VTE sends outside application
cursor/keypad mode; other sequences show as `<Esc>` and text
(`<Esc>OA`). Decoding inverts encoding for every byte string (tested
exhaustively over all byte pairs). An unknown or unclosed token is an
error, never literal text, so a typo cannot reach the remote shell.

## Editor

A modal window: the name, then one row per step — its number, the pause
in milliseconds, the keys in the notation, and buttons to insert a step
below, move up, move down, and delete — with Add Step, a summary
(`9 steps · 2.1 s`), and a notation reminder. A row whose keys do not
parse is marked, the first error is shown, and Save stays disabled until
it parses and the name is not blank. It opens after every recording
(titled New Macro, named `Macro N`, the lowest free number) and from the
panel's Edit. `Draft` is its display-free model.

## Panel

The side dock's Macros panel (doc 01): Record at the top, the macros by
name (case-insensitive) with each one's summary, and Play, Edit and
Delete for the selected one. Record arms the current tab's tap and moves
focus to its terminal; it becomes Stop, and stopping opens the editor
(nothing recorded → a note instead). Play becomes Stop while playing;
double-click or Enter on a macro plays it. On a tab without a terminal
— Home, RDP, VNC, a tab not yet connected — Record and Play explain
instead of acting. Buttons never take keyboard focus, so it stays in the
terminal being recorded or played into.

## Storage

The `macros` table (schema v5, created on open like doc 06's other
idempotent steps):

```sql
CREATE TABLE IF NOT EXISTS macros (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    steps       TEXT NOT NULL,   -- JSON: [{"delay_ms": 120, "keys": [108]}, …]
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
```

`keys` is a byte array so non-UTF-8 input survives; the wire format is
locked by `macros::model` tests. Macros are global, not per session.

Steps are stored as typed, in plain text, like other user data. Wayterm
cannot tell a password typed at a prompt from a command, and by decision
(2026-10-12) shows no recording warning; macro contents are never
logged.

## Not done

- Terminal tabs only (SSH, local shell, serial): RDP and VNC tabs pass
  keys to the remote session and have no tap.
- No hotkeys to play a macro, no playback into several tabs at once,
  and macros are not part of the session import/export (doc 06).

## Testing

Display-free: the recorder's pause arithmetic and report filtering, the
notation (exhaustive roundtrip), the tap's ordering and arming, playback
order, stop and a dropped terminal (`FakeBackend`), the editor's `Draft`
operations and validation, and the store's CRUD and migration. Under
xvfb (`ui::gtk_tests`): the editor's rows and validation, the panel's
list and toolbar, and a record-then-play roundtrip through a real VTE
terminal, including a non-UTF-8 byte.
