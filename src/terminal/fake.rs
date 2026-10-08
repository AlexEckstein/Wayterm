use super::backend::{TerminalBackend, TerminalEvent, TerminalFeatures};
use gtk4::Widget;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// One interaction call routed through [`TerminalBackend`]'s doc-02
/// interaction/scrollback section. Recorded by [`FakeBackend`] so tests can
/// assert which action reached the backend — the action-to-terminal routing
/// table (`crate::ui::main_window::route_terminal_action`) is exercised
/// display-free through this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    CopySelection,
    Paste(String),
    SelectAll,
    Clear,
    Search { text: String, reverse: bool },
    ScrollToTop,
    ScrollToBottom,
}

/// In-memory backend for transport tests — no widget, no GTK.
/// Records every [`write`](TerminalBackend::write) call so tests can assert
/// what the remote actually received, plus every interaction call (see
/// [`Interaction`]) so the terminal-action routing can be tested display-free.
#[derive(Clone)]
pub struct FakeBackend {
    written: Arc<Mutex<Vec<u8>>>,
    interactions: Arc<Mutex<Vec<Interaction>>>,
    features_override: Option<TerminalFeatures>,
    // Test-only event hook. The handle returned by [`create_terminal`](super::create_terminal)
    // owns the receiver; this sender lets tests push synthetic events through
    // the backend to verify channel wiring.
    #[doc(hidden)]
    pub event_tx: Option<mpsc::Sender<TerminalEvent>>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            written: Arc::new(Mutex::new(Vec::new())),
            interactions: Arc::new(Mutex::new(Vec::new())),
            features_override: None,
            event_tx: None,
        }
    }

    /// Build a fake backend with a fixed [`TerminalFeatures`] value.
    pub fn with_features(features: TerminalFeatures) -> Self {
        Self {
            written: Arc::new(Mutex::new(Vec::new())),
            interactions: Arc::new(Mutex::new(Vec::new())),
            features_override: Some(features),
            event_tx: None,
        }
    }

    /// All bytes written to this backend, in order.
    pub fn written(&self) -> Vec<u8> {
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// All interaction calls routed to this backend, in order.
    pub fn interactions(&self) -> Vec<Interaction> {
        self.interactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn record(&self, interaction: Interaction) {
        self.interactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(interaction);
    }

    /// Test helper: emit a synthetic event into the channel held by the
    /// corresponding [`TerminalHandle`](super::TerminalHandle).
    #[doc(hidden)]
    pub fn emit(&self, ev: TerminalEvent) {
        if let Some(ref tx) = self.event_tx {
            // Tests run inside the tokio runtime, so try_send is synchronous.
            let _ = tx.try_send(ev);
        }
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalBackend for FakeBackend {
    fn widget(&self) -> Widget {
        // No widget in tests. Use a debug panic message so test failures are
        // clearer than a bare `unreachable!`.
        panic!("FakeBackend has no GTK widget; widget() is only valid for real backends")
    }

    fn write(&self, data: &[u8]) {
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(data);
    }

    fn set_size(&self, _rows: u32, _cols: u32) {}

    fn features(&self) -> TerminalFeatures {
        self.features_override.clone().unwrap_or(TerminalFeatures {
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
        })
    }

    fn set_color_scheme(&self, _name: &str) {}
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
    fn fake_backend_records_writes_in_order() {
        let backend = FakeBackend::new();
        backend.write(b"a");
        backend.write(b"bc");
        assert_eq!(backend.written(), b"abc");
    }

    #[test]
    fn fake_backend_default_features_match_vte_parity() {
        let backend = FakeBackend::new();
        let f = backend.features();
        assert!(f.search);
        assert!(f.color_schemes);
        assert!(f.scrollback_search);
        assert!(f.url_detection);
        assert!(!f.sixel_graphics);
        assert!(!f.kitty_graphics);
        assert!(f.custom_fonts);
        assert!(f.reflow);
        assert!(f.alternate_screen);
        assert!(f.scrollback_config);
    }

    #[test]
    fn fake_backend_features_override_round_trips() {
        let features = TerminalFeatures {
            search: false,
            kitty_graphics: true,
            ..TerminalFeatures::default()
        };
        let backend = FakeBackend::with_features(features.clone());
        assert_eq!(backend.features(), features);
    }
}
