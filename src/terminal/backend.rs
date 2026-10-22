//! The [`TerminalBackend`] trait, its events and features, and
//! [`create_terminal`], which picks the backend (doc 02 §Backend selection).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk4::{pango, Widget};
use tokio::sync::mpsc;

/// Capabilities a backend supports — the UI hides what is unsupported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalFeatures {
    /// Text search.
    pub search: bool,
    /// Built-in color schemes.
    pub color_schemes: bool,
    /// Searching the scrollback, not only the screen.
    pub scrollback_search: bool,
    /// Clickable URLs.
    pub url_detection: bool,
    /// SIXEL images.
    pub sixel_graphics: bool,
    /// Kitty graphics protocol images.
    pub kitty_graphics: bool,
    /// Font changes.
    pub custom_fonts: bool,
    /// Rewrapping lines on resize.
    pub reflow: bool,
    /// The alternate screen (full-screen programs).
    pub alternate_screen: bool,
    /// A configurable scrollback limit.
    pub scrollback_config: bool,
}

/// Events flowing out of the terminal toward the transport pump.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
    /// Input bytes for the remote side (keystrokes, pastes, replies).
    DataOut(Vec<u8>),
    /// The grid changed size.
    SizeChange { rows: u32, cols: u32 },
    /// The program set the window title (OSC 0/2).
    TitleChanged(String),
    /// BEL.
    Bell,
    /// The widget went away.
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
    /// Resize the grid to `rows` × `cols` cells.
    fn set_size(&self, rows: u32, cols: u32);
    /// What this backend supports; queried once per tab.
    fn features(&self) -> TerminalFeatures;

    // Appearance — no-ops where unsupported.
    /// Apply a built-in palette by name (doc 02 §Color schemes).
    fn set_color_scheme(&self, name: &str);
    /// Set the font.
    fn set_font(&self, font_desc: &pango::FontDescription);
    /// Set the background opacity, 0.0–1.0.
    fn set_opacity(&self, opacity: f64);
    /// Lines of scrollback to keep.
    fn set_scrollback_limit(&self, lines: u32);

    // Interaction.
    /// The selection as plain text, if any.
    fn selected_text(&self) -> Option<String>;
    /// Copy the selection to the clipboard.
    fn copy_selection(&self);
    /// Paste `text` as the user would (bracketed where enabled).
    fn paste(&self, text: &str);
    /// Select the whole buffer.
    fn select_all(&self);
    /// Clear the screen and scrollback.
    fn clear(&self);

    // Scrollback.
    /// Step to the next (or previous) match of `text`; empty clears.
    fn search(&self, text: &str, reverse: bool);
    /// The whole buffer as text, scrollback included.
    fn scrollback_text(&self) -> Option<String>;
    /// Scroll the viewport to the oldest line.
    fn scroll_to_top(&self);
    /// Scroll the viewport to the live screen.
    fn scroll_to_bottom(&self);

    /// Full terminal reset.
    fn reset(&self);

    // Identity.
    /// Short id, e.g. `vte`.
    fn name(&self) -> &str;
    /// Version string.
    fn version(&self) -> &str;
    /// Human-readable one-liner.
    fn description(&self) -> &str;
}

/// A backend plus the receiver for the events it emits; the receiver goes
/// to the transport pump ([`TerminalTab`](crate::ui::terminal_tab::TerminalTab)).
pub struct TerminalHandle {
    /// The backend, shared by the tab, the output drain and the actions.
    pub backend: Arc<dyn TerminalBackend>,
    /// What the backend emits, for the transport pump.
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
    /// The GTK4 VTE widget.
    #[default]
    Vte,
    /// libghostty-vt with a custom widget.
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

    /// The `default_backend` spelling.
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

/// The installed preference; set from Settings, read per new tab.
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
