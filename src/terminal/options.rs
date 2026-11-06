//! What each backend alone takes from the settings rows (doc 02 §View
//! options), and the parsers for those rows. Display-free, and compiled
//! without the Ghostty feature, as the rows are read regardless.

use std::ops::RangeInclusive;
use std::str::FromStr;
use std::time::Duration;

pub const VTE_FOREGROUND_SETTING: &str = "vte.foreground";
pub const VTE_BACKGROUND_SETTING: &str = "vte.background";
pub const VTE_CURSOR_SHAPE_SETTING: &str = "vte.cursor_shape";
pub const VTE_CURSOR_BLINK_SETTING: &str = "vte.cursor_blink";
pub const VTE_AUDIBLE_BELL_SETTING: &str = "vte.audible_bell";
pub const VTE_SELECTION_FG_SETTING: &str = "vte.selection_fg";
pub const VTE_SELECTION_BG_SETTING: &str = "vte.selection_bg";
pub const VTE_LINE_HEIGHT_SETTING: &str = "vte.line_height";
pub const VTE_BOLD_IS_BRIGHT_SETTING: &str = "vte.bold_is_bright";
pub const VTE_HIDE_MOUSE_SETTING: &str = "vte.hide_mouse";

pub const GHOSTTY_FOREGROUND_SETTING: &str = "ghostty.foreground";
pub const GHOSTTY_BACKGROUND_SETTING: &str = "ghostty.background";
pub const GHOSTTY_CURSOR_STYLE_SETTING: &str = "ghostty.cursor_style";
pub const GHOSTTY_CURSOR_BLINKS_SETTING: &str = "ghostty.cursor_blinks";
pub const GHOSTTY_IMAGE_LIMIT_SETTING: &str = "ghostty.image_limit_mb";
pub const GHOSTTY_WHEEL_ROWS_SETTING: &str = "ghostty.wheel_rows";
pub const GHOSTTY_CURSOR_BLINK_SETTING: &str = "ghostty.cursor_blink_ms";
pub const GHOSTTY_TEXT_BLINK_SETTING: &str = "ghostty.text_blink_ms";
pub const GHOSTTY_SELECTION_FG_SETTING: &str = "ghostty.selection_fg";
pub const GHOSTTY_SELECTION_BG_SETTING: &str = "ghostty.selection_bg";
pub const GHOSTTY_SEARCH_FG_SETTING: &str = "ghostty.search_fg";
pub const GHOSTTY_SEARCH_BG_SETTING: &str = "ghostty.search_bg";
pub const GHOSTTY_SEARCH_CURRENT_BG_SETTING: &str = "ghostty.search_current_bg";

/// VTE's own bounds on the cell height scale.
pub const LINE_HEIGHT: RangeInclusive<f64> = 1.0..=2.0;
pub const WHEEL_ROWS: RangeInclusive<f64> = 0.5..=30.0;
/// In milliseconds. A period of zero would spin the main loop.
pub const BLINK_PERIOD: RangeInclusive<u64> = 50..=5000;
/// In megabytes. Ghostty's default is the top: PNG sizes are checked against
/// it before a decode, and a remote program picks the size.
pub const IMAGE_LIMIT: RangeInclusive<u32> = 0..=320;

/// A setting stored as one of a few names; the name is also its label.
pub trait RowChoice: Copy + PartialEq + 'static {
    const ALL: &'static [(Self, &'static str)];

    fn from_row(raw: Option<String>) -> Option<Self> {
        let raw = raw?;
        let raw = raw.trim();
        Self::ALL
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(raw))
            .map(|(choice, _)| *choice)
    }

    fn position(self) -> usize {
        Self::ALL
            .iter()
            .position(|(choice, _)| *choice == self)
            .unwrap_or_default()
    }

    fn row(self) -> &'static str {
        Self::ALL[self.position()].1
    }
}

/// The cursor a VTE tab starts with, until a program sets its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VteCursor {
    #[default]
    Block,
    IBeam,
    Underline,
}

impl RowChoice for VteCursor {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::Block, "Block"),
        (Self::IBeam, "I-beam"),
        (Self::Underline, "Underline"),
    ];
}

/// Whether the cursor blinks; `System` follows the GTK setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VteBlink {
    #[default]
    System,
    On,
    Off,
}

impl RowChoice for VteBlink {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::System, "System"),
        (Self::On, "On"),
        (Self::Off, "Off"),
    ];
}

/// The cursor a Ghostty tab starts with, until a program sets its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GhosttyCursor {
    #[default]
    Block,
    Bar,
    Underline,
    Hollow,
}

impl RowChoice for GhosttyCursor {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::Block, "Block"),
        (Self::Bar, "Bar"),
        (Self::Underline, "Underline"),
        (Self::Hollow, "Hollow"),
    ];
}

