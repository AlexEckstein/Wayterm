# Terminal

A terminal tab renders bytes from a transport (doc 03) in a terminal
backend. The rest of the app — tabs, hotkeys, SFTP, macros — never cares
which backend draws a tab: everything goes through one trait.

Two kinds of backend fit behind it:

- **Widget backends** (VTE) own rendering, scrollback, selection and fonts.
- **Engine backends** (libghostty-vt) only hold terminal state; Wayterm
  draws it in its own GTK widget.

Code: `src/terminal/` (backends, color schemes), `src/backpressure.rs`
(the never-blocking sender), `src/ui/tabs/terminal_actions.rs` (copy, paste,
search), `src/ui/appearance.rs` (applying color schemes),
`src/ui/translucency.rs` (the window under a translucent terminal).

## The trait

`src/terminal/backend.rs`:

```rust
pub trait TerminalBackend {
    fn widget(&self) -> Widget;
    fn write(&self, data: &[u8]);            // transport → terminal, main thread
    fn send_input(&self, data: &[u8]);       // input as if typed → DataOut (doc 12)
    fn set_size(&self, rows: u32, cols: u32);
    fn features(&self) -> TerminalFeatures;
    fn set_color_scheme(&self, name: &str);  // appearance: no-op if unsupported
    fn set_font(&self, font_desc: &pango::FontDescription);
    fn set_opacity(&self, opacity: f64);
    fn set_scrollback_limit(&self, lines: u32);
    fn selected_text(&self) -> Option<String>;
    fn copy_selection(&self);
    fn paste(&self, text: &str);
    fn select_all(&self);
    fn clear(&self);
    fn search(&self, text: &str, reverse: bool);
    fn scrollback_text(&self) -> Option<String>;
    fn scroll_to_top(&self);
    fn scroll_to_bottom(&self);
    fn reset(&self);
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn description(&self) -> &str;
}

pub enum TerminalEvent { DataOut(Vec<u8>), SizeChange { rows: u32, cols: u32 },
                         TitleChanged(String), Bell, Closed }
```

- There is no `Send` bound: widget backends hold GTK objects and live on
  the main thread.
- Bytes come in through `write`, which takes bytes, not `&str`: terminal
  data is not guaranteed UTF-8. Everything going out is a `TerminalEvent`
  on a bounded channel.
- `send_input` is input that was not typed (macro playback, doc 12). It
  leaves as `DataOut` through the same queue as keystrokes.
- `TerminalFeatures` (search, color schemes, SIXEL, Kitty graphics,
  reflow, …) is queried once per tab; the UI hides what is false.
- Backends are held as `Arc<dyn TerminalBackend>`: the tab, the output
  drain and the terminal actions share them.

The trait is a module boundary (AGENTS.md): changing it is an
orchestration decision.

## Backend selection

`create_terminal()` builds the backend and its event channel together and
returns a `TerminalHandle { backend, events }`; the receiver goes to the
transport pump (doc 03 §Wiring).

- VTE is the default and the fallback.
- With the `backend-ghostty` Cargo feature, Settings' General page shows
  an "Experimental Ghostty terminal" check box that writes `ghostty` or
  `vte` to the `default_backend` row (doc 13 §Settings rows). Without the
  feature the box is disabled, with a tooltip saying to rebuild with it.
- The row is installed at startup and on change
  (`install_backend_preference`); tabs opened afterwards use it, open tabs
  keep their backend.
- Ghostty falls back to VTE when the feature is off or libghostty-vt cannot
  allocate a terminal.

A further backend would be another Cargo feature and another match arm in
`create_terminal`: type-safe, no `dlopen`. Loading backends as runtime
plugins (`.so` behind a C ABI) is not supported; it would need fixed
plugin directories, no credentials crossing the boundary, and calls on the
GTK main thread.

## Backpressure

The VTE `commit` signal (keystrokes, pastes) and the resize signals fire
on the main thread, which must never block. Every backend sends events
through an `IdleSender` (`src/backpressure.rs`):

