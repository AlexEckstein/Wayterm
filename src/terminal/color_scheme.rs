//! Terminal color schemes (doc 02 §Color schemes, doc 06 §Terminal
//! appearance): the built-in palettes, the stored scheme choice, and how a
//! tab's scheme is resolved. Display-free: backends turn a [`ColorScheme`]
//! into widget colors, the UI supplies the system theme.
//!
//! A tab's scheme is its session's `color_scheme` override, else the global
//! `default_color_scheme` row, else [`SchemeChoice::System`]. `System` is
//! not a palette: it resolves to [`LIGHT`] or [`DARK`] by the desktop theme
//! ([`classify_system_theme`]).

/// The global settings row naming the scheme for tabs without an override
/// (doc 06 §Default Data).
pub const DEFAULT_COLOR_SCHEME_SETTING: &str = "default_color_scheme";

/// An sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `#rrggbb`.
    pub const fn hex(value: u32) -> Self {
        Self((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    /// WCAG 2 relative luminance, 0.0 (black) to 1.0 (white).
    pub fn luminance(self) -> f64 {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.0) + 0.7152 * channel(self.1) + 0.0722 * channel(self.2)
    }

    /// WCAG 2 contrast ratio against `other`, 1.0 to 21.0.
    pub fn contrast(self, other: Self) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
}

/// One terminal palette: default text and background, the cursor, and the
/// 16 ANSI colors (0–7 normal, 8–15 bright).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorScheme {
    /// The name stored in settings and passed to
    /// [`set_color_scheme`](super::TerminalBackend::set_color_scheme).
    pub name: &'static str,
    pub foreground: Rgb,
    pub background: Rgb,
    /// The cursor block, and the text under it.
    pub cursor: Rgb,
    pub cursor_foreground: Rgb,
    pub palette: [Rgb; 16],
}

/// The Tango palette, as in GNOME Terminal's Tango schemes.
const TANGO: [Rgb; 16] = [
    Rgb::hex(0x2E3436),
    Rgb::hex(0xCC0000),
    Rgb::hex(0x4E9A06),
    Rgb::hex(0xC4A000),
    Rgb::hex(0x3465A4),
    Rgb::hex(0x75507B),
    Rgb::hex(0x06989A),
    Rgb::hex(0xD3D7CF),
    Rgb::hex(0x555753),
    Rgb::hex(0xEF2929),
    Rgb::hex(0x8AE234),
    Rgb::hex(0xFCE94F),
    Rgb::hex(0x729FCF),
    Rgb::hex(0xAD7FA8),
    Rgb::hex(0x34E2E2),
    Rgb::hex(0xEEEEEC),
];

/// GNOME Terminal's "Tango light": dark grey on off-white.
pub const LIGHT: ColorScheme = ColorScheme {
    name: "Light",
    foreground: Rgb::hex(0x2E3436),
    background: Rgb::hex(0xEEEEEC),
    cursor: Rgb::hex(0x2E3436),
    cursor_foreground: Rgb::hex(0xEEEEEC),
    palette: TANGO,
};

/// GNOME Terminal's "Tango dark": light grey on charcoal.
pub const DARK: ColorScheme = ColorScheme {
    name: "Dark",
    foreground: Rgb::hex(0xD3D7CF),
    background: Rgb::hex(0x2E3436),
    cursor: Rgb::hex(0xD3D7CF),
    cursor_foreground: Rgb::hex(0x2E3436),
    palette: TANGO,
};

/// Every built-in palette.
pub const SCHEMES: [&ColorScheme; 2] = [&LIGHT, &DARK];

/// The built-in palette called `name` (case and surrounding space ignored).
pub fn by_name(name: &str) -> Option<&'static ColorScheme> {
    let name = name.trim();
    SCHEMES
        .into_iter()
        .find(|scheme| scheme.name.eq_ignore_ascii_case(name))
}

/// Light or dark: what the desktop theme is, and which palette `System`
/// picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brightness {
    Light,
    Dark,
}

impl Brightness {
    pub fn scheme(self) -> &'static ColorScheme {
        match self {
            Self::Light => &LIGHT,
            Self::Dark => &DARK,
        }
    }
}