/// What only VTE terminals take; global, with no session override (doc 02
/// §View options). The defaults are VTE's own.
#[derive(Clone, Debug, PartialEq)]
pub struct VteOptions {
    pub colors: DefaultColors,
    pub cursor_shape: VteCursor,
    pub cursor_blink: VteBlink,
    pub audible_bell: bool,
    pub selection: SelectionColors,
    /// Cell height as a multiple of the font's.
    pub line_height: f64,
    pub bold_is_bright: bool,
    pub hide_mouse: bool,
}

impl Default for VteOptions {
    fn default() -> Self {
        Self {
            colors: DefaultColors::default(),
            cursor_shape: VteCursor::default(),
            cursor_blink: VteBlink::default(),
            audible_bell: true,
            selection: SelectionColors::default(),
            line_height: 1.0,
            bold_is_bright: false,
            hide_mouse: false,
        }
    }
}

impl VteOptions {
    /// Read the rows through `row`; a missing or unusable row keeps its
    /// default.
    pub fn from_rows(row: &impl Fn(&str) -> Option<String>) -> Self {
        let seed = Self::default();
        Self {
            colors: DefaultColors {
                fg: color(row(VTE_FOREGROUND_SETTING)),
                bg: color(row(VTE_BACKGROUND_SETTING)),
            },
            cursor_shape: VteCursor::from_row(row(VTE_CURSOR_SHAPE_SETTING))
                .unwrap_or(seed.cursor_shape),
            cursor_blink: VteBlink::from_row(row(VTE_CURSOR_BLINK_SETTING))
                .unwrap_or(seed.cursor_blink),
            audible_bell: flag(row(VTE_AUDIBLE_BELL_SETTING)).unwrap_or(seed.audible_bell),
            selection: SelectionColors {
                fg: color(row(VTE_SELECTION_FG_SETTING)),
                bg: color(row(VTE_SELECTION_BG_SETTING)),
            },
            line_height: parsed(row(VTE_LINE_HEIGHT_SETTING))
                .filter(|height| LINE_HEIGHT.contains(height))
                .unwrap_or(seed.line_height),
            bold_is_bright: flag(row(VTE_BOLD_IS_BRIGHT_SETTING)).unwrap_or(seed.bold_is_bright),
            hide_mouse: flag(row(VTE_HIDE_MOUSE_SETTING)).unwrap_or(seed.hide_mouse),
        }
    }
}

/// What only the Ghostty view draws or decides for itself; global, with no
/// session override (doc 02 §View options).
#[derive(Clone, Debug, PartialEq)]
pub struct GhosttyOptions {
    pub colors: DefaultColors,
    pub cursor_style: GhosttyCursor,
    pub cursor_blinks: bool,
    /// Megabytes of Kitty images each screen holds; 0 turns them off.
    pub image_limit_mb: u32,
    /// Rows per wheel notch.
    pub wheel_rows: f64,
    /// How long a blinking cursor stays on, and off; Ghostty's default.
    pub cursor_blink: Duration,
    /// How long blinking text stays on, and off: one cycle a second, SGR 5's
    /// slow blink. SGR 6 blinks the same, as libghostty-vt keeps one flag.
    pub text_blink: Duration,
    pub selection: SelectionColors,
    pub search: SearchColors,
}

/// Text and background colors that replace the color scheme's; either can
/// be left to the scheme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DefaultColors {
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
}

/// Colors a selection takes in place of swapping the cell's own; either
/// can be left to the swap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionColors {
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
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
            colors: DefaultColors::default(),
            cursor_style: GhosttyCursor::default(),
            cursor_blinks: false,
            image_limit_mb: *IMAGE_LIMIT.end(),
            wheel_rows: 3.0,
            cursor_blink: Duration::from_millis(600),
            text_blink: Duration::from_millis(500),
            selection: SelectionColors::default(),
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
            colors: DefaultColors {
                fg: color(row(GHOSTTY_FOREGROUND_SETTING)),
                bg: color(row(GHOSTTY_BACKGROUND_SETTING)),
            },
            cursor_style: GhosttyCursor::from_row(row(GHOSTTY_CURSOR_STYLE_SETTING))
                .unwrap_or(seed.cursor_style),
            cursor_blinks: flag(row(GHOSTTY_CURSOR_BLINKS_SETTING)).unwrap_or(seed.cursor_blinks),
            image_limit_mb: parsed(row(GHOSTTY_IMAGE_LIMIT_SETTING))
                .filter(|limit| IMAGE_LIMIT.contains(limit))
                .unwrap_or(seed.image_limit_mb),
            wheel_rows: parsed(row(GHOSTTY_WHEEL_ROWS_SETTING))
                .filter(|rows| WHEEL_ROWS.contains(rows))
                .unwrap_or(seed.wheel_rows),
            cursor_blink: period(row(GHOSTTY_CURSOR_BLINK_SETTING)).unwrap_or(seed.cursor_blink),
            text_blink: period(row(GHOSTTY_TEXT_BLINK_SETTING)).unwrap_or(seed.text_blink),
            selection: SelectionColors {
                fg: color(row(GHOSTTY_SELECTION_FG_SETTING)),
                bg: color(row(GHOSTTY_SELECTION_BG_SETTING)),
            },
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

