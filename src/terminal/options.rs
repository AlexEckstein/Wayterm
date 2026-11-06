//! What each backend alone takes from the settings rows (doc 02 §View
//! options), and the parsers for those rows. Display-free, and compiled
//! without the Ghostty feature, as Settings shows them regardless.

use std::ops::RangeInclusive;
use std::str::FromStr;
use std::time::Duration;

pub const GHOSTTY_WHEEL_ROWS_SETTING: &str = "ghostty.wheel_rows";
pub const GHOSTTY_CURSOR_BLINK_SETTING: &str = "ghostty.cursor_blink_ms";
pub const GHOSTTY_TEXT_BLINK_SETTING: &str = "ghostty.text_blink_ms";
pub const GHOSTTY_SEARCH_FG_SETTING: &str = "ghostty.search_fg";
pub const GHOSTTY_SEARCH_BG_SETTING: &str = "ghostty.search_bg";
pub const GHOSTTY_SEARCH_CURRENT_BG_SETTING: &str = "ghostty.search_current_bg";

pub const WHEEL_ROWS: RangeInclusive<f64> = 0.5..=30.0;
/// In milliseconds. A period of zero would spin the main loop.
pub const BLINK_PERIOD: RangeInclusive<u64> = 50..=5000;

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

impl GhosttyOptions {
    /// Read the rows through `row`; a missing or unusable row keeps its
    /// default.
    pub fn from_rows(row: &impl Fn(&str) -> Option<String>) -> Self {
        let seed = Self::default();
        let search = seed.search;
        Self {
            wheel_rows: parsed(row(GHOSTTY_WHEEL_ROWS_SETTING))
                .filter(|rows| WHEEL_ROWS.contains(rows))
                .unwrap_or(seed.wheel_rows),
            cursor_blink: period(row(GHOSTTY_CURSOR_BLINK_SETTING)).unwrap_or(seed.cursor_blink),
            text_blink: period(row(GHOSTTY_TEXT_BLINK_SETTING)).unwrap_or(seed.text_blink),
            search: SearchColors {
                fg: color(row(GHOSTTY_SEARCH_FG_SETTING)).unwrap_or(search.fg),
                bg: color(row(GHOSTTY_SEARCH_BG_SETTING)).unwrap_or(search.bg),
                current_bg: color(row(GHOSTTY_SEARCH_CURRENT_BG_SETTING))
                    .unwrap_or(search.current_bg),
            },
        }
    }
}

pub(super) fn parsed<T: FromStr>(raw: Option<String>) -> Option<T> {
    raw?.trim().parse().ok()
}

fn period(raw: Option<String>) -> Option<Duration> {
    parsed(raw)
        .filter(|ms| BLINK_PERIOD.contains(ms))
        .map(Duration::from_millis)
}

fn color(raw: Option<String>) -> Option<[u8; 3]> {
    let raw = raw?;
    let hex = raw.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// A color as its row holds it, `#rrggbb`.
pub fn color_row([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ghostty(values: &[(&str, &str)]) -> GhosttyOptions {
        GhosttyOptions::from_rows(&|key| {
            values
                .iter()
                .find(|(row, _)| *row == key)
                .map(|(_, value)| value.to_string())
        })
    }

    /// Ghostty's own values, which a tab keeps until a row says otherwise.
    #[test]
    fn view_options_start_at_ghosttys_defaults() {
        let options = ghostty(&[]);
        assert_eq!(options.wheel_rows, 3.0);
        assert_eq!(options.cursor_blink, Duration::from_millis(600));
        assert_eq!(options.text_blink, Duration::from_millis(500));
        assert_eq!(options.search.bg, [0xff, 0xe0, 0x82]);
        assert_eq!(options.search.current_bg, [0xf2, 0xa5, 0x7e]);
    }

    #[test]
    fn stored_ghostty_rows_are_read() {
        let options = ghostty(&[
            (GHOSTTY_WHEEL_ROWS_SETTING, "1.5"),
            (GHOSTTY_CURSOR_BLINK_SETTING, "250"),
            (GHOSTTY_TEXT_BLINK_SETTING, " 1000 "),
            (GHOSTTY_SEARCH_FG_SETTING, "#010203"),
            (GHOSTTY_SEARCH_BG_SETTING, "#A0B0C0"),
            (GHOSTTY_SEARCH_CURRENT_BG_SETTING, " #ffffff "),
        ]);
        assert_eq!(
            options,
            GhosttyOptions {
                wheel_rows: 1.5,
                cursor_blink: Duration::from_millis(250),
                text_blink: Duration::from_millis(1000),
                search: SearchColors {
                    fg: [1, 2, 3],
                    bg: [0xa0, 0xb0, 0xc0],
                    current_bg: [0xff, 0xff, 0xff],
                },
            }
        );
    }

    /// A blink period of zero or a NaN wheel must not reach a view.
    #[test]
    fn unusable_ghostty_rows_keep_the_defaults() {
        for (key, values) in [
            (
                GHOSTTY_WHEEL_ROWS_SETTING,
                &["0", "-1", "NaN", "inf", "x"][..],
            ),
            (GHOSTTY_CURSOR_BLINK_SETTING, &["0", "49", "5001", "-1", ""]),
            (GHOSTTY_TEXT_BLINK_SETTING, &["0", "1e3"]),
            (
                GHOSTTY_SEARCH_BG_SETTING,
                &["", "#", "ffffff", "#fff", "#gggggg", "#+1+2+3", "#ééé"],
            ),
        ] {
            for value in values {
                assert_eq!(
                    ghostty(&[(key, value)]),
                    GhosttyOptions::default(),
                    "{key}={value:?}"
                );
            }
        }
    }

    #[test]
    fn a_color_reads_back_from_its_row() {
        let stored = color_row([0x0a, 0xb0, 0xff]);
        assert_eq!(stored, "#0ab0ff");
        assert_eq!(color(Some(stored)), Some([0x0a, 0xb0, 0xff]));
    }
}
