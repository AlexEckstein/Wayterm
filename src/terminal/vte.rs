use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use gtk4::{pango, Widget};
use tokio::sync::mpsc;
use vte4::prelude::*;
use vte4::Terminal as VteTerminal;

use super::backend::{TerminalBackend, TerminalEvent, TerminalFeatures};
use crate::backpressure::PendingQueue;

/// The built-in backend: the GTK4 VTE widget (doc 02). Main thread only.
pub struct VteBackend {
    terminal: VteTerminal,
    /// The queue `commit` feeds, for input `feed_child` cannot carry.
    input: EventSender,
}

impl VteBackend {
    /// Build the widget and wire its signals to `event_tx`, the bounded
    /// channel the transport pump consumes.
    pub fn new(event_tx: mpsc::Sender<TerminalEvent>) -> Self {
        let terminal = VteTerminal::new();
        let sender = EventSender::new(event_tx);

        let input = sender.clone();
        let commit = sender.clone();
        terminal.connect_commit(move |_, data, _| {
            commit.send(TerminalEvent::DataOut(data.as_bytes().to_vec()));
        });

        // `resize-window` reports child-initiated resizes as (cols, rows).
        let resize = sender.clone();
        terminal.connect_resize_window(move |_, cols, rows| {
            resize.send(TerminalEvent::SizeChange { rows, cols });
        });

        // Widget allocation changes (pane drags, window resizes) only show up
        // as new row/column counts on `contents-changed`, which also fires on
        // every byte of output — so only a changed grid is sent.
        let last = Cell::new(grid_from_counts(
            terminal.row_count(),
            terminal.column_count(),
        ));
        terminal.connect_contents_changed(move |term| {
            let grid = grid_from_counts(term.row_count(), term.column_count());
            if last.replace(grid) != grid {
                sender.send(TerminalEvent::SizeChange {
                    rows: grid.0,
                    cols: grid.1,
                });
            }
        });

        Self { terminal, input }
    }
}

/// Sends terminal events without ever blocking the GTK main thread
/// (doc 02 §Backpressure): events the channel refuses are parked and
/// flushed, in order, from an idle callback. Nothing is dropped unless the
/// pump is gone. Shared with the other GTK backends.
#[derive(Clone)]
pub(super) struct EventSender {
    tx: mpsc::Sender<TerminalEvent>,
    pending: Rc<RefCell<PendingQueue<TerminalEvent>>>,
    flush_scheduled: Rc<Cell<bool>>,
}

impl EventSender {
    pub(super) fn new(tx: mpsc::Sender<TerminalEvent>) -> Self {
        Self {
            tx,
            pending: Rc::default(),
            flush_scheduled: Rc::default(),
        }
    }

