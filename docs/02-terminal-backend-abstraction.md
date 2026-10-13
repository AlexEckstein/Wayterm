# Terminal Backend Abstraction

The rest of the app (tabs, hotkeys, SFTP) must not care which terminal
renders a tab. Two kinds of backend fit behind one trait:

- **Widget backends** (VTE) own rendering, scrollback, selection, fonts.
- **Engine backends** (libghostty-vt, alacritty_terminal) only hold
  terminal state; we would draw it in a custom GTK widget.

Either way `widget()` returns the `gtk::Widget` the tab embeds.

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

There is no `Send` bound: widget backends hold GTK objects and live on the
main thread. Bytes come in through `write`; everything going out is a
`TerminalEvent` on a bounded channel. `send_input` is input that was not
typed — macro playback (doc 12) — and leaves as `DataOut` through the
same queue as keystrokes. `create_terminal()` builds the backend
and that channel together and returns a `TerminalHandle { backend, events }`;
the receiver goes to the transport pump (doc 04 §Wiring). Backends are held
as `Arc<dyn TerminalBackend>` because the tab, the output drain, and the
terminal actions share them.

`TerminalFeatures` is one struct (search, color schemes, SIXEL, Kitty
graphics, reflow, …) queried once per tab; the UI hides what is false.
`write` takes bytes, not `&str`: terminal data is not guaranteed UTF-8.

## Backpressure

The VTE `commit` signal (keystrokes, pastes) and the resize signals fire on
the main thread, which must never block. Every backend sends events with
`try_send`; whatever the channel refuses is parked in a `PendingQueue`
(`src/backpressure.rs`) and offered again from an idle callback, in order,
until the queue drains — then the idle source removes itself. Nothing is
dropped unless the pump is gone. Keystrokes take the fast path; large
pastes exercise the queue. The SFTP sidebar uses the same queue for its
commands.

Output flows the other way through a bounded channel drained on the main
context, so a stalled UI backs up into the transport and, ultimately, SSH
flow control (doc 04).

## VteBackend

- **Size:** `resize-window` only reports child-initiated resizes; pane and
  window resizes surface as new row/column counts on `contents-changed`,
  which also fires on every byte of output, so only a changed grid is sent.
- **Search** sets a PCRE2 regex with wrap-around; an empty or invalid query
  clears the highlight.
- **SIXEL** is reported from the runtime VTE version (≥ 0.62), never
  hardcoded. VTE has no Kitty graphics.
- **Input:** `send_input` calls `feed_child`, which emits `commit`, so
  injected input reaches every `commit` listener like a keystroke. The
  vte4 `commit` binding reads its text as a NUL-terminated UTF-8 string,
  so bytes it cannot carry (invalid UTF-8, NUL) are queued as `DataOut`
  directly instead, still in order.
- **Not yet implemented:** color schemes and opacity (need the color-scheme
  store).

### Scrollback

`scrollback_text` uses `text_range_format` (VTE ≥ 0.72, crate feature
`v0_72`). Row 0 is the oldest line and `row_count` counts screen rows only,
so the range spans the scrollback limit plus the screen. Verified under
xvfb by `tests/terminal_polish.rs`.

### TitleChanged seam

A tab label carries a title state machine: placeholder (the session name)
→ set (the terminal's window title) → renamed (by the user, by
double-click). Once renamed, terminal titles no longer change it; a blank
terminal title falls back to the session name.

## FakeBackend

An in-memory backend with no widget that records every `write`,
`send_input`, and interaction call, so the pump, the bridge, and the
terminal-action routing are tested without a display.

## Engine backends (future)

A `GhosttyBackend` would pair a custom render widget (drawing ghostty state
via Cairo or GL) with the same trait and the same backpressure pattern. No
trait change is needed; see docs 03 and 05. Its `send_input` queues
`DataOut` the way its own key handler does, so macros (doc 12) work
unchanged.
