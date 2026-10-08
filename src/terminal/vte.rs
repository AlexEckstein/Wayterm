use super::backend::{TerminalBackend, TerminalEvent, TerminalFeatures};
use gtk4::glib;
use gtk4::Widget;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use vte4::prelude::*;
use vte4::Terminal as VteTerminal;

/// Built-in terminal backend backed by the GTK4 VTE widget.
///
/// Implements the [`TerminalBackend`] trait from doc 02. User keystrokes and
/// resize events are forwarded via an async channel without ever blocking the
/// GTK main thread: the fast path uses [`mpsc::Sender::try_send`]; if the
/// channel is full, the bytes/payload are parked in a [`RefCell<VecDeque>`]
/// and flushed from a `glib::idle_add_local` callback. See doc 02
/// §Backpressure for the normative shape.
pub struct VteBackend {
    terminal: VteTerminal,
    // The sender is retained only to satisfy the "owned by the backend"
    // contract; the wired signal closures each keep their own clone.
    #[allow(dead_code)]
    event_tx: mpsc::Sender<TerminalEvent>,
}

// VteBackend is intentionally not Send: the vte4::Terminal widget must stay on
// the GTK main thread. The TerminalBackend trait no longer requires Send so
// `Arc<dyn TerminalBackend>` can hold the widget without unsafe impls.

impl VteBackend {
    /// Construct a new VTE-backed terminal and wire GTK signals.
    ///
    /// Must be called from the GTK main thread. `event_tx` is the sender half
    /// of the bounded channel consumed by the transport pump task.
    pub fn new(event_tx: mpsc::Sender<TerminalEvent>) -> Self {
        let terminal = VteTerminal::new();

        wire_commit(&terminal, event_tx.clone());
        wire_resize(&terminal, event_tx.clone());

        Self { terminal, event_tx }
    }
}

/// Wire VTE's `commit` signal to the event channel using the try_send +
/// idle-flush backpressure pattern from doc 02.
fn wire_commit(terminal: &VteTerminal, tx: mpsc::Sender<TerminalEvent>) {
    let pending: Rc<RefCell<VecDeque<Vec<u8>>>> = Rc::new(RefCell::new(VecDeque::new()));
    let idle_scheduled = Rc::new(Cell::new(false));

    terminal.connect_commit(move |_term, data: &str, _size: u32| {
        let event = TerminalEvent::DataOut(data.as_bytes().to_vec());
        if let Err(err) = tx.try_send(event) {
            let chunk = match err {
                TrySendError::Full(TerminalEvent::DataOut(c)) => c,
                // commit only emits DataOut; a Closed channel is silently dropped.
                TrySendError::Closed(_) | TrySendError::Full(_) => return,
            };
            pending.borrow_mut().push_back(chunk);
            if !idle_scheduled.get() {
                idle_scheduled.set(true);
                let pending2 = pending.clone();
                let tx2 = tx.clone();
                let idle_scheduled2 = idle_scheduled.clone();
                glib::idle_add_local(move || {
                    let mut q = pending2.borrow_mut();
                    while let Some(chunk) = q.pop_front() {
                        match tx2.try_send(TerminalEvent::DataOut(chunk)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(TerminalEvent::DataOut(chunk))) => {
                                q.push_front(chunk);
                                return glib::ControlFlow::Continue;
                            }
                            Err(TrySendError::Closed(_)) => {
                                q.clear();
                                return glib::ControlFlow::Break;
                            }
                            Err(TrySendError::Full(_)) => {
                                unreachable!("commit flush only emits DataOut events")
                            }
                        }
                    }
                    idle_scheduled2.set(false);
                    glib::ControlFlow::Continue
                });
            }
        }
    });
}

