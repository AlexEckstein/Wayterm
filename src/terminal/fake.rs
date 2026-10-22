//! [`FakeBackend`]: a display-free backend that records calls for tests
//! (doc 02 §FakeBackend).

use std::sync::{Arc, Mutex, MutexGuard};

use gtk4::{pango, Widget};

use super::backend::{TerminalBackend, TerminalFeatures};

/// One interaction/scrollback call recorded by [`FakeBackend`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    /// `send_input`.
    SendInput(Vec<u8>),
    /// `set_color_scheme`.
    SetColorScheme(String),
    /// `copy_selection`.
    CopySelection,
    /// `paste`.
    Paste(String),
    /// `select_all`.
    SelectAll,
    /// `clear`.
    Clear,
    /// `search`.
    Search { text: String, reverse: bool },
    /// `scroll_to_top`.
    ScrollToTop,
    /// `scroll_to_bottom`.
    ScrollToBottom,
}

/// In-memory backend for display-free tests: no widget, records every
/// `write` and every interaction call. Clones share the recordings.
#[derive(Clone, Default)]
pub struct FakeBackend {
    /// Every `write`, concatenated.
    written: Arc<Mutex<Vec<u8>>>,
    /// Every interaction call, in order.
    interactions: Arc<Mutex<Vec<Interaction>>>,
}

/// Lock, ignoring poison: a panicked test must not hide later records.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeBackend {
    /// An empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// All bytes written so far, in order.
    pub fn written(&self) -> Vec<u8> {
        lock(&self.written).clone()
    }

    /// All interaction calls so far, in order.
    pub fn interactions(&self) -> Vec<Interaction> {
        lock(&self.interactions).clone()
    }

    /// Append one interaction call.
    fn record(&self, interaction: Interaction) {
        lock(&self.interactions).push(interaction);
    }
}

impl TerminalBackend for FakeBackend {
    fn widget(&self) -> Widget {
        panic!("FakeBackend has no GTK widget")
    }

    fn write(&self, data: &[u8]) {
        lock(&self.written).extend_from_slice(data);
    }

    fn send_input(&self, data: &[u8]) {
        self.record(Interaction::SendInput(data.to_vec()));
    }

    fn set_size(&self, _rows: u32, _cols: u32) {}

    /// VTE parity, minus the runtime-detected SIXEL support.
    fn features(&self) -> TerminalFeatures {
        TerminalFeatures {
            search: true,
            color_schemes: true,
            scrollback_search: true,
            url_detection: true,
            sixel_graphics: false,
            kitty_graphics: false,
            custom_fonts: true,
            reflow: true,
            alternate_screen: true,
            scrollback_config: true,
        }
    }

    fn set_color_scheme(&self, name: &str) {
        self.record(Interaction::SetColorScheme(name.to_string()));
    }
    fn set_font(&self, _font_desc: &pango::FontDescription) {}
    fn set_opacity(&self, _opacity: f64) {}
    fn set_scrollback_limit(&self, _lines: u32) {}

    fn selected_text(&self) -> Option<String> {
        None
    }
    fn copy_selection(&self) {
        self.record(Interaction::CopySelection);
    }
    fn paste(&self, text: &str) {
        self.record(Interaction::Paste(text.to_string()));
    }
    fn select_all(&self) {
        self.record(Interaction::SelectAll);
    }
    fn clear(&self) {
        self.record(Interaction::Clear);
    }

    fn search(&self, text: &str, reverse: bool) {
        self.record(Interaction::Search {
            text: text.to_string(),
            reverse,
        });
    }
    fn scrollback_text(&self) -> Option<String> {
        None
    }
    fn scroll_to_top(&self) {
        self.record(Interaction::ScrollToTop);
    }
    fn scroll_to_bottom(&self) {
        self.record(Interaction::ScrollToBottom);
    }

    fn reset(&self) {}

    fn name(&self) -> &str {
        "fake"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "In-memory terminal backend for tests"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_writes_in_order() {
        let backend = FakeBackend::new();
        backend.write(b"a");
        backend.write(b"bc");
        assert_eq!(backend.written(), b"abc");
    }
}
