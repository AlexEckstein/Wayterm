use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gtk4::prelude::*;
use gtk4::{Widget, pango};
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
    SizeChange {
        rows: u32,
        cols: u32,
    },
    Bell,
    /// A program asked for a desktop notification (OSC 9, OSC 777). `title`
    /// is empty when the protocol has none. Remote text, shown only through
    /// [`tray::terminal_alert`](crate::ui::tray::terminal_alert).
    Notification {
        title: String,
        body: String,
    },
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

/// The CSS class a backend's widget carries while its default background
/// is translucent; the window clears its own background under it (doc 02
/// §Opacity).
pub const TRANSLUCENT_CLASS: &str = "wayterm-translucent";

/// `opacity` as a backend applies it: clamped to `0..=1`, NaN opaque.
pub(super) fn clamp_opacity(opacity: f64) -> f64 {
    if opacity.is_nan() {
        1.0
    } else {
        opacity.clamp(0.0, 1.0)
    }
}

/// Add or drop [`TRANSLUCENT_CLASS`] on `widget` for a clamped `opacity`.
pub(super) fn mark_translucent(widget: &impl IsA<Widget>, opacity: f64) {
    if opacity < 1.0 {
        widget.add_css_class(TRANSLUCENT_CLASS);
    } else {
        widget.remove_css_class(TRANSLUCENT_CLASS);
    }
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

/// Whether this build has the Ghostty backend
/// (`backend-ghostty` feature).
pub const GHOSTTY_COMPILED: bool = cfg!(feature = "backend-ghostty");

/// The linked libghostty-vt's version, such as `1.0.0`; `None` in a build
/// without the backend.
pub fn ghostty_version() -> Option<String> {
    #[cfg(feature = "backend-ghostty")]
    {
        // SAFETY: a static NUL-terminated string the library owns.
        let version =
            unsafe { std::ffi::CStr::from_ptr(super::ghostty::ffi::wt_ghostty_version()) };
        Some(version.to_string_lossy().into_owned())
    }
    #[cfg(not(feature = "backend-ghostty"))]
    None
}

/// The logged note when Ghostty was asked for and VTE is built instead.
const GHOSTTY_UNAVAILABLE: &str = "libghostty-vt terminal unavailable; using VTE";

/// A terminal backend [`create_terminal_for`] can build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendKind {
    #[default]
    Vte,
    Ghostty,
}

impl BackendKind {
    /// Parse a `default_backend` value; missing or unknown is VTE.
    pub fn from_setting(raw: Option<&str>) -> Self {
        Self::parse(raw).unwrap_or_default()
    }

    /// The backend for a session: its `terminal_backend` value, `global`
    /// when that is empty or unknown.
    pub fn for_session(preferred: Option<&str>, global: Self) -> Self {
        Self::parse(preferred).unwrap_or(global)
    }

    /// The backend `raw` names (case and surrounding space ignored).
    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw?.trim().to_ascii_lowercase().as_str() {
            "vte" => Some(Self::Vte),
            "ghostty" => Some(Self::Ghostty),
            _ => None,
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
        if GHOSTTY_COMPILED { self } else { Self::Vte }
    }
}

static PREFER_GHOSTTY: AtomicBool = AtomicBool::new(false);

/// Record the stored `default_backend` preference; tabs created afterwards
/// use it unless their session names its own, open tabs keep their backend.
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

/// Build the installed [`backend_preference`] ([`create_terminal_for`]).
/// Main thread only.
pub fn create_terminal() -> TerminalHandle {
    create_terminal_for(backend_preference())
}

/// Build the terminal backend and its event channel: `kind` when this
/// build has it, VTE otherwise or when libghostty-vt cannot allocate a
/// terminal. Main thread only.
pub fn create_terminal_for(kind: BackendKind) -> TerminalHandle {
    let (tx, events) = mpsc::channel(256);
    if kind.effective() != kind {
        tracing::warn!("{GHOSTTY_UNAVAILABLE}");
    }
    #[cfg(feature = "backend-ghostty")]
    if kind.effective() == BackendKind::Ghostty {
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
            None => tracing::warn!("{GHOSTTY_UNAVAILABLE}"),
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
    fn a_ghostty_version_exactly_when_the_backend_is_built() {
        let version = ghostty_version();
        assert_eq!(version.is_some(), GHOSTTY_COMPILED);
        if let Some(version) = version {
            assert!(
                version.starts_with(|c: char| c.is_ascii_digit()),
                "{version}"
            );
        }
    }

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
    fn a_session_value_wins_over_the_global_row() {
        use BackendKind::{Ghostty, Vte};
        assert_eq!(BackendKind::for_session(Some("ghostty"), Vte), Ghostty);
        assert_eq!(BackendKind::for_session(Some(" Ghostty "), Vte), Ghostty);
        assert_eq!(BackendKind::for_session(Some("vte"), Ghostty), Vte);
    }

    #[test]
    fn an_empty_or_unknown_session_value_follows_the_global_row() {
        for global in [BackendKind::Vte, BackendKind::Ghostty] {
            for preferred in [None, Some(""), Some("  "), Some("alacritty")] {
                assert_eq!(BackendKind::for_session(preferred, global), global);
            }
        }
    }

    #[test]
    fn a_session_asking_for_ghostty_is_built_only_when_compiled_in() {
        let built = BackendKind::for_session(Some("ghostty"), BackendKind::Vte).effective();
        assert_eq!(built == BackendKind::Ghostty, GHOSTTY_COMPILED);
    }

    #[test]
    fn opacity_clamps_to_the_unit_range_and_nan_is_opaque() {
        assert_eq!(clamp_opacity(0.8), 0.8);
        assert_eq!(clamp_opacity(-1.0), 0.0);
        assert_eq!(clamp_opacity(2.0), 1.0);
        assert_eq!(clamp_opacity(f64::NAN), 1.0);
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
