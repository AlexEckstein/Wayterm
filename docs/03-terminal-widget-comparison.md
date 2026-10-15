# Terminal Widget Comparison

Embeddable terminals for a Rust + GTK 4 app, by integration model:
**widgets** render themselves; **engines** hold state and leave rendering,
input, selection, and scrollback UI to us — thousands of lines of work.

| Library | Model | Language | License | Fit |
|---|---|---|---|---|
| **VTE** (`vte4`) | GTK 4 widget | C | LGPL | ★★★★★ chosen — the most widely used Linux terminal widget (GNOME Terminal, Tilix, Ptyxis, …) |
| libghostty-vt | engine, zero-dep C API | Zig | MIT | ★★★★ experimental second backend (`backend-ghostty`, doc 02) — fastest parsing measured below, Kitty graphics; C API not yet stable |
| alacritty_terminal | engine crate | Rust | Apache-2.0 | ★★★ possible stopgap — no FFI, but an unstable internal API |
| QTermWidget, Konsole KPart | Qt widget | C++ | GPL | ✗ wrong toolkit |
| xterm.js | browser | JS | MIT | ✗ browser only |
| Jediterm | Swing | Java | LGPL | ✗ wrong ecosystem |

VTE gaps: no Kitty graphics; SIXEL only from VTE 0.62 and opt-in, so it is
detected at runtime (doc 02).

## Decision

Ship VTE as the default. Wayterm's value is session management, SFTP,
RDP/VNC, and hotkeys, not rendering. libghostty-vt is a second backend
behind the same trait (a custom widget drawing its state, doc 02
§GhosttyBackend), with no change to the rest of the app. Because its C API
is not yet stable it is opt-in twice over: compiled only with the
`backend-ghostty` feature, against one pinned commit
(`docs/plans/phase10-pin.md`), and used only when the user ticks
"Experimental Ghostty terminal" in Settings.

## Benchmark

`benches/terminal_parse.rs` (`cargo bench --features backend-ghostty`,
under `xvfb-run -a` for the VTE half) feeds the same 8 MiB corpora to an
80×24 terminal of each backend in 16 KiB chunks — transport-sized reads —
and keeps the best of five runs. Neither side draws. libghostty-vt is
timed around its parser; VTE is fed through `VteBackend::write` and timed
until a final OSC 2 title shows it processed everything, so its figure
includes the main-loop scheduling of its input processing, which is what
a tab sees.

Pinned commit `22fa134a`, release build, AMD Ryzen 9 3900X, VTE 0.80.1,
two runs agreeing within 10%:

| Corpus | libghostty-vt MiB/s | VTE MiB/s | Ratio |
|---|---|---|---|
| plain text lines | 664 | 99 | 6.7× |
| SGR colors (16, 256, truecolor) | 84 | 54 | 1.5× |
| cursor-addressed redraws (`top`-like) | 251 | 129 | 2.0× |
| multi-byte UTF-8 (CJK, accents, emoji) | 237 | 54 | 4.4× |