/// Wire VTE's `resize-window` signal to the event channel using the same
/// try_send + idle-flush backpressure pattern.
fn wire_resize(terminal: &VteTerminal, tx: mpsc::Sender<TerminalEvent>) {
    let pending: Rc<RefCell<VecDeque<TerminalEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
    let idle_scheduled = Rc::new(Cell::new(false));

    // vte4 0.10 names the signal `resize-window`; the binding method is
    // `connect_resize_window` and the parameters are `(width, height)` in
    // terminal cells.
    terminal.connect_resize_window(move |_term, width: u32, height: u32| {
        let event = TerminalEvent::SizeChange {
            rows: height,
            cols: width,
        };
        if let Err(err) = tx.try_send(event) {
            let ev = match err {
                TrySendError::Full(TerminalEvent::SizeChange { rows, cols }) => {
                    TerminalEvent::SizeChange { rows, cols }
                }
                TrySendError::Closed(_) => return,
                // resize only emits SizeChange.
                TrySendError::Full(_) => return,
            };
            pending.borrow_mut().push_back(ev);
            if !idle_scheduled.get() {
                idle_scheduled.set(true);
                let pending2 = pending.clone();
                let tx2 = tx.clone();
                let idle_scheduled2 = idle_scheduled.clone();
                glib::idle_add_local(move || {
                    let mut q = pending2.borrow_mut();
                    while let Some(ev) = q.pop_front() {
                        match tx2.try_send(ev) {
                            Ok(()) => {}
                            Err(TrySendError::Full(TerminalEvent::SizeChange { rows, cols })) => {
                                q.push_front(TerminalEvent::SizeChange { rows, cols });
                                return glib::ControlFlow::Continue;
                            }
                            Err(TrySendError::Closed(_)) => {
                                q.clear();
                                return glib::ControlFlow::Break;
                            }
                            Err(TrySendError::Full(_)) => {
                                unreachable!("resize flush only emits SizeChange events")
                            }
                        }
                    }
                    idle_scheduled2.set(false);
                    glib::ControlFlow::Continue
                });
            }
        }
    });
}

// Interaction + scrollback methods below all drive the live widget, so they
// are only verifiable under a display: the xvfb path is the ignored
// `vte_scrollback_roundtrip` test in `tests/terminal_polish.rs` (see
// `scrollback_text`). Display-free routing of the actions that reach them is
// covered by the FakeBackend tests in the same file.
impl TerminalBackend for VteBackend {
    fn widget(&self) -> Widget {
        self.terminal.clone().upcast::<Widget>()
    }

    fn write(&self, data: &[u8]) {
        self.terminal.feed(data);
    }

    fn set_size(&self, rows: u32, cols: u32) {
        // vte4 0.10's `set_size` takes `(columns, rows)` as `libc::c_long`.
        self.terminal.set_size(cols as i64, rows as i64);
    }

    fn features(&self) -> TerminalFeatures {
        build_features()
    }

    fn set_color_scheme(&self, _name: &str) {
        // Loading/applying palettes is intentionally out of scope for P2-2.
        // This is a documented stub pending the color-scheme store (doc 03).
    }

    fn set_font(&self, font_desc: &pango::FontDescription) {
        self.terminal.set_font(Some(font_desc));
    }

    fn set_opacity(&self, opacity: f64) {
        // vte4 0.10 does not expose `set_opacity` on the GTK4 port; the
        // background color alpha path lives in the color-scheme work later.
        // Clamp and store the value would be a hidden no-op, so simply leave
        // this as a documented stub.
        let _ = opacity;
    }

    fn set_scrollback_limit(&self, lines: u32) {
        // vte4 0.10 `set_scrollback_lines` takes `libc::c_long`.
        self.terminal.set_scrollback_lines(lines as i64);
    }

    fn selected_text(&self) -> Option<String> {
        if self.terminal.has_selection() {
            // vte4 0.10 implements this as `TerminalExt::text_selected`
            // (requires the `v0_70` feature, which is enabled by vte4's
            // default binding set). It is gated behind the trait.
            self.terminal
                .text_selected(vte4::Format::Text)
                .as_ref()
                .map(|s| s.to_string())
        } else {
            None
        }
    }

