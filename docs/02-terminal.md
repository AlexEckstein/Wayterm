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
search), `src/ui/appearance.rs` (applying color schemes, fonts, opacity
and scrollback), `src/terminal/defaults.rs` (resolving the last three),
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
- `TerminalFeatures` (search, color schemes, URL detection, SIXEL, Kitty
  graphics, reflow, …) states what a backend supports. No UI reads it yet;
  each backend's section says what the user gets.
- Backends are held as `Arc<dyn TerminalBackend>`: the tab, the output
  drain and the terminal actions share them.

The trait is a module boundary (AGENTS.md): changing it is an
orchestration decision.

## Backend selection

`create_terminal_for(kind)` builds the backend and its event channel
together and returns a `TerminalHandle { backend, events }`; the receiver
goes to the transport pump (doc 03 §Wiring). `create_terminal()` builds the
installed preference.

- VTE is the default and the fallback.
- With the `backend-ghostty` Cargo feature, Settings' General page shows
  an "Experimental Ghostty terminal" check box that writes `ghostty` or
  `vte` to the `default_backend` row (doc 13 §Settings rows). Without the
  feature the box is disabled, with a tooltip saying to rebuild with it.
- The row is installed at startup and on change
  (`install_backend_preference`); tabs opened afterwards use it, open tabs
  keep their backend.
- A session's `terminal_backend` column (`ghostty` or `vte`, doc 09) wins
  over the row for that session's tabs; an empty or unknown value follows
  the row. The session dialog does not edit it (doc 09 §Limitations).
- Ghostty falls back to VTE, with a note on stderr, when the feature is off
  or libghostty-vt cannot allocate a terminal.