/// What a theme the classifier cannot place counts as: dark, like the
/// pre-`System` seed.
pub const UNKNOWN_THEME: Brightness = Brightness::Dark;

/// Widely shipped light GTK themes whose names carry no `light` segment.
const LIGHT_THEMES: [&str; 12] = [
    "adwaita",
    "default",
    "breeze",
    "yaru",
    "highcontrast",
    "arc",
    "materia",
    "pop",
    "elementary",
    "greybird",
    "numix",
    "oxygen-gtk",
];

/// The desktop theme's brightness from GTK's settings:
/// `gtk-application-prefer-dark-theme` and `gtk-theme-name`. The dark
/// preference wins when set; otherwise a name with a `dark` segment
/// (`Adwaita-dark`, `Breeze Dark`, `Yaru_dark`) is dark, one with a `light`
/// segment or a known light theme's name is light, and anything else —
/// no name included — is [`UNKNOWN_THEME`].
pub fn classify_system_theme(prefer_dark: bool, theme_name: Option<&str>) -> Brightness {
    if prefer_dark {
        return Brightness::Dark;
    }
    let Some(name) = theme_name.map(str::trim).filter(|name| !name.is_empty()) else {
        return UNKNOWN_THEME;
    };
    let name = name.to_ascii_lowercase();
    let mut segments = name.split(|c: char| !c.is_ascii_alphanumeric());
    if segments.clone().any(|segment| segment == "dark") {
        return Brightness::Dark;
    }
    if segments.any(|segment| segment == "light") || LIGHT_THEMES.contains(&name.as_str()) {
        return Brightness::Light;
    }
    UNKNOWN_THEME
}

/// A stored scheme value: a palette, or `System` to follow the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemeChoice {
    System,
    Light,
    Dark,
}

impl SchemeChoice {
    /// Every choice, in the order the settings page offers them.
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// Parse a stored value (case and surrounding space ignored); empty or
    /// unknown is `None`, so resolution falls through to the next level.
    pub fn from_setting(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|choice| choice.as_setting().eq_ignore_ascii_case(raw))
    }

    /// The stored value, which is also the label shown.
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// The palette, with `System` following `system`.
    pub fn scheme(self, system: Brightness) -> &'static ColorScheme {
        match self {
            Self::System => system.scheme(),
            Self::Light => &LIGHT,
            Self::Dark => &DARK,
        }
    }
}

/// The choice in force: the session override, else the global default,
/// else `System`. Empty or unknown values count as missing.
pub fn resolve_choice(session_override: Option<&str>, global: Option<&str>) -> SchemeChoice {
    session_override
        .and_then(SchemeChoice::from_setting)
        .or_else(|| global.and_then(SchemeChoice::from_setting))
        .unwrap_or(SchemeChoice::System)
}