    pub(super) fn send(&self, event: TerminalEvent) {
        // Queue behind anything already parked so events stay in order.
        self.pending.borrow_mut().push(event);
        if !self.flush() && !self.flush_scheduled.replace(true) {
            let this = self.clone();
            glib::idle_add_local(move || {
                if this.flush() {
                    this.flush_scheduled.set(false);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }
    }

    /// Offer parked events to the channel; `true` once nothing is left.
    fn flush(&self) -> bool {
        let report = self
            .pending
            .borrow_mut()
            .drain_with(|event| self.tx.try_send(event));
        report.remaining == 0
    }
}

/// VTE reports signed cell counts; never emit a zero or negative grid.
fn grid_from_counts(rows: i64, cols: i64) -> (u32, u32) {
    (
        rows.clamp(1, u32::MAX.into()) as u32,
        cols.clamp(1, u32::MAX.into()) as u32,
    )
}

impl TerminalBackend for VteBackend {
    fn widget(&self) -> Widget {
        self.terminal.clone().upcast()
    }

    fn write(&self, data: &[u8]) {
        self.terminal.feed(data);
    }

    /// `feed_child` emits `commit`, so input takes the keystroke path,
    /// `commit` listeners included. The `commit` binding reads its text
    /// as a NUL-terminated UTF-8 string, so anything else (`<0xFF>`,
    /// `<Ctrl-@>`) joins the same queue directly instead — still in order.
    fn send_input(&self, data: &[u8]) {
        if std::str::from_utf8(data).is_ok() && !data.contains(&0) {
            self.terminal.feed_child(data);
        } else {
            self.input.send(TerminalEvent::DataOut(data.to_vec()));
        }
    }

    fn set_size(&self, rows: u32, cols: u32) {
        self.terminal.set_size(cols.into(), rows.into());
    }

    fn features(&self) -> TerminalFeatures {
        // SAFETY: plain getters for the loaded library's version.
        let (major, minor) = unsafe {
            (
                vte4::ffi::vte_get_major_version(),
                vte4::ffi::vte_get_minor_version(),
            )
        };
        TerminalFeatures {
            search: true,
            color_schemes: true,
            scrollback_search: true,
            url_detection: true,
            // SIXEL landed in VTE 0.62 — a runtime check, never a hardcode.
            sixel_graphics: (major, minor) >= (0, 62),
            kitty_graphics: false,
            custom_fonts: true,
            reflow: true,
            alternate_screen: true,
            scrollback_config: true,
        }
    }

    /// Not implemented yet: needs the color-scheme store.
    fn set_color_scheme(&self, _name: &str) {}

    fn set_font(&self, font_desc: &pango::FontDescription) {
        self.terminal.set_font(Some(font_desc));
    }

    /// Not implemented yet: the GTK4 VTE binding has no `set_opacity`; it
    /// arrives with background-alpha color schemes.
    fn set_opacity(&self, _opacity: f64) {}

    fn set_scrollback_limit(&self, lines: u32) {
        self.terminal.set_scrollback_lines(lines.into());
    }

    fn selected_text(&self) -> Option<String> {
        self.terminal
            .has_selection()
            .then(|| self.terminal.text_selected(vte4::Format::Text))
            .flatten()
            .map(|text| text.to_string())
    }

    fn copy_selection(&self) {
        if self.terminal.has_selection() {
            self.terminal.copy_clipboard_format(vte4::Format::Text);
        }
    }

    fn paste(&self, text: &str) {
        self.terminal.paste_text(text);
    }

    fn select_all(&self) {
        self.terminal.select_all();
    }

    fn clear(&self) {
        self.terminal.reset(true, true);
    }

    /// Regex search (PCRE2, case-sensitive) with wrap-around. An empty or
    /// invalid query clears the highlight instead of stepping a stale match.
    fn search(&self, text: &str, reverse: bool) {
        let regex = (!text.is_empty())
            .then(|| vte4::Regex::for_match(text, 0).ok())
            .flatten();
        let Some(regex) = regex else {
            self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
            return;
        };
        self.terminal.search_set_wrap_around(true);
        self.terminal.search_set_regex(Some(&regex), 0);
        if reverse {
            self.terminal.search_find_previous();
        } else {
            self.terminal.search_find_next();
        }
    }

    /// The whole buffer, scrollback included (VTE >= 0.72). Row 0 is the
    /// oldest line and `row_count` only counts screen rows, so the range
    /// spans the scrollback limit plus the screen; VTE clamps past the end.
    /// Verified under xvfb by `tests/terminal_polish.rs`.
    fn scrollback_text(&self) -> Option<String> {
        let rows = self.terminal.row_count();
        let cols = self.terminal.column_count();
        if rows <= 0 || cols <= 0 {
            return None;
        }
        // Negative = unlimited; VTE only holds what was written anyway.
        let ring = match self.terminal.scrollback_lines() {
            n if n >= 0 => n,
            _ => 100_000,
        };
        let (text, _) =
            self.terminal
                .text_range_format(vte4::Format::Text, 0, 0, ring + rows - 1, cols - 1);
        text.map(|text| text.to_string())
    }

    fn scroll_to_top(&self) {
        if let Some(adj) = self.terminal.vadjustment() {
            adj.set_value(adj.lower());
        }
    }

    fn scroll_to_bottom(&self) {
        if let Some(adj) = self.terminal.vadjustment() {
            adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_from_counts_clamps_degenerate_values() {
        assert_eq!(grid_from_counts(24, 80), (24, 80));
        assert_eq!(grid_from_counts(0, 0), (1, 1));
        assert_eq!(grid_from_counts(-5, -5), (1, 1));
        assert_eq!(grid_from_counts(i64::MAX, 80), (u32::MAX, 80));
    }

    /// The sender's display-free path: a full channel parks events, and a
    /// later flush delivers them in order once there is room.
    #[test]
    fn a_full_channel_parks_events_until_a_flush_finds_room() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = EventSender::new(tx);
        sender.pending.borrow_mut().push(TerminalEvent::Bell);
        assert!(sender.flush(), "room for one event");
        sender
            .pending
            .borrow_mut()
            .push(TerminalEvent::DataOut(b"a".to_vec()));
        assert!(!sender.flush(), "the channel is full");

        assert_eq!(rx.try_recv().unwrap(), TerminalEvent::Bell);
        assert!(sender.flush());
        assert_eq!(
            rx.try_recv().unwrap(),
            TerminalEvent::DataOut(b"a".to_vec())
        );
    }
}
