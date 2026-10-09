# Terminal Widget Comparison

Embeddable terminals for a Rust + GTK 4 app, by integration model:
**widgets** render themselves; **engines** hold state and leave rendering,
input, selection, and scrollback UI to us — thousands of lines of work.

| Library | Model | Language | License | Fit |
|---|---|---|---|---|
| **VTE** (`vte4`) | GTK 4 widget | C | LGPL | ★★★★★ chosen — the most widely used Linux terminal widget (GNOME Terminal, Tilix, Ptyxis, …) |
| libghostty-vt | engine, zero-dep C API | Zig | MIT | ★★★★ future — best-in-class parsing, Kitty graphics; C API not yet stable |
| alacritty_terminal | engine crate | Rust | Apache-2.0 | ★★★ possible stopgap — no FFI, but an unstable internal API |
| QTermWidget, Konsole KPart | Qt widget | C++ | GPL | ✗ wrong toolkit |
| xterm.js | browser | JS | MIT | ✗ browser only |
| Jediterm | Swing | Java | LGPL | ✗ wrong ecosystem |

VTE gaps: no Kitty graphics; SIXEL only from VTE 0.62 and opt-in, so it is
detected at runtime (doc 02).

## Decision

Ship VTE. Wayterm's value is session management, SFTP, RDP/VNC, and
hotkeys, not rendering. When libghostty-vt's C API stabilizes, add it as a
second backend behind the same trait (a custom widget drawing its state),
with no change to the rest of the app.