fn flag(raw: Option<String>) -> Option<bool> {
    match raw?.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
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

    fn stored<'a>(values: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |key| {
            values
                .iter()
                .find(|(row, _)| *row == key)
                .map(|(_, value)| value.to_string())
        }
    }

    fn ghostty(values: &[(&str, &str)]) -> GhosttyOptions {
        GhosttyOptions::from_rows(&stored(values))
    }

    fn vte(values: &[(&str, &str)]) -> VteOptions {
        VteOptions::from_rows(&stored(values))
    }

    #[test]
    fn stored_vte_rows_are_read() {
        let options = vte(&[
            (VTE_FOREGROUND_SETTING, "#0a0b0c"),
            (VTE_CURSOR_SHAPE_SETTING, "i-beam"),
            (VTE_CURSOR_BLINK_SETTING, "Off"),
            (VTE_AUDIBLE_BELL_SETTING, "0"),
            (VTE_SELECTION_FG_SETTING, "#010203"),
            (VTE_LINE_HEIGHT_SETTING, "1.5"),
            (VTE_BOLD_IS_BRIGHT_SETTING, "1"),
            (VTE_HIDE_MOUSE_SETTING, "1"),
        ]);
        assert_eq!(
            options,
            VteOptions {
                colors: DefaultColors {
                    fg: Some([10, 11, 12]),
                    bg: None,
                },
                cursor_shape: VteCursor::IBeam,
                cursor_blink: VteBlink::Off,
                audible_bell: false,
                selection: SelectionColors {
                    fg: Some([1, 2, 3]),
                    bg: None,
                },
                line_height: 1.5,
                bold_is_bright: true,
                hide_mouse: true,
            }
        );
    }

    #[test]
    fn unusable_vte_rows_keep_the_defaults() {
        for (key, values) in [
            (VTE_CURSOR_SHAPE_SETTING, &["", "Bar"][..]),
            (VTE_CURSOR_BLINK_SETTING, &["Sometimes"]),
            (VTE_AUDIBLE_BELL_SETTING, &["", "maybe"]),
            (VTE_LINE_HEIGHT_SETTING, &["0.9", "2.1", "NaN"]),
        ] {
            for value in values {
                assert_eq!(
                    vte(&[(key, value)]),
                    VteOptions::default(),
                    "{key}={value:?}"
                );
            }
        }
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
            (GHOSTTY_BACKGROUND_SETTING, "#0A0B0C"),
            (GHOSTTY_CURSOR_STYLE_SETTING, " bar "),
            (GHOSTTY_CURSOR_BLINKS_SETTING, "1"),
            (GHOSTTY_IMAGE_LIMIT_SETTING, "0"),
            (GHOSTTY_WHEEL_ROWS_SETTING, "1.5"),
            (GHOSTTY_CURSOR_BLINK_SETTING, "250"),
            (GHOSTTY_TEXT_BLINK_SETTING, " 1000 "),
            (GHOSTTY_SELECTION_BG_SETTING, "#102030"),
            (GHOSTTY_SEARCH_FG_SETTING, "#010203"),
            (GHOSTTY_SEARCH_BG_SETTING, "#A0B0C0"),
            (GHOSTTY_SEARCH_CURRENT_BG_SETTING, " #ffffff "),
        ]);
        assert_eq!(
            options,
            GhosttyOptions {
                colors: DefaultColors {
                    fg: None,
                    bg: Some([10, 11, 12]),
                },
                cursor_style: GhosttyCursor::Bar,
                cursor_blinks: true,
                image_limit_mb: 0,
                wheel_rows: 1.5,
                cursor_blink: Duration::from_millis(250),
                text_blink: Duration::from_millis(1000),
                selection: SelectionColors {
                    fg: None,
                    bg: Some([0x10, 0x20, 0x30]),
                },
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
    fn a_choice_reads_back_from_its_row() {
        for (choice, name) in GhosttyCursor::ALL {
            assert_eq!(choice.row(), *name);
            assert_eq!(
                GhosttyCursor::from_row(Some(choice.row().into())),
                Some(*choice)
            );
        }
    }

    #[test]
    fn a_color_reads_back_from_its_row() {
        let stored = color_row([0x0a, 0xb0, 0xff]);
        assert_eq!(stored, "#0ab0ff");
        assert_eq!(color(Some(stored)), Some([0x0a, 0xb0, 0xff]));
    }
}