/// The palette a tab shows ([`resolve_choice`], `System` following
/// `system`).
pub fn resolve(
    session_override: Option<&str>,
    global: Option<&str>,
    system: Brightness,
) -> &'static ColorScheme {
    resolve_choice(session_override, global).scheme(system)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_are_complete_distinct_and_readable() {
        for scheme in SCHEMES {
            assert_eq!(scheme.palette.len(), 16);
            assert!(
                scheme.foreground.contrast(scheme.background) >= 7.0,
                "{}: text must meet WCAG AAA on its background",
                scheme.name
            );
            assert!(
                scheme.cursor.contrast(scheme.cursor_foreground) >= 7.0,
                "{}: text under the cursor must stay readable",
                scheme.name
            );
            assert_eq!(by_name(scheme.name), Some(scheme));
            assert_eq!(
                by_name(&format!(" {} ", scheme.name.to_uppercase())),
                Some(scheme)
            );
        }
        assert!(
            LIGHT.background.luminance() > 0.5,
            "Light has a light background"
        );
        assert!(
            DARK.background.luminance() < 0.1,
            "Dark has a dark background"
        );
        assert_ne!(LIGHT.background, DARK.background);
        assert_eq!(by_name("System"), None, "System is not a palette");
        assert_eq!(by_name("Solarized"), None);
    }

    #[test]
    fn contrast_is_symmetric_and_bounded() {
        let (black, white) = (Rgb::hex(0x000000), Rgb::hex(0xFFFFFF));
        assert!((black.contrast(white) - 21.0).abs() < 1e-9);
        assert_eq!(black.contrast(white), white.contrast(black));
        assert!((white.contrast(white) - 1.0).abs() < 1e-9);
        assert_eq!(Rgb::hex(0x12AB34), Rgb(0x12, 0xAB, 0x34));
    }

    #[test]
    fn dark_preference_beats_the_theme_name() {
        assert_eq!(
            classify_system_theme(true, Some("Adwaita")),
            Brightness::Dark
        );
        assert_eq!(
            classify_system_theme(true, Some("Breeze")),
            Brightness::Dark
        );
        assert_eq!(classify_system_theme(true, None), Brightness::Dark);
    }

    #[test]
    fn dark_name_segments_are_dark() {
        for name in [
            "Adwaita-dark",
            "Breeze-Dark",
            "Breeze Dark",
            "Yaru_dark",
            "Dark",
        ] {
            assert_eq!(
                classify_system_theme(false, Some(name)),
                Brightness::Dark,
                "{name}"
            );
        }
        // A segment, not a substring.
        assert_eq!(
            classify_system_theme(false, Some("Darkling")),
            UNKNOWN_THEME
        );
    }

    #[test]
    fn light_names_are_light() {
        for name in [
            "Adwaita",
            "Breeze",
            "Yaru",
            " HighContrast ",
            "Materia-light",
            "Default",
        ] {
            assert_eq!(
                classify_system_theme(false, Some(name)),
                Brightness::Light,
                "{name}"
            );
        }
    }

    #[test]
    fn unknown_themes_use_the_defined_default() {
        assert_eq!(UNKNOWN_THEME, Brightness::Dark);
        for name in [None, Some(""), Some("  "), Some("Nordic"), Some("Darkling")] {
            assert_eq!(
                classify_system_theme(false, name),
                UNKNOWN_THEME,
                "{name:?}"
            );
        }
    }

    #[test]
    fn choices_parse_and_round_trip() {
        for choice in SchemeChoice::ALL {
            assert_eq!(
                SchemeChoice::from_setting(choice.as_setting()),
                Some(choice)
            );
        }
        assert_eq!(
            SchemeChoice::from_setting(" light "),
            Some(SchemeChoice::Light)
        );
        assert_eq!(SchemeChoice::from_setting(""), None);
        assert_eq!(SchemeChoice::from_setting("Solarized"), None);
    }

    #[test]
    fn session_override_wins() {
        let scheme = resolve(Some("Light"), Some("Dark"), Brightness::Dark);
        assert_eq!(scheme, &LIGHT);
        let scheme = resolve(Some("Dark"), Some("Light"), Brightness::Light);
        assert_eq!(scheme, &DARK);
    }

    #[test]
    fn missing_values_fall_through_to_system() {
        // Empty or unknown override → the global row.
        for missing in [None, Some(""), Some("Solarized")] {
            assert_eq!(resolve(missing, Some("Light"), Brightness::Dark), &LIGHT);
        }
        // No usable global row either → System.
        assert_eq!(resolve_choice(None, None), SchemeChoice::System);
        assert_eq!(
            resolve_choice(Some(""), Some("bogus")),
            SchemeChoice::System
        );
        assert_eq!(resolve(None, None, Brightness::Light), &LIGHT);
        assert_eq!(resolve(None, None, Brightness::Dark), &DARK);
    }

    #[test]
    fn system_follows_the_classified_theme() {
        let system = classify_system_theme(false, Some("Breeze"));
        assert_eq!(resolve(None, Some("System"), system), &LIGHT);
        let system = classify_system_theme(false, Some("Breeze-Dark"));
        assert_eq!(resolve(None, Some("System"), system), &DARK);
        let system = classify_system_theme(true, Some("Breeze"));
        assert_eq!(resolve(Some("System"), Some("Light"), system), &DARK);
        // A fixed override ignores the system theme.
        assert_eq!(resolve(Some("Light"), Some("System"), system), &LIGHT);
    }
}
