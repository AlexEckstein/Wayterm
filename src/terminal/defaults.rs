//! Font, opacity and scrollback for terminal tabs (doc 02 §Font, opacity
//! and scrollback): the global settings rows and a session's overrides on
//! them. Also the Ghostty view's options (doc 02 §View options). Display-free.

use std::str::FromStr;
use std::time::Duration;

use gtk4::pango;

pub const DEFAULT_FONT_FAMILY_SETTING: &str = "default_font_family";
pub const DEFAULT_FONT_SIZE_SETTING: &str = "default_font_size";
pub const DEFAULT_OPACITY_SETTING: &str = "default_opacity";
pub const SCROLLBACK_LIMIT_SETTING: &str = "scrollback_limit";

/// In points. Stored sizes can come from an import; past this they are
/// ignored, as a tab would not fit one cell.
const MAX_FONT_SIZE: f64 = 512.0;

/// The global rows, parsed. Their defaults are the seed's (doc 13
/// §Settings rows).
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalDefaults {
    pub font_family: String,
    pub font_size: f64,
    pub opacity: f64,
    pub scrollback_lines: u32,
    pub ghostty: GhosttyOptions,
}

/// What only the Ghostty view draws or decides for itself; global, with no
/// session override (doc 02 §View options).
#[derive(Clone, Debug, PartialEq)]
pub struct GhosttyOptions {
    /// Rows per wheel notch.
    pub wheel_rows: f64,
    /// How long a blinking cursor stays on, and off; Ghostty's default.
    pub cursor_blink: Duration,
    /// How long blinking text stays on, and off: one cycle a second, SGR 5's
    /// slow blink. SGR 6 blinks the same, as libghostty-vt keeps one flag.
    pub text_blink: Duration,
    pub search: SearchColors,
}

/// Search highlight colors, Ghostty's defaults: dark text on yellow, on
/// orange for the current match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchColors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub current_bg: [u8; 3],
}

impl Default for GhosttyOptions {
    fn default() -> Self {
        Self {
            wheel_rows: 3.0,
            cursor_blink: Duration::from_millis(600),
            text_blink: Duration::from_millis(500),
            search: SearchColors::default(),
        }
    }
}

impl Default for SearchColors {
    fn default() -> Self {
        Self {
            fg: [0, 0, 0],
            bg: [0xff, 0xe0, 0x82],
            current_bg: [0xf2, 0xa5, 0x7e],
        }
    }
}

impl Default for TerminalDefaults {
    fn default() -> Self {
        Self {
            font_family: "Monospace".to_string(),
            font_size: 12.0,
            opacity: 1.0,
            scrollback_lines: 10_000,
            ghostty: GhosttyOptions::default(),
        }
    }
}

impl TerminalDefaults {
    /// Read the rows through `row`; a missing or unusable row keeps its
    /// default.
    pub fn from_rows(row: impl Fn(&str) -> Option<String>) -> Self {
        let seed = Self::default();
        let family = row(DEFAULT_FONT_FAMILY_SETTING);
        Self {
            font_family: usable_family(family.as_deref()).map_or(seed.font_family, str::to_string),
            font_size: parsed(row(DEFAULT_FONT_SIZE_SETTING))
                .filter(|size| usable_size(*size))
                .unwrap_or(seed.font_size),
            opacity: parsed(row(DEFAULT_OPACITY_SETTING))
                .filter(|opacity: &f64| opacity.is_finite())
                .unwrap_or(seed.opacity),
            scrollback_lines: parsed(row(SCROLLBACK_LIMIT_SETTING))
                .unwrap_or(seed.scrollback_lines),
            ghostty: seed.ghostty,
        }
    }

    /// A session's font: family and size override separately.
    pub fn font(&self, family: Option<&str>, size: Option<f64>) -> pango::FontDescription {
        let size = size.filter(|size| usable_size(*size));
        let mut font = pango::FontDescription::new();
        font.set_family(usable_family(family).unwrap_or(&self.font_family));
        font.set_size((size.unwrap_or(self.font_size) * f64::from(pango::SCALE)).round() as i32);
        font
    }

