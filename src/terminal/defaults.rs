//! Font, opacity and scrollback for terminal tabs (doc 02 §Font, opacity
//! and scrollback): the global settings rows and a session's overrides on
//! them, with each backend's own options beside them
//! ([`options`](super::options)). Display-free.

use gtk4::pango;

use super::options::{parsed, GhosttyOptions};

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
            ghostty: GhosttyOptions::from_rows(&row),
        }
    }

    /// Whether every terminal's rows match `other`'s, each backend's own
    /// options aside: only then does a change leave other backends' tabs
    /// alone.
    pub fn same_shared_rows(&self, other: &Self) -> bool {
        let aligned = Self {
            ghostty: other.ghostty.clone(),
            ..self.clone()
        };
        aligned == *other
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
    fn backend_options_are_not_shared_rows() {
        let defaults = TerminalDefaults::default();
        let wheel = TerminalDefaults {
            ghostty: GhosttyOptions {
                wheel_rows: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(defaults.same_shared_rows(&wheel));
        let font = TerminalDefaults {
            font_size: 20.0,
            ..wheel.clone()
        };
        assert!(!wheel.same_shared_rows(&font));
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