    fn copy_selection(&self) {
        if self.terminal.has_selection() {
            // vte4 0.10 uses `copy_clipboard_format` with `Format::Text`.
            self.terminal.copy_clipboard_format(vte4::Format::Text);
        }
    }

    fn paste(&self, text: &str) {
        // vte4 0.10 binding name is `paste_text` (doc 02 guessed correctly).
        self.terminal.paste_text(text);
    }

    fn select_all(&self) {
        self.terminal.select_all();
    }

    fn clear(&self) {
        self.terminal.reset(true, true);
    }

    fn search(&self, text: &str, reverse: bool) {
        // vte4 0.10 exposes `search_set_regex(&Regex, flags)` — the GTK3-era
        // `search_set_gregex` does not exist in the GTK4 binding. Regexes are
        // PCRE2 via `vte4::Regex::for_match`; flags 0 = case-sensitive,
        // unanchored (doc 02 leaves match flags to the implementation).
        if text.is_empty() {
            // Empty query clears the highlight instead of keeping the last
            // match selected.
            self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
            return;
        }
        let Ok(regex) = vte4::Regex::for_match(text, 0) else {
            // Invalid regex (e.g. a dangling '('): clear any previous match
            // rather than stepping with a stale one.
            self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
            return;
        };
        // Configure wrap-around *before* stepping so the very first
        // forward/backward step already wraps.
        self.terminal.search_set_wrap_around(true);
        self.terminal.search_set_regex(Some(&regex), 0);
        if reverse {
            self.terminal.search_find_previous();
        } else {
            self.terminal.search_find_next();
        }
    }

    fn scrollback_text(&self) -> Option<String> {
        // vte4 0.10 leaves `text_range` commented out (the callback argument
        // has no binding conversion); the generated replacement is
        // `text_range_format` (VTE >= 0.72, crate feature `v0_72`).
        //
        // Row 0 is the oldest line of the buffer (verified under xvfb) and
        // `row_count` counts *screen* rows only, so the range end must span
        // the scrollback ring limit + screen rows; VTE clamps past the last
        // line that exists. `scrollback_lines()` is the configured limit
        // (negative = unlimited) — for unlimited rings fall back to a large
        // fixed window (the ring is scanned, not the number).
        //
        // Widget-dependent: not reachable from display-free tests. The
        // xvfb verification path is the `vte_scrollback_roundtrip` test in
        // `tests/terminal_polish.rs`, run via:
        //   xvfb-run -a cargo test --test terminal_polish -- --ignored
        let rows = self.terminal.row_count();
        let cols = self.terminal.column_count();
        if rows <= 0 || cols <= 0 {
            return None;
        }
        let scrollback = self.terminal.scrollback_lines();
        let ring = if scrollback >= 0 {
            scrollback
        } else {
            // Unlimited scrollback: any fixed end is a guess; VTE only holds
            // the lines actually written, and clamps the range.
            100_000
        };
        let end_row = ring + rows - 1;
        let (text, _len) =
            self.terminal
                .text_range_format(vte4::Format::Text, 0, 0, end_row, cols - 1);
        text.map(|g| g.to_string())
    }

    fn scroll_to_top(&self) {
        // VTE4 implements scrolling via its `Scrollable` vadjustment.
        if let Some(adj) = self.terminal.vadjustment() {
            adj.set_value(adj.lower());
        }
    }

    fn scroll_to_bottom(&self) {
        if let Some(adj) = self.terminal.vadjustment() {
            let bottom = (adj.upper() - adj.page_size()).max(adj.lower());
            adj.set_value(bottom);
        }
    }

    fn reset(&self) {
        self.terminal.reset(true, true);
    }