    /// A session's opacity; backends clamp it to `0..=1`.
    pub fn opacity(&self, session: Option<f64>) -> f64 {
        session
            .filter(|opacity| opacity.is_finite())
            .unwrap_or(self.opacity)
    }
}

fn parsed<T: FromStr>(raw: Option<String>) -> Option<T> {
    raw?.trim().parse().ok()
}

fn usable_family(family: Option<&str>) -> Option<&str> {
    family.map(str::trim).filter(|family| !family.is_empty())
}

fn usable_size(size: f64) -> bool {
    size > 0.0 && size <= MAX_FONT_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[(&str, &str)]) -> TerminalDefaults {
        TerminalDefaults::from_rows(|key| {
            values
                .iter()
                .find(|(row, _)| *row == key)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn stored_rows_are_read() {
        let defaults = rows(&[
            (DEFAULT_FONT_FAMILY_SETTING, " Fira Code "),
            (DEFAULT_FONT_SIZE_SETTING, "14.5"),
            (DEFAULT_OPACITY_SETTING, "0.8"),
            (SCROLLBACK_LIMIT_SETTING, "0"),
        ]);
        assert_eq!(
            defaults,
            TerminalDefaults {
                font_family: "Fira Code".to_string(),
                font_size: 14.5,
                opacity: 0.8,
                scrollback_lines: 0,
                ..Default::default()
            }
        );
    }

    /// Ghostty's own values, which a tab keeps until a row says otherwise.
    #[test]
    fn view_options_start_at_ghosttys_defaults() {
        let options = TerminalDefaults::from_rows(|_| None).ghostty;
        assert_eq!(options.wheel_rows, 3.0);
        assert_eq!(options.cursor_blink, Duration::from_millis(600));
        assert_eq!(options.text_blink, Duration::from_millis(500));
        assert_eq!(options.search.bg, [0xff, 0xe0, 0x82]);
        assert_eq!(options.search.current_bg, [0xf2, 0xa5, 0x7e]);
    }

    #[test]
    fn missing_or_unusable_rows_keep_the_seed_values() {
        assert_eq!(rows(&[]), TerminalDefaults::default());
        let unusable = rows(&[
            (DEFAULT_FONT_FAMILY_SETTING, "   "),
            (DEFAULT_FONT_SIZE_SETTING, "0"),
            (DEFAULT_OPACITY_SETTING, "NaN"),
            (SCROLLBACK_LIMIT_SETTING, "-1"),
        ]);
        assert_eq!(unusable, TerminalDefaults::default());
        let too_large = rows(&[(DEFAULT_FONT_SIZE_SETTING, "1e9")]);
        assert_eq!(too_large, TerminalDefaults::default());
    }

    #[test]
    fn a_session_overrides_family_and_size_separately() {
        let defaults = TerminalDefaults::default();
        let font = |family, size| defaults.font(family, size).to_string();
        assert_eq!(font(None, None), "Monospace 12");
        assert_eq!(font(Some("Fira Code"), None), "Fira Code 12");
        assert_eq!(font(None, Some(9.5)), "Monospace 9.5");
        assert_eq!(font(Some("Fira Code"), Some(9.5)), "Fira Code 9.5");
    }

    #[test]
    fn an_unusable_session_font_falls_through_to_the_global_row() {
        let defaults = TerminalDefaults::default();
        let font = |family, size| defaults.font(family, size).to_string();
        assert_eq!(font(Some(""), Some(0.0)), "Monospace 12");
        assert_eq!(font(Some("  "), Some(f64::NAN)), "Monospace 12");
        assert_eq!(font(None, Some(-3.0)), "Monospace 12");
    }

    #[test]
    fn a_session_opacity_overrides_the_global_row() {
        let defaults = TerminalDefaults {
            opacity: 0.8,
            ..Default::default()
        };
        assert_eq!(defaults.opacity(None), 0.8);
        assert_eq!(defaults.opacity(Some(0.0)), 0.0);
        assert_eq!(defaults.opacity(Some(f64::NAN)), 0.8);
    }
}
