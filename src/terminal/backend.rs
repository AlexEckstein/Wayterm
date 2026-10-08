use gtk4::Widget;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Capabilities a backend supports — used for feature gating in the UI.
// Public API surface for the terminal abstraction. Dead-code lints are
// expected while the trait is defined in P2-1 and first consumed by
// VteBackend in P2-2.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalFeatures {
    pub search: bool,
    pub color_schemes: bool,
    pub scrollback_search: bool,
    pub url_detection: bool,
    pub sixel_graphics: bool,
    pub kitty_graphics: bool,
    pub custom_fonts: bool,
    pub reflow: bool,            // text reflow on resize
    pub alternate_screen: bool,  // alternate screen buffer support
    pub scrollback_config: bool, // backend exposes scrollback limit config
}

/// Data flowing out of the terminal (user keystrokes → transport).
/// Sent via channel, not return value, to keep the backend decoupled
/// from the transport.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
    DataOut(Vec<u8>),
    SizeChange { rows: u32, cols: u32 },
    TitleChanged(String),
    Bell,
    Closed,
}

/// The terminal backend trait. Implementations:
/// - VteBackend (built-in, uses vte4 crate)
/// - GhosttyBackend (future, uses libghostty-vt C API)
/// - FakeBackend (test-only: records writes, no widget) — see Testing
///
/// All methods are &self. The backend owns its widget.
/// Data flows in via write(). Data flows out via the event channel
/// created at construction time.
///
/// Note: this trait intentionally has no `Send` supertrait. Widget backends
/// hold GTK widgets (`vte4::Terminal`) which are `!Send` by design — they must
/// stay on the GTK main thread. The `TerminalHandle` keeps the backend on the
/// GTK side; the transport pump receives [`TerminalEvent`]s from the channel,
/// not the backend itself. This is an orchestrator ruling that corrects doc 02's
/// original `Send` bound, which only fit the fake/test backends.
pub trait TerminalBackend {
    /// The GTK widget to embed in a tab.
    /// Always returns a valid gtk::Widget — the tab embeds this.
    /// Widget backends: their own rendering surface.
    /// Engine backends: custom render widget reading engine state.
    fn widget(&self) -> Widget;

    /// Bytes from transport (SSH/PTY/serial) → terminal.
    /// Called from the GTK main thread only (the glib channel receiver
    /// in doc 04 invokes this). Never called from tokio directly.
    fn write(&self, data: &[u8]);

    /// Terminal size in rows/cols.
    fn set_size(&self, rows: u32, cols: u32);

    /// Feature discovery — called once at tab creation.
    fn features(&self) -> TerminalFeatures;

    // ── Appearance ──────────────────────────────────────
    /// No-op if backend doesn't support it.
    fn set_color_scheme(&self, name: &str);
    fn set_font(&self, font_desc: &pango::FontDescription);
    fn set_opacity(&self, opacity: f64);
    fn set_scrollback_limit(&self, lines: u32);

    // ── Interaction ─────────────────────────────────────
    fn selected_text(&self) -> Option<String>;
    fn copy_selection(&self);
    fn paste(&self, text: &str);
    fn select_all(&self);
    fn clear(&self);

    // ── Scrollback ──────────────────────────────────────
    /// Returns None if unsupported.
    fn search(&self, text: &str, reverse: bool);
    fn scrollback_text(&self) -> Option<String>;
    fn scroll_to_top(&self);
    fn scroll_to_bottom(&self);

    // ── Lifecycle ───────────────────────────────────────
    fn reset(&self); // reset terminal state

    // ── Identity ────────────────────────────────────────
    fn name(&self) -> &str; // "vte", "ghostty-vt"
    fn version(&self) -> &str;
    fn description(&self) -> &str;
}

/// Owner handle returned by [`create_terminal`]. The receiver half is held by
/// the transport pump task; the backend keeps the sender half and emits
/// [`TerminalEvent`]s as the user interacts with the terminal.
pub struct TerminalHandle {
    pub backend: Arc<dyn TerminalBackend>,
    pub events: mpsc::Receiver<TerminalEvent>,
}

/// Construct a terminal backend by name.
///
/// * `"vte"` returns the built-in VteBackend (task P2-2).
/// * `"ghostty"` is available only with the `backend-ghostty` Cargo feature
///   (not created yet; intended for Phase 10).
/// * Any other `backend_name` falls back to the VteBackend default.
pub fn create_terminal(
    backend_name: &str,
    event_tx: mpsc::Sender<TerminalEvent>,
) -> TerminalHandle {
    let (_tx, rx) = mpsc::channel(256);

    // Clippy flags `Arc<VteBackend>` because VteBackend is `!Send`/`!Sync`.
    // The backend is intentionally main-thread only; `Arc` is still the right
    // owner for the GTK-side tab and channel receiver closures. Suppress the
    // lint locally rather than change the trait handle type.
    #[allow(clippy::arc_with_non_send_sync)]
    let backend: Arc<dyn TerminalBackend> = match backend_name {
        "vte" => Arc::new(crate::terminal::vte::VteBackend::new(event_tx)),
        #[cfg(feature = "backend-ghostty")]
        "ghostty" => Arc::new(ghostty_backend_stub()),
        _ => Arc::new(crate::terminal::vte::VteBackend::new(event_tx)),
    };

    TerminalHandle {
        backend,
        events: rx,
    }
}

#[cfg(feature = "backend-ghostty")]
fn ghostty_backend_stub() -> Arc<dyn TerminalBackend> {
    unimplemented!("GhosttyBackend is planned for Phase 10; feature backend-ghostty not wired")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::fake::FakeBackend;

    #[tokio::test]
    async fn terminal_handle_wires_events_from_backend() {
        // The VteBackend constructor requires GTK init and a display, so this
        // test uses the FakeBackend stand-in to verify the TerminalHandle
        // channel wiring without touching the widget.
        let (tx, rx) = mpsc::channel::<TerminalEvent>(256);
        let mut backend = FakeBackend::new();
        backend.event_tx = Some(tx.clone());
        let mut handle = TerminalHandle {
            backend: Arc::new(backend.clone()),
            events: rx,
        };

        let ev = TerminalEvent::DataOut(vec![0x41, 0x42]);
        backend.emit(ev.clone());

        let received =
            tokio::time::timeout(std::time::Duration::from_secs(1), handle.events.recv())
                .await
                .expect("event should arrive within 1s")
                .expect("channel should not close");
        assert_eq!(received, ev);
    }
}
