use std::sync::atomic::{AtomicBool, Ordering};
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

/// A terminal backend (doc 02): VTE, Ghostty when compiled in, or
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
    /// Input → transport, as if typed (macro playback, doc 12): sent as
    /// `DataOut` through the same queue as keystrokes. Main thread only.
    fn send_input(&self, data: &[u8]);
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
/// to the transport pump ([`TerminalTab`](crate::ui::tabs::terminal_tab::TerminalTab)).
pub struct TerminalHandle {
    pub backend: Arc<dyn TerminalBackend>,
    pub events: mpsc::Receiver<TerminalEvent>,
}

/// The settings row naming the backend for new tabs: `vte` or `ghostty`
/// (doc 13 §Settings rows).
pub const DEFAULT_BACKEND_SETTING: &str = "default_backend";

/// Whether this build has the experimental Ghostty backend
/// (`backend-ghostty` feature).
pub const GHOSTTY_COMPILED: bool = cfg!(feature = "backend-ghostty");

/// A terminal backend [`create_terminal`] can build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendKind {
    #[default]
    Vte,
    Ghostty,
}

impl BackendKind {
    /// Parse a `default_backend` value; missing or unknown is VTE.
    pub fn from_setting(raw: Option<&str>) -> Self {
        match raw
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("ghostty") => Self::Ghostty,
            _ => Self::Vte,
        }
    }

    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Vte => "vte",
            Self::Ghostty => "ghostty",
        }
    }

    /// The backend actually built: Ghostty falls back to VTE in a build
    /// without `backend-ghostty`.
    pub fn effective(self) -> Self {
        if GHOSTTY_COMPILED {
            self
        } else {
            Self::Vte
        }
    }
}

static PREFER_GHOSTTY: AtomicBool = AtomicBool::new(false);

/// Record the stored `default_backend` preference; tabs created afterwards
/// use it, open tabs keep their backend.
pub fn install_backend_preference(kind: BackendKind) {
    PREFER_GHOSTTY.store(kind == BackendKind::Ghostty, Ordering::SeqCst);
}

/// The installed preference (VTE until one is installed).
pub fn backend_preference() -> BackendKind {
    if PREFER_GHOSTTY.load(Ordering::SeqCst) {
        BackendKind::Ghostty
    } else {
        BackendKind::Vte
    }
}

/// Build the terminal backend and its event channel: the installed
/// [`backend_preference`] when this build has it, VTE otherwise or when
/// libghostty-vt cannot allocate a terminal. Main thread only.
pub fn create_terminal() -> TerminalHandle {
    let (tx, events) = mpsc::channel(256);
    #[cfg(feature = "backend-ghostty")]
    if backend_preference().effective() == BackendKind::Ghostty {
        match crate::terminal::GhosttyBackend::new(tx.clone()) {
            Some(backend) => {
                // !Send by design, like VteBackend; the Arc shares the trait
                // object between main-thread closures.
                let backend: Box<dyn TerminalBackend> = Box::new(backend);
                return TerminalHandle {
                    backend: Arc::from(backend),
                    events,
                };
            }
            None => eprintln!("Wayterm: libghostty-vt terminal unavailable; using VTE"),
        }
    }
    // VteBackend is !Send by design; Arc is still the right owner for the
    // main-thread closures that share it.
    #[allow(clippy::arc_with_non_send_sync)]
    let backend = Arc::new(crate::terminal::VteBackend::new(tx));
    TerminalHandle { backend, events }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backend_values_parse_with_vte_as_fallback() {
        assert_eq!(BackendKind::from_setting(None), BackendKind::Vte);
        assert_eq!(BackendKind::from_setting(Some("vte")), BackendKind::Vte);
        assert_eq!(
            BackendKind::from_setting(Some(" Ghostty ")),
            BackendKind::Ghostty
        );
        assert_eq!(
            BackendKind::from_setting(Some("alacritty")),
            BackendKind::Vte
        );
        for kind in [BackendKind::Vte, BackendKind::Ghostty] {
            assert_eq!(BackendKind::from_setting(Some(kind.as_setting())), kind);
        }
    }

    #[test]
    fn ghostty_is_built_only_when_compiled_in() {
        assert_eq!(BackendKind::Vte.effective(), BackendKind::Vte);
        let expected = if GHOSTTY_COMPILED {
            BackendKind::Ghostty
        } else {
            BackendKind::Vte
        };
        assert_eq!(BackendKind::Ghostty.effective(), expected);
    }
}