1. it `try_send`s, and parks whatever the channel refuses;
2. on the first refusal it spawns one drain future on the main context;
3. the future sleeps in `Sender::reserve()` until the pump frees a slot,
   sends the oldest parked event, flushes what else fits, and repeats
   until the queue is empty — then it ends.

Nothing runs while the channel stays full. A new event always queues
behind parked ones, so order holds. Nothing is dropped unless the pump is
gone; then the queue is cleared and the drain stops. Keystrokes take the
fast path; large pastes exercise the queue. The SFTP sidebar sends its
commands the same way (doc 07 §Commands).

Output flows the other way through a bounded channel drained on the main
context, so a stalled UI backs up into the transport and, for SSH, into
flow control (doc 01 §Threading model).

## VteBackend

`src/terminal/vte.rs`, the GTK4 VTE widget (`vte4`):

- **Size.** `resize-window` only reports child-initiated resizes. Pane and
  window resizes surface as new row and column counts on
  `contents-changed`, which also fires on every byte of output, so only a
  changed grid is sent as `SizeChange`. Counts are clamped to at least 1.
- **Input.** `commit` sends `DataOut`. `send_input` calls `feed_child`,
  which emits `commit`, so injected input reaches every `commit` listener
  like a keystroke. The vte4 `commit` binding reads its text as a
  NUL-terminated UTF-8 string, so bytes it cannot carry (invalid UTF-8,
  NUL) are queued as `DataOut` directly instead, still in order.
- **Search** sets a PCRE2 regex (case-sensitive) with wrap-around; an
  empty or invalid query clears the highlight.
- **Color schemes:** `set_color_scheme` looks the name up among the
  built-in palettes (§Color schemes) and applies foreground, background,
  the 16 ANSI colors and the cursor colors; bold and the selection keep
  VTE's palette-derived defaults. An unknown name changes nothing.
- **Clear and reset** both call `reset(true, true)`: screen and
  scrollback.
- **SIXEL** is reported from the runtime VTE version (≥ 0.62), never
  hardcoded. VTE has no Kitty graphics.
- **Opacity** is the default background's alpha
  (`set_color_background`), kept across scheme changes (§Opacity).
  VTE's own stylesheet paints the widget's CSS background under the
  cells, so a display-wide rule above its priority clears it while the
  widget is translucent.

### Scrollback

`scrollback_text` uses `text_range_format` (VTE ≥ 0.72, crate feature
`v0_72`). Row 0 is the oldest line and `row_count` counts screen rows
only, so the range spans the scrollback limit plus the screen; an
unlimited scrollback reads up to 100 000 lines.

## Terminal actions

`src/ui/tabs/terminal_actions.rs` registers the terminal-context window
actions (doc 11 §Default actions) on the current tab's backend: copy,
paste (from the clipboard), select all, clear, and the two searches.
Both searches open a popover anchored to the terminal: Enter finds the
next match, Shift+Enter the previous one, Escape closes. `search` starts
forwards, `search_scrollback` backwards. The popover holds the backend
weakly, so a closed tab is not kept alive by it.

## Color schemes

`src/terminal/color_scheme.rs` is display-free data and logic. Two
palettes, `Light` and `Dark` (GNOME Terminal's Tango light and Tango
dark), each with a foreground, a background, cursor colors and 16 ANSI
colors; a test checks both meet WCAG AAA contrast.

A tab's scheme resolves from its session's `color_scheme` override, else
the global `default_color_scheme` row, else `System`. Values are `System`,
`Light` or `Dark`, case-insensitive; empty or unknown values fall through
to the next level, so a row naming a scheme this build lacks never breaks
a tab. New stores are seeded with `System`.

`System` is not a palette: it resolves to `Light` or `Dark` by the desktop
theme (`classify_system_theme`):

1. a GTK theme name with a `dark` segment (`Adwaita-dark`, `Breeze Dark`)
   is dark, since such a theme draws dark;