A further backend would be another Cargo feature and another match arm in
`create_terminal_for`: type-safe, no `dlopen`. Loading backends as runtime
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
- **Search** sets a PCRE2 regex (case-sensitive) with wrap-around. The
  current match is the selection, and a lone match stays selected when
  stepped; an empty or invalid query clears it.
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
created, before its widget is shown, and again for every open terminal
when the global row changes (Settings' "Default color scheme") or the
desktop theme does. Both backends apply it (§VteBackend,
§GhosttyBackend). The window chrome follows the same preference (doc 13
§Window theme).

## Font, opacity and scrollback

`src/terminal/defaults.rs` is display-free. A tab's font family, font size
and opacity each resolve from its session's column, else the global row
(`default_font_family`, `default_font_size`, `default_opacity`), else the
seed value (doc 13 §Settings rows). Scrollback is global only:
`scrollback_limit`, in lines, 0 for none. As with color schemes, an unusable
value falls through to the next level: a blank family, a font size outside
0–512 points, or a number that does not parse.

`ui::appearance` applies them when a tab's terminal is created, before its
widget is shown, so the first grid is measured in the right font. The rows
are installed at startup and whenever the General or Customize settings
page saves (`install_terminal_defaults`); a change re-applies them to every
open terminal, and an unchanged install touches nothing. A desktop theme
switch does not re-apply them.

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

## Reported directory

A shell may report its directory at each prompt with OSC 7
(`file://host/path`, percent-encoded). Every terminal tab, SSH, local or
serial and on either backend, keeps the last report on its page:
`finish_connect` (`src/ui/tabs/terminal.rs`) connects to VTE's
`current-directory-uri-changed` signal or the Ghostty view's
`connect_directory` hook, so `TerminalBackend` and `TerminalEvent` are
unchanged. `SessionTab::shell_directory_for_page` returns it.

- **Decoded.** Both backends pass the raw URI on; the tab keeps the
  percent-decoded path (`/srv/my site`), read by the same
  `DirectoryFollower` as the SFTP sidebar. Its other rules apply too: the
  first report pins the host, so a nested `ssh` does not replace the
  directory, and anything that is not a `file://` URI is dropped (doc 07
  §Following the shell).
- **One terminal's.** The value starts empty with each new terminal, on a
  reconnect too, and a late report from the old terminal goes nowhere. A
  shell that exited keeps its last directory until the tab's next
  terminal.

Nothing reads the kept value yet: what consumes it (a new tab in the same
directory, say) is a separate decision. The SFTP sidebar does not use it;
it follows the same signals itself, and also probes, on SSH tabs only.

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
  and five flat structs (a cell, a frame, a span of search highlight, an
  image placement, an image's pixels), checked against the C compiler by
  unit tests: their sizes, and where the cell's grapheme, link, wrap and
  underline fields, and the fields after any padding, sit. `build.rs`
  compiles
  nothing beyond libc is needed.
- **`Engine`.** The safe owner of one terminal, with no display: VT
  parsing, resize with reflow, scrollback limit, viewport scrolling,
  selection, search, plain-text export, key, paste, mouse and focus
  encoding, the mouse modes a program set, and a viewport snapshot with
  colors resolved to RGB and the cursor's style and blink. A snapshot
  cell holds its whole grapheme cluster: the base codepoint, or for a
  longer cluster a slice of a
  codepoint buffer that grows as needed and is reused frame to frame;
  the tail cell of a wide character holds nothing. A cell with an OSC 8
  hyperlink points at its entry in the snapshot's list of links. A cell
  also holds its
  underline style (`GHOSTTY_SGR_UNDERLINE_*`: none, single, double, curly,
  dotted or dashed) and its SGR 58 underline color, if it has one, a
  palette index resolved through the active palette; SGR 59 drops the
  color, and SGR 24 and 4:0 the style. A row's last cell
  says whether the row soft-wraps into the next. A snapshot also lists
  the Kitty images on screen (§Kitty graphics). Effects raised while
  parsing — replies to queries (device attributes, status reports), BEL,
  OSC 0/2
  titles, OSC 7 directories, clipboard writes — come back from the `write`
  that raised them and leave as `DataOut`, `Bell` and `TitleChanged`; a
  directory goes to the view's directory hook instead (§Reported
  directory), and a clipboard write to the view (§GhosttyBackend,
  Program clipboard).
- **`GhosttyView`.** The GTK4 widget. Each frame snapshots the engine and
  paints it with `GtkSnapshot`: one background rectangle per run of equal
  color, and per run of equal style (bold, italic, strikethrough, faint;
  inverse, the selection and the block cursor swap colors) glyphs that
  Pango picks and shapes but does not place. The cell is the font's "M"
  rounded up to whole pixels; each glyph goes at its own cell, not where
  the font's advances would take it, since those are rarely whole pixels
  and would drift off the grid along a line. A ligature sits at its first
  cell. A cell's whole grapheme cluster — a base with its combining marks,
  a ZWJ sequence, a flag, an emoji with its variation selector — is shaped
  together and sits at that cell; a wide cluster's tail cell adds a blank,
  so the cells after it keep their columns. Strikethrough is a rectangle
  across the run, on whole pixels. Underlines have runs of their own, of
  equal style and color, so a line carries on under text that changes
  color, and they are drawn in the cell's underline color, otherwise the
  text color, fading with faint text and absent under hidden text. Single
  and double are Pango's: the line of a blank shaped to the run's width,
  moved and stretched onto whole pixels. Pango has no dotted or dashed
  underline, so those are rectangles in a pattern counted from the grid's
  left edge, and GTK draws Pango's error underline as round dots, so curly
  is a stroked wave, a crest and a trough per cell, reaching to the cell's
  bottom. Blinking text (SGR 5 or 6, until SGR 25) goes out and back
  once a second, half a second each way, its lines with it; the cell keeps
  its place and background, and selection and search reach it as ever.
  The timer runs only while the view is mapped and text that would change
  shows on screen. A frame paints the whole grid's backgrounds, then its
  text, with images between them (§Kitty graphics). A changed allocation
       resizes the grid and sends `SizeChange`
  once per new grid.
- **Cursor.** It takes the shape the terminal asks for: block, underline
  or bar, per DECSCUSR. The block swaps its cell's colors. The others are
  rectangles as thick as the font's underline, on the cell's bottom or
  left edge, over the cell as painted, so a search match stays lit under
  them; they take the cursor color a scheme or program set, else the
  color of the cell's text. Without focus the cursor is a hollow block in
  the same way, on all four edges, whatever its shape, and it does not
  blink (`cursor_shape` in `view.rs`, in the order Ghostty's renderer
  decides it).

  A blink (DECSCUSR's blinking variants, or mode 12) is 600 ms on and
  600 ms off, Ghostty's period. A timer flips the phase and queues a
  draw, so a phase is a whole frame like any other change. The paint
  starts the timer when a frame's cursor blinks, and stops it once the
  cursor stops blinking, is hidden or leaves the viewport. Losing focus
  and unmapping stop it too, so hidden tabs wake nothing, and gaining
  focus starts the blink over with the cursor on.
- **Input.** Keys go through libghostty-vt's key encoder, so cursor-key
  and Kitty keyboard modes apply. Shift+PageUp/PageDown scroll a page and
  Shift+Home/End jump to the top and bottom, as in VTE, instead of
  reaching the remote side. Each mouse event goes one of three ways
  (`route` in `view.rs`, read from the terminal's modes per event):
  - **Reported** while the program tracks the mouse (mode 9, 1000, 1002
    or 1003): presses, drags and releases of the primary, middle and
    right buttons, motion, and the wheel (vertical as buttons 4 and 5,
    horizontal as 6 and 7) go through libghostty-vt's mouse encoder,
    which the terminal's modes also give its format — X10 bytes unless
    the program picked SGR (1006), UTF-8, urxvt or SGR-pixels — and which
    drops what the tracking mode does not report. A reported press
    selects nothing and ends any click sequence. A press decides for its
    whole drag, so a reported press always gets its release and a local
    one never does. Each report is a `DataOut` chunk of its own, so macro
    recording skips it (doc 12 §Recording).
  - **Arrow keys** for the vertical wheel on the alternate screen (1047
    or 1049) under alternate scroll (mode 1007, on until the program
    resets it), when no tracking mode is set: Up or Down per row, in the
    cursor-key mode's form.
  - **Local** otherwise: a primary-button drag selects, a middle click
    pastes PRIMARY, and the vertical wheel scrolls the viewport. A right
    click does nothing, and the horizontal wheel is left to the widgets
    above the view.

  Selection is libghostty-vt's gesture: it counts clicks by the desktop's
  double-click time and a cell's width of movement, and picks what the
  press takes and what a drag extends by. A double click takes the word
  under it and a triple click the logical line, wrapped rows included,
  each then dragging by word or line. A selection reaches PRIMARY when
  the button is released, and Select All at once; a click that selects
  nothing leaves PRIMARY alone. A middle click pastes like the Paste
  action, bracketed when the program enabled mode 2004.

  Holding Shift keeps a press, its drag, a middle click and the wheel
  local even while the program tracks the mouse. The wheel counts three rows per notch,
  adding up the fractional steps of high-resolution wheels and touchpads
  (pixel deltas count by cell height); a reported or arrow-key wheel
  sends one report or key per row. The horizontal wheel counts columns
  the same way, on its own carry and by cell width, one report per
  column. With mode 1004 set, keyboard focus
  entering and leaving the widget sends `CSI I` and `CSI O`.
- **Search.** libghostty-vt's search covers the active screen and its
  scrollback, and the shim builds VTE's behavior on it: case-sensitive,
  with wrap-around. A new query starts from the viewport, forward from
  its top and backward from its bottom; the same query again moves on
  from the current match. libghostty-vt folds ASCII case, so each
  candidate is read back and compared with the query as typed. The
  current match is held in tracked grid refs, which follow it through
  output, scrolling and reflow, rather than in the search's own selected
  match, which moves one slow call per match. A match off screen is
  scrolled to the middle of the viewport.

  Every visible match is lit in Ghostty's colors, the current one in its
  own (`search_marks` in `view.rs`), with the search caught up on each
  frame. It only colors cells: the terminal's selection is untouched and
  draws on top, as does the cursor. An empty query, a press, a typed key
  or a reset ends the search; the UI never sends an empty query, so the
  press and the key are how a highlight goes away.
- **Links.** A program marks text as a link with OSC 8. The snapshot
  lists the URIs of the links on screen, and each linked cell points at
  its own entry. The C API has no link id, so the URI identifies a link:
  linked cells in reading order share an entry for as long as the URI
  stays the same, with or without unlinked cells between them, so a link
  a program broke over two rows stays one link. A URI that is not UTF-8
  is no link. When the program marks nothing, `scan_links` finds URLs in
  the text of the last frame, with no help from it: it reads a logical
  line, the rows joined by their soft-wrap flag, so a wrapped link is
  one link and a line that only fills its last column does not run into
  the next. False positives must not open anything, so text is a link
  only if:
  - it starts with `https://`, `http://` or `mailto:`, in any case, with
    no scheme character (letter, digit, `+`, `-`, `.`) right before it;
  - it runs over ASCII URL characters: anything else ends it, including
    an empty cell, a cell holding a longer cluster, text hidden with SGR 8,
    and a closing bracket that closes nothing in the link, so
    `(https://a.b/c)` stops before the `)` and `https://a.b/c_(d)` keeps it;
  - trailing `.` `,` `;` `:` `!` `?` `'` `*` `(` `[` are left out;
  - what remains has a target: a host name or bracketed IPv6 address, with
    an optional user and numeric port, or for `mailto:` an address.
  An OSC 8 link's URI is the program's word: it is not put through
  these rules, only through `may_open`.

  While mouse events are local (`route`), the link under the pointer shows
  its URI in a tooltip, is underlined on all its cells, and turns the
  cursor to `pointer`. Moving off it or leaving the widget clears all
  three, and so does output or scrolling that takes the link away: each
  frame checks again. Links are read from the last frame drawn, so what
  opens is what is on screen. Ctrl+click opens the link through
  `gtk::UriLauncher`, on release, without waiting for the handler; a press
  that turned into a selection, or a release off the link, opens nothing.
  While a program tracks the mouse the press is the program's, so Shift
  brings links back: hover with Shift, Ctrl+Shift+click. A failure to open
  is logged without the URI, which can carry a token.

  `may_open` in `view.rs` decides what reaches the handler. A link comes
  from whatever is on the far side of the tab, and the view does not know
  the transport, so one rule covers every tab: a `file:` URI, in any case,
  opens only if its authority is exactly this machine's name
  (`glib::host_name`). Any other authority is refused, and so are an empty
  one (`file:///path`), `localhost`, a user or a port in it, and the forms
  with none (`file:/path`). A refused link still shows its tooltip and
  underline, so the target can be read. Text with no URI scheme is refused;
  every other scheme goes to the handler.

- **Program clipboard.** An OSC 52 write sets a GTK clipboard, so a remote
  Neovim yank or tmux copy reaches the desktop: `p` and `s` go to PRIMARY,
  `c` and any other selection letter to the system clipboard. OSC 1337
  Copy takes the same path; libghostty-vt reports both through one
  callback. A write lands whole or not at all:
  - libghostty-vt decodes the base64 and drops a payload with any character
    outside its alphabet, whitespace included; missing padding is fine, and
    a control byte inside the sequence never reaches the decoder. The shim
    passes that note to stderr, the only part of libghostty-vt's log
    Wayterm shows.
  - libghostty-vt drops a sequence over 8 MiB, about 6 MiB of text, without
    a note. The engine also refuses over 8 MiB decoded, a backstop OSC 52
    cannot reach while the parser's limit stands.
  - The engine drops decoded bytes that are not UTF-8, or that hold a NUL,
    which would cut GDK's text short. Notes never carry the contents.

  Clipboard contents never leave the machine: the shim installs no
  `clipboard_read` callback, so an OSC 52 query (`?`) gets no reply at all.
  The kitty clipboard protocol stays off. The write callback would open
  OSC 5522 writes, so the shim sets their limit to zero: a write with data
  is answered `EFBIG`, one without `ENOSYS`, and neither reaches a
  clipboard. Wayterm's own Copy and Paste are unrelated.
- **Backend.** Events go through the same never-blocking sender as VTE's
  (§Backpressure). `send_input` and `paste` queue `DataOut` like a key
  press, so macros work unchanged; `paste` is bracketed when the program
  enabled mode 2004. `clear` and `reset` both fully reset, as VTE's
  `clear` does. `set_size` resizes the grid and makes it the widget's
  natural size. `set_opacity` paints the frame's background rectangle
  with that alpha; colored cell runs stay opaque (§Opacity). The view's
  `connect_input`, `connect_output` and `connect_directory` hooks stand
  in for VTE's `commit`, `contents-changed` and
  `current-directory-uri-changed` signals, which the SFTP sidebar
  follows the shell by (doc 07 §Following the shell); the directory
  hook also feeds the tab's kept directory (§Reported directory).
- **Color schemes.** `set_color_scheme` looks the name up as VTE does
  (§Color schemes) and makes the palette's foreground, background, cursor
  and 16 ANSI colors libghostty-vt's default colors. The 256-color cube
  and grayscale ramp stay xterm's, as in VTE, and colors a program set
  (OSC 4, 10, 11, 12) stay in force. Every cell resolves its colors at
  the next snapshot, so an open tab repaints at once. An unknown name
  changes nothing, and a reset keeps the scheme. Until the first call a
  terminal has libghostty-vt's built-in colors.

### Kitty graphics

libghostty-vt parses and stores the images; the view draws them, driven
by the render state each frame like the cells, so an image scrolls with
its text, follows the viewport through the scrollback and goes when it is
deleted, with nothing to track on Wayterm's side.

- **Placements.** Each snapshot reads the render state's image list:
  images placed at a cell, placed relative to another image, and shown
  through Unicode placeholder cells. It gives each one's cell, pixel offset
  and size, source rectangle, layer and z. Placeholder cells draw no glyph.
  A list needs the terminal's cell size, which `fit_grid` gives it, so
  nothing shows before the first allocation.
- **Layers.** Images below backgrounds go over the frame's default
  background, then the cell backgrounds, then images below text, the text,
  and images above it. Within a layer they go by z, then by image ID, as
  in Ghostty's renderer.
- **Pixels.** Stored images are always decoded, so a texture is an upload
  of RGB, RGBA, gray or gray-and-alpha bytes. The view holds the textures of
  the last frame only, keyed by image ID and pixel generation: pixels are
  read once per generation, and what a frame does not draw is released.
  Each image is drawn whole, scaled so that its source rectangle lands on
  the target, and clipped to the target and the grid.
- **PNG.** libghostty-vt leaves PNG decoding to the embedder, through one
  process-wide callback set before the first terminal. `gdk::Texture`
  decodes in `Engine`, which still needs no display. The payload is the
  remote side's, so the header is checked first: only PNG reaches GDK, and
  only if its pixels could fit the storage limit.
- **Limits.** Each screen stores up to 320 MB of images, Ghostty's default;
  libghostty-vt's own is 10 MB. The oldest unused images go first. Only
  images sent in the stream are accepted: the file, temporary-file and
  shared-memory media stay off, since they would let a remote program name
  local paths.
- **Animation.** Each paint ticks libghostty-vt's animations before the
  snapshot. A new frame changes the image's generation, so it uploads like
  any new pixels. A one-shot timeout asks for a redraw when the next frame
  is due; a tab that is not shown draws nothing, so its animations wait.

### libghostty-vt pin

libghostty-vt has no tagged release and its C API still moves, so the pin
changes only in a deliberate bump: rebuild, rerun the checks below, and
update this section, the comment in `build.rs` and the `ghostty` job's
`env` in `.github/workflows/ci.yml` together; that job fails while its
commit differs from the other two.

| | |
|---|---|
| Upstream | <https://github.com/ghostty-org/ghostty> |
| Commit | `9ee02ae601d3eec567723287e1c20bd0e9f1489c` (2026-10-26) |
| Zig | 0.17.0 (`minimum_zig_version` in upstream `build.zig.zon`) |
| Library version | `0.1.0-dev` (upstream pkg-config) |

Build it outside the Wayterm checkout:

```sh
# Zig 0.17.0, user-local (sha256 from ziglang.org/download/index.json:
# 1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026)
curl -LO https://ziglang.org/download/0.17.0/zig-x86_64-linux-0.17.0.tar.xz
tar -xJf zig-x86_64-linux-0.17.0.tar.xz -C ~/.local/zig
ln -sf ~/.local/zig/zig-x86_64-linux-0.17.0/zig ~/.local/bin/zig

git clone https://github.com/ghostty-org/ghostty.git ghostty-build
cd ghostty-build
git checkout --detach 9ee02ae601d3eec567723287e1c20bd0e9f1489c
zig build -Demit-lib-vt -Doptimize=fast --prefix "$PWD/out"
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
under xvfb, and the benchmark. CI runs all but the benchmark, building
the library the same way and caching it per pin (doc 15 §CI).

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

Pinned commit `9ee02ae6`, release build, AMD Ryzen 5 PRO 4650U on AC
power, VTE 0.84.1, two runs agreeing within 10%:

| Corpus | libghostty-vt MiB/s | VTE MiB/s | Ratio |
|---|---|---|---|
| plain text lines | 678 | 101 | 6.7× |
| SGR colors (16, 256, truecolor) | 87 | 50 | 1.8× |
| cursor-addressed redraws (`top`-like) | 265 | 125 | 2.1× |
| multi-byte UTF-8 (CJK, accents, emoji) | 235 | 54 | 4.4× |

## Limitations

- **No per-session font or opacity control.** Tabs honor the session
  columns (§Font, opacity and scrollback), but the session dialog does not
  edit them (doc 09 §Limitations); Settings edits the global rows (doc 13
  §Customize).
- **Opacity needs a compositor.** Every Wayland session has one; on X11
  without a compositing manager the cleared areas draw dark instead of
  showing the desktop.
- **No per-session color scheme picker.** The session dialog has none; an
  override already in the store (imported, restored) is honored.
- **The kept directory has no consumer** (§Reported directory), and only
  OSC 7 fills it: a shell that reports nothing, as many do over SSH (doc
  07 §Following the shell), leaves it empty, and probes do not update it.
- **VTE:** no Kitty graphics. Search steps between rows, reaching only
- **VTE:** no Kitty graphics. Search steps between rows, reaching only
  the first match on each. `features()` reports URL detection, but no
  match is registered with the widget, so links are not lit or opened.
- **Ghostty:**
- **Ghostty:** no SIXEL (`features()` says so); no input method or
  input method or compose support; no local use of a right click or the
  horizontal wheel, which only reach a program tracking the mouse; a
  second button pressed during a drag resets GTK's gesture for the
  first, so that button's release is reported then, and the second's
  when any other button changes, each press still getting one release;
  the text under the cursor takes its cell's background color, since
  libghostty-vt has no cursor text color (the scheme's matches the
  default background in both palettes, so only a cell a program colored
  differs from VTE).
- **Ghostty images:** programs cannot learn the cell's size in pixels,
  so tools that size images from it may refuse or misjudge them: `CSI
  14 t`, `16 t` and `18 t` go unanswered, and a PTY or SSH window size
  carries no pixels (`SizeChange` has none). Only mode 2048 reports
  them. Images are sent in the stream or not at all (§Kitty graphics),
  and a PNG decodes on the main thread, so a large one stalls the tab's
  window while it does. On a scaled display an image is drawn at
  logical pixels and upscaled. Selecting across placeholder cells
  copies their U+10EEEE characters.
- **Ghostty cursor:** the blink period is fixed and GTK's cursor blink
  settings are not read, where VTE follows them. Typing does not restart
  a blink, so the cursor can be in its off phase as it moves. On a wide
  character it covers one cell.
- **Ghostty selection:** word boundaries are libghostty-vt's defaults,
  where `:` ends a word, so a double click takes part of a URL. A drag
  past the top or bottom edge does not scroll the viewport.
- **Ghostty hyperlinks:** the `file:` check compares names and nothing
  more. A remote machine with this machine's name, or a program that
  learned it, gets its link opened here, where the path means a local
  file; so does `file://localhost/` on a machine named `localhost`. The
  comparison is exact, so a fully qualified or differently cased name is
  refused. The view cannot tell a local tab from a remote one, so a local
  program's `file:///path` link is refused too. Schemes other than `file:`
  are not filtered. A link is marked only on hover. Without a link id,
  OSC 8's `id=` is not honored: parts of a link with another link between
  them light apart, and neighbors with one URI light as one. URIs are read
  cell by cell through the grid reference API, which upstream does not
  mean for render loops: a 200×50 screen of links adds about 0.7 ms a
  frame.
- **Ghostty URL detection:** no OSC 8 hyperlinks, and no tooltip
  showing the target. A URL with non-ASCII characters ends at the first
  one, and a host with an underscore is no link. A link cut off by the
  viewport is not detected: scrolled so its tail is below the bottom edge
  it would open the wrong address, and with its scheme above the top edge
  nothing marks it as a link. A row broken by the program instead of
  soft-wrapped ends a link, so one split by a pager or `tmux` pane is only
  its first row.
- **Ghostty search:** a query is plain text, not a regular expression as
  on VTE, and matching is case-exact (libghostty-vt folds ASCII case, so
  a match is read back and compared as typed). A match is not the
  selection, so Copy after a search copies nothing new, where VTE copies
  the match.
- **Ghostty clipboard:** a program writes without asking: there is no
  prompt and no setting to turn it off. An OSC 52 clear (an empty payload)
  is ignored, since it looks the same as an OSC 5522 one. Only text is
  taken, and queries go unanswered, so a program waiting on one times out.
- **Ghostty scrollback:** `set_scrollback_limit` drops libghostty-vt's
  byte limit, so lines alone bound it, but it prunes whole pages: what is
  kept lands within a page of the limit, often just under it (a page is
  387 rows at 120 columns, and a limit below one page keeps part of a
  page). Only 0 is exact.
- **Ghostty underline styles and underline color:** a cell's underline
  color stays as the program set it under the selection, the cursor and
  a search match, so a line can match the color behind it. A font with
  little room under its baseline puts the second line of a double
  underline or the bottom of a wave in the next row, whose background
  covers it.
- **Ghostty text:** box-drawing and block characters come from the font,
  so a line of them shows faint seams wherever the cell is wider than
  the glyph, as it is at the default font. The cell is measured from
  "M" alone, so a proportional font, or any glyph wider than "M",
  overlaps the next cell. Ligatures are drawn as the font shapes them,
  at their first cell, and nothing turns them off. Rapid blink (SGR 6)
  blinks at the slow rate, since libghostty-vt keeps one flag for both.
