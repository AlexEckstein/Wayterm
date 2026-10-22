//! Trigger parsing and key capture (doc 11 §Trigger parsing).
//!
//! Bindings are stored in canonical UI format (`"Ctrl+Shift+T"`: modifiers
//! in `MODS` order, a letter upper-case, `ISO_Left_Tab` as `Tab`), so
//! two spellings of one chord compare equal. They convert to GTK
//! accelerator syntax (`"<Control><Shift>t"`) for binding; there only
//! single characters are lowercased — `Tab`, `F11` and `Delete` are
//! case-sensitive key names. Everything here is display-free except
//! [`parse_trigger`].

use gtk4::gdk::{Key, ModifierType};

/// A modifier's UI name, accelerator token, and GDK mask.
type Modifier = (&'static str, &'static str, ModifierType);

/// Every modifier, in the order bindings render them (so round trips are
/// stable).
const MODS: &[Modifier] = &[
    ("Ctrl", "<Control>", ModifierType::CONTROL_MASK),
    ("Shift", "<Shift>", ModifierType::SHIFT_MASK),
    ("Alt", "<Alt>", ModifierType::ALT_MASK),
    ("Super", "<Super>", ModifierType::SUPER_MASK),
    ("Meta", "<Meta>", ModifierType::META_MASK),
];

/// `"Ctrl+Shift+T"` → `"<Control><Shift>t"`, modifiers in the order
/// written; `None` for an unknown modifier or an empty key (a trailing
/// `'+'`). A literal `+` key cannot be written in this format — bind it
/// through capture.
pub fn ui_key_to_accel(key: &str) -> Option<String> {
    let (mods, name) = split_ui_key(key)?;
    let mut accel: String = mods.iter().map(|(_, accel, _)| *accel).collect();
    accel.push_str(&normalize_key_name(name));
    Some(accel)
}

/// `"<Control><Shift>t"` → `"Ctrl+Shift+T"`, the reverse of
/// [`ui_key_to_accel`], in canonical form. `None` for anything
/// [`parse_trigger`] would reject.
pub fn accel_to_ui_key(accel: &str) -> Option<String> {
    let mut rest = accel;
    let mut mods = ModifierType::empty();
    while let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?; // unterminated '<' → None
        let token = &rest[..=end + 1]; // "<Control>" — brackets included
        mods |= MODS.iter().find(|(_, accel, _)| *accel == token)?.2;
        rest = &stripped[end + 1..];
    }
    KeyChord::new(Key::from_name(rest)?, mods).to_ui_key()
}

/// `key` in canonical form: `"Shift+Ctrl+t"` → `"Ctrl+Shift+T"`. Two
/// spellings of one chord canonicalize alike; `None` for anything
/// [`ui_key_is_bindable`] rejects.
pub fn canonical_ui_key(key: &str) -> Option<String> {
    KeyChord::from_ui_key(key)?.to_ui_key()
}

/// The key part of a UI-format binding (after the last `'+'`).
pub fn key_token(key: &str) -> &str {
    key.rsplit('+').next().unwrap_or(key)
}

/// Whether GDK knows `token` as a key name (no display needed): `Return`
/// resolves, `Enter` does not.
pub fn key_token_is_bindable(token: &str) -> bool {
    !token.is_empty() && Key::from_name(token).is_some()
}

/// Whether `key` parses and names a key GTK knows — the gate every stored
/// binding passes, so none can be accepted and then never fire.
pub fn ui_key_is_bindable(key: &str) -> bool {
    ui_key_to_accel(key).is_some() && key_token_is_bindable(key_token(key))
}

/// A chord: a captured key press (`key()` + `modifier_state()`), or a
/// parsed binding. Built canonical — only `MODS` modifiers kept (lock
/// and button masks dropped), a letter upper-case, `ISO_Left_Tab` (what
/// Shift+Tab sends) as `Tab` — so equal chords compare equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub key: Key,
    pub modifiers: ModifierType,
}

impl KeyChord {
    pub fn new(key: Key, modifiers: ModifierType) -> Self {
        let mask = MODS
            .iter()
            .fold(ModifierType::empty(), |all, (_, _, mask)| all | *mask);
        let key = if key == Key::ISO_Left_Tab {
            Key::Tab
        } else {
            key.to_upper()
        };
        Self {
            key,
            modifiers: modifiers & mask,
        }
    }

    /// The inverse of [`KeyChord::to_ui_key`].
    pub fn from_ui_key(key: &str) -> Option<Self> {
        let (mods, name) = split_ui_key(key)?;
        let modifiers = mods
            .iter()
            .fold(ModifierType::empty(), |all, (_, _, mask)| all | *mask);
        Some(Self::new(Key::from_name(name)?, modifiers))
    }

    /// The chord in canonical UI format (`"Ctrl+Shift+T"`); `None` when
    /// GDK has no name for the key.
    pub fn to_ui_key(self) -> Option<String> {
        let mut out = String::new();
        for (ui, _, mask) in MODS {
            if self.modifiers.contains(*mask) {
                out.push_str(ui);
                out.push('+');
            }
        }
        out.push_str(&self.key.name()?);
        Some(out)
    }
}

/// A UI-format binding's modifiers, as written, and its key token; `None`
/// for an unknown modifier or an empty key.
fn split_ui_key(key: &str) -> Option<(Vec<&'static Modifier>, &str)> {
    let mut tokens: Vec<&str> = key.split('+').collect();
    let name = tokens.pop().filter(|name| !name.is_empty())?;
    let mods = tokens
        .into_iter()
        .map(|token| MODS.iter().find(|(ui, _, _)| *ui == token))
        .collect::<Option<_>>()?;
    Some((mods, name))
}

/// Single characters lowercase; key names keep their case.
fn normalize_key_name(name: &str) -> String {
    if name.chars().count() == 1 {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

/// `"Ctrl+Shift+T"` → a `gtk4::ShortcutTrigger`; `None` when the format
/// or the key name is invalid. Requires GTK to be initialized.
pub fn parse_trigger(key: &str) -> Option<gtk4::ShortcutTrigger> {
    let accel = ui_key_to_accel(key)?;
    gtk4::ShortcutTrigger::parse_string(&accel)
}

#[cfg(test)]
mod tests;