2. otherwise the desktop's stated preference decides;
3. otherwise a name with a `light` segment or a known light theme
   (`Adwaita`, `Breeze`, `Yaru`, …) is light;
4. anything else counts as dark.

The preference is xdg-desktop-portal's `org.freedesktop.appearance`
`color-scheme` (1 dark, 2 light), which `ui::appearance` reads at start
and follows through the portal's `SettingChanged` signal. Without a portal
answer it falls back to GTK's own settings: `gtk-interface-color-scheme`
on GTK 4.20+, else a set `gtk-application-prefer-dark-theme`. The portal
comes first because neither GTK setting tracks a live switch on KDE: the
theme name stays `Breeze` in dark mode, and the dark flag comes from
`settings.ini`, which GTK reads once at startup.

`ui::appearance` applies the resolved name when a tab's terminal is
created, and again for every open terminal when the global row changes
(Settings' "Default color scheme") or the desktop theme does. Backends
without color schemes (Ghostty) ignore the call. The window chrome follows
the same preference (doc 13 §Window theme).

## Opacity

`set_opacity` takes 0 (clear) to 1 (opaque); values outside are clamped
and NaN counts as opaque. Only the terminal's default background fades:
cells a program colors, the selection and the cursor stay opaque, as in
GNOME Terminal. Text is always opaque.

A backend's widget carries the `wayterm-translucent` CSS class while its
opacity is below 1. The desktop shows through only if nothing under the
terminal paints, so `src/ui/translucency.rs` follows each tab's terminal
widget (`follow`, from the connect flow): while a translucent terminal is
mapped — a notebook maps only its current page — its window carries
`wayterm-see-through`, which clears the window's background and the
notebook's page area. The chrome around the tabs (the header row, the
dock strip and panel, the status bar) carries `wayterm-chrome` and keeps
the theme's background. Switching to another tab, closing the tab or
setting opacity back to 1 restores the window. A new part of the window
outside the tabs needs `wayterm-chrome`, or it goes see-through with the
window.

## Window titles

A tab label carries a title state machine: placeholder (the session name)
→ set (the terminal's window title, `TitleChanged`) → renamed (by the
user). Once renamed, terminal titles no longer change it; a blank terminal
title falls back to the session name (doc 03 §Tab labels).

## FakeBackend

`src/terminal/fake.rs`: an in-memory backend with no widget that records
every `write`, `send_input` and interaction call, so the pump, the
bridge, macro playback and terminal-action routing are tested without a
display. Clones share the recordings.

## GhosttyBackend

An engine backend, experimental and opt-in: built only with the
`backend-ghostty` Cargo feature, against libghostty-vt from one pinned
upstream commit (§libghostty-vt pin). The default build has none of it.
`src/terminal/ghostty/`:

- **FFI.** `shim.c` wraps the C API, whose sized structs and tagged unions
  stay on the C side; `ffi.rs` declares only the shim's plain functions
  and two flat structs (a cell, a frame), size-checked against the C
  compiler by a unit test. `build.rs` compiles the shim with the `cc` crate
  and links `libghostty-vt.a` statically; nothing beyond libc is needed.
- **`Engine`.** The safe owner of one terminal, with no display: VT
  parsing, resize with reflow, scrollback limit, viewport scrolling,
  selection, plain-text export, key, paste, mouse and focus encoding, the
  mouse modes a program set, and a viewport
  snapshot with colors resolved to RGB. Effects raised while parsing —
  replies to queries (device attributes, status reports), BEL, OSC 0/2
  titles — come back from the `write` that raised them and leave as
  `DataOut`, `Bell` and `TitleChanged`.
- **`GhosttyView`.** The GTK4 widget. Each frame snapshots the engine and
  paints it with `GtkSnapshot`: one background rectangle per run of equal
  color, one Pango layout per run of equal style (bold, italic, underline,
  strikethrough, faint; inverse, the selection and the block cursor swap
  colors). ASCII runs share a layout; any other character is placed at
  its own column, so wide glyphs keep the grid aligned. A changed
  allocation resizes the grid and sends `SizeChange` once per new grid.
