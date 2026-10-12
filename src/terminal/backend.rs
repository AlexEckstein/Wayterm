use std::sync::Arc;

use gtk4::{pango, Widget};
use tokio::sync::mpsc;

/// Capabilities a backend supports — the UI hides what is unsupported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalFeatures {
    pub search: bool,
    pub color_schemes: bool,
    pub scrollback_search: bool,
    pub url_detection: bool,
    pub sixel_graphics: bool,
    pub kitty_graphics: bool,
    pub custom_fonts: bool,
    pub reflow: bool,
    pub alternate_screen: bool,
    pub scrollback_config: bool,
}

/// Events flowing out of the terminal toward the transport pump.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
    DataOut(Vec<u8>),
    SizeChange { rows: u32, cols: u32 },
    TitleChanged(String),
    Bell,
    Closed,
}

/// A terminal backend (doc 02): VTE today, an engine backend later,
/// [`FakeBackend`](super::FakeBackend) in tests.
///
/// Bytes come in through [`write`](Self::write); events go out through the
/// channel the backend was built with. There is deliberately no `Send`
/// bound: widget backends hold GTK objects and live on the main thread.
pub trait TerminalBackend {
    /// The widget to embed in a tab.
    fn widget(&self) -> Widget;
    /// Transport bytes → terminal. Main thread only.
    fn write(&self, data: &[u8]);
    fn set_size(&self, rows: u32, cols: u32);
    fn features(&self) -> TerminalFeatures;

    // Appearance — no-ops where unsupported.
    fn set_color_scheme(&self, name: &str);
    fn set_font(&self, font_desc: &pango::FontDescription);
    fn set_opacity(&self, opacity: f64);
    fn set_scrollback_limit(&self, lines: u32);

    // Interaction.
    fn selected_text(&self) -> Option<String>;
    fn copy_selection(&self);
    fn paste(&self, text: &str);
    fn select_all(&self);
    fn clear(&self);

    // Scrollback.
    fn search(&self, text: &str, reverse: bool);
    fn scrollback_text(&self) -> Option<String>;
    fn scroll_to_top(&self);
    fn scroll_to_bottom(&self);

    fn reset(&self);

    // Identity.
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn description(&self) -> &str;
}

/// A backend plus the receiver for the events it emits; the receiver goes
/// to the transport pump ([`TerminalTab`](crate::transport::tab::TerminalTab)).
pub struct TerminalHandle {
    pub backend: Arc<dyn TerminalBackend>,
    pub events: mpsc::Receiver<TerminalEvent>,
}

/// Build the terminal backend and its event channel. VTE is the only
/// built-in backend (doc 05); main thread only.
pub fn create_terminal() -> TerminalHandle {
    let (tx, events) = mpsc::channel(256);
    // VteBackend is !Send by design; Arc is still the right owner for the
    // main-thread closures that share it.
    #[allow(clippy::arc_with_non_send_sync)]
    let backend = Arc::new(crate::terminal::VteBackend::new(tx));
    TerminalHandle { backend, events }
}