    fn name(&self) -> &str {
        "vte"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &str {
        "VTE terminal widget (GTK4)"
    }
}

/// Feature mapping extracted for display-free unit testing.
///
/// Doc 02 required SIXEL detection to be a runtime check, never hardcoded.
/// The vte4 0.10 crate does not expose `check_version`; the installed VTE
/// version is queried via `vte4::ffi` (pkg-config tells us runtime version).
fn build_features() -> TerminalFeatures {
    // Runtime SIXEL capability check: VTE >= 0.62. The vte4-sys crate exposes
    // the C getters, not a helper, so compute the predicate directly.
    // SAFETY: these are pure getters returning the compile-time/runtime
    // version of the loaded VTE library.
    let sixel_graphics = unsafe {
        let major = vte4::ffi::vte_get_major_version();
        let minor = vte4::ffi::vte_get_minor_version();
        (major, minor) >= (0, 62)
    };

    TerminalFeatures {
        search: true,
        color_schemes: true,
        scrollback_search: true,
        url_detection: true,
        sixel_graphics,
        kitty_graphics: false, // no Kitty graphics protocol in VTE
        custom_fonts: true,
        reflow: true,
        alternate_screen: true,
        scrollback_config: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use tokio::sync::mpsc::error::TrySendError;

    /// The features mapping must always report VTE-parity defaults.
    /// SIXEL depends on the installed library; the other flags are fixed.
    #[test]
    fn vte_feature_mapping_matches_contract() {
        let f = build_features();
        assert!(f.search);
        assert!(f.color_schemes);
        assert!(f.scrollback_search);
        assert!(f.url_detection);
        assert!(!f.kitty_graphics);
        assert!(f.custom_fonts);
        assert!(f.reflow);
        assert!(f.alternate_screen);
        assert!(f.scrollback_config);
    }

    /// Display-free model of the try_send + idle-flush backpressure path.
    /// Simulates a bounded channel of capacity zero so the first send parks
    /// the chunk; a second attempt after space is made drains the queue.
    #[tokio::test]
    async fn idle_flush_mechanic_drains_parked_events() {
        let (tx, mut rx) = mpsc::channel::<TerminalEvent>(1);
        let pending: Rc<RefCell<VecDeque<Vec<u8>>>> = Rc::new(RefCell::new(VecDeque::new()));
        let idle_scheduled = Rc::new(Cell::new(false));

        // Fill the channel so the next send takes the slow path.
        tx.try_send(TerminalEvent::DataOut(vec![0x00]))
            .expect("precondition: fill channel");

        let chunk = vec![0x41, 0x42];
        if let Err(err) = tx.try_send(TerminalEvent::DataOut(chunk.clone())) {
            let parked = match err {
                TrySendError::Full(TerminalEvent::DataOut(c)) => c,
                other => panic!("expected Full(DataOut), got {:?}", other),
            };
            pending.borrow_mut().push_back(parked);
            idle_scheduled.set(true);
        } else {
            panic!("expected channel to be full");
        }

        // Simulate the idle flush: drain the queue by re-sending once capacity
        // is available. In the real widget this happens inside
        // glib::idle_add_local; here we exercise the same state machine.
        assert!(idle_scheduled.get());

        // Make room in the channel.
        let _ = rx.recv().await;

        // Flush exactly like the idle callback would.
        let flushed = {
            let mut q = pending.borrow_mut();
            while let Some(chunk) = q.pop_front() {
                match tx.try_send(TerminalEvent::DataOut(chunk)) {
                    Ok(()) => {}
                    Err(TrySendError::Full(c)) => {
                        if let TerminalEvent::DataOut(v) = c {
                            q.push_front(v);
                            break;
                        }
                        panic!("unexpected event variant returned");
                    }
                    Err(TrySendError::Closed(_)) => {
                        q.clear();
                        break;
                    }
                }
            }
            q.is_empty()
        };

        assert!(flushed);
        idle_scheduled.set(false);

        let received = rx.recv().await.expect("parked event should arrive");
        assert_eq!(received, TerminalEvent::DataOut(vec![0x41, 0x42]));
    }
}