- **Input.** Keys go through libghostty-vt's key encoder, so cursor-key
  and Kitty keyboard modes apply. Shift+PageUp/PageDown scroll a page and
  Shift+Home/End jump to the top and bottom, as in VTE, instead of
  reaching the remote side. Each mouse event goes one of three ways
  (`route` in `view.rs`, read from the terminal's modes per event):
  - **Reported** while the program tracks the mouse (mode 9, 1000, 1002
    or 1003): primary-button presses, drags and releases, motion, and the
    wheel (as buttons 4 and 5) go through libghostty-vt's mouse encoder,
    which the terminal's modes also give its format — X10 bytes unless
    the program picked SGR (1006), UTF-8, urxvt or SGR-pixels — and which
    drops what the tracking mode does not report. A reported press
    selects nothing. Each report is a `DataOut` chunk of its own, so
    macro recording skips it (doc 12 §Recording).
  - **Arrow keys** for the wheel on the alternate screen (1047 or 1049)
    under alternate scroll (mode 1007, on until the program resets it),
    when no tracking mode is set: Up or Down per row, in the cursor-key
    mode's form.
  - **Local** otherwise: a primary-button drag selects, and the wheel
    scrolls the viewport.

  Holding Shift keeps a press, its drag and the wheel local even while
  the program tracks the mouse. The wheel counts three rows per notch,
  adding up the fractional steps of high-resolution wheels and touchpads
  (pixel deltas count by cell height); a reported or arrow-key wheel
  sends one report or key per row. With mode 1004 set, keyboard focus
  entering and leaving the widget sends `CSI I` and `CSI O`.
- **Backend.** Events go through the same never-blocking sender as VTE's
  (§Backpressure). `send_input` and `paste` queue `DataOut` like a key
  press, so macros work unchanged; `paste` is bracketed when the program
  enabled mode 2004. `clear` and `reset` both fully reset, as VTE's
  `clear` does. `set_size` resizes the grid and makes it the widget's
  natural size. `set_opacity` paints the frame's background rectangle
  with that alpha; colored cell runs stay opaque (§Opacity).

### libghostty-vt pin

libghostty-vt has no tagged release and its C API still moves, so the pin
changes only in a deliberate bump: rebuild, rerun the checks below, and
update this section and the comment in `build.rs` together.

| | |
|---|---|
| Upstream | <https://github.com/ghostty-org/ghostty> |
| Commit | `22fa134a07f5749edd9e92c6bc17d8aaf61843be` (2026-10-15) |
| Zig | 0.16.0 (`minimum_zig_version` in upstream `build.zig.zon`) |
| Library version | `0.1.0-dev` (upstream pkg-config) |

Build it outside the Wayterm checkout:

```sh
# Zig 0.16.0, user-local (sha256 from ziglang.org/download/index.json:
# 70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00)
curl -LO https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz
tar -xJf zig-x86_64-linux-0.16.0.tar.xz -C ~/.local/zig
ln -s ~/.local/zig/zig-x86_64-linux-0.16.0/zig ~/.local/bin/zig

git clone https://github.com/ghostty-org/ghostty.git ghostty-build
cd ghostty-build
git checkout --detach 22fa134a07f5749edd9e92c6bc17d8aaf61843be
zig build -Demit-lib-vt -Doptimize=ReleaseFast --prefix "$PWD/out"
```

`zig build` fetches upstream's own hash-pinned dependencies into
`~/.cache/zig/p`; nothing else is downloaded. `out/` then holds
`include/ghostty/vt.h` (+ `vt/`) and `lib/libghostty-vt.a`.

`build.rs` looks for them under `$WAYTERM_GHOSTTY_VT_DIR`, default
`third_party/libghostty-vt/` in the checkout:

```sh
mkdir -p third_party/libghostty-vt/lib
cp -r ghostty-build/out/include third_party/libghostty-vt/
cp ghostty-build/out/lib/libghostty-vt.a third_party/libghostty-vt/lib/
printf '*\n' > third_party/libghostty-vt/.gitignore   # never committed
```

The library is built per machine and never committed. Without it, a build
with the feature fails in `build.rs` with a pointer here; the default
build never looks for it. Checks for a pin (doc 15 §Ghostty backend):
clippy and `cargo test` with `--features backend-ghostty`, the widget
under xvfb, and the benchmark.

## Widget choice

Embeddable terminals for a Rust + GTK 4 app:

| Library | Model | Language | License | Verdict |
|---|---|---|---|---|
| **VTE** (`vte4`) | GTK 4 widget | C | LGPL | default — the most widely used Linux terminal widget (GNOME Terminal, Tilix, Ptyxis) |
| libghostty-vt | engine, C API | Zig | MIT | opt-in second backend — fastest parsing (§Benchmark), Kitty graphics; C API not yet stable |
| alacritty_terminal | engine crate | Rust | Apache-2.0 | possible — no FFI, but an unstable internal API |
| QTermWidget, Konsole KPart | Qt widget | C++ | GPL | wrong toolkit |
| xterm.js | browser | JS | MIT | browser only |
| Jediterm | Swing | Java | LGPL | wrong ecosystem |

Wayterm's value is session management, SFTP, remote desktops and
hotkeys, not rendering, so VTE is the default. libghostty-vt sits behind
the same trait with no change to the rest of the app; because its C API is
unstable it is opt-in twice over: compiled only with the feature, against
one pinned commit, and used only when the user ticks the Settings box.

## Benchmark

`benches/terminal_parse.rs` (`cargo bench --features backend-ghostty`,
under `xvfb-run -a` for the VTE half) feeds the same 8 MiB corpora to an
80×24 terminal of each backend in 16 KiB chunks — transport-sized reads —
and keeps the best of five runs. Neither side draws. libghostty-vt is
timed around its parser; VTE is fed through `VteBackend::write` and timed
until a final OSC 2 title shows it processed everything, so its figure
includes the main-loop scheduling of its input processing, which is what a
tab sees.

Pinned commit `22fa134a`, release build, AMD Ryzen 9 3900X, VTE 0.80.1,
two runs agreeing within 10%:

| Corpus | libghostty-vt MiB/s | VTE MiB/s | Ratio |
|---|---|---|---|
| plain text lines | 664 | 99 | 6.7× |
| SGR colors (16, 256, truecolor) | 84 | 54 | 1.5× |
| cursor-addressed redraws (`top`-like) | 251 | 129 | 2.0× |
| multi-byte UTF-8 (CJK, accents, emoji) | 237 | 54 | 4.4× |

## Limitations

- **Font, opacity and scrollback settings are not applied.** The
  `default_font_family`, `default_font_size`, `default_opacity` and
  `scrollback_limit` rows and the per-session `font_family`, `font_size`
  and `opacity` columns are stored, but no tab reads them: terminals use
  the backend's own defaults. Both backends implement `set_opacity`
  (§Opacity), but nothing calls it yet.
- **Opacity needs a compositor.** Every Wayland session has one; on X11
  without a compositing manager the cleared areas draw dark instead of
  showing the desktop.
- **The per-session `terminal_backend` override is not read:**
  `create_terminal()` follows the global row only.
- **No per-session color scheme picker.** The session dialog has none; an
  override already in the store (imported, restored) is honored.
- **VTE:** no Kitty graphics.
- **Ghostty:** no search, color schemes, URL detection or image
  drawing (`features()` says so, and the UI hides them); no input method
  or compose support; only a grapheme's base codepoint is drawn; mouse
  reporting covers the primary button and the wheel only, so middle and
  right clicks and horizontal scrolling never reach the program. CI does
  not build libghostty-vt, so the feature is checked locally only.
