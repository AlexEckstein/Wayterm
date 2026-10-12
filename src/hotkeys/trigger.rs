//! Trigger parsing and key capture (doc 07 §Trigger Parsing).
//!
//! Bindings are stored in UI format (`"Ctrl+Shift+T"`), converted to GTK
//! accelerator syntax (`"<Control><Shift>t"`) for binding, and rendered
//! back for display. Only single characters are lowercased — `Tab`, `F11`
//! and `Delete` are case-sensitive key names. Everything here is
//! display-free except [`parse_trigger`] and [`KeyChord::to_trigger`].

use gtk4::gdk::{Key, ModifierType};

/// UI name, accelerator token, and GDK mask of each modifier, in the
/// order every direction emits them (so round trips are stable).
const MODS: &[(&str, &str, ModifierType)] = &[
    ("Ctrl", "<Control>", ModifierType::CONTROL_MASK),
    ("Shift", "<Shift>", ModifierType::SHIFT_MASK),
    ("Alt", "<Alt>", ModifierType::ALT_MASK),
    ("Super", "<Super>", ModifierType::SUPER_MASK),
    ("Meta", "<Meta>", ModifierType::META_MASK),
];

/// `"Ctrl+Shift+T"` → `"<Control><Shift>t"`; `None` for an unknown
/// modifier or an empty key (a trailing `'+'`). A literal `+` key cannot be
/// written in this format — bind it through capture.
pub fn ui_key_to_accel(key: &str) -> Option<String> {
    let tokens: Vec<&str> = key.split('+').collect();
    let key_name = tokens.last()?;
    if key_name.is_empty() {
        return None; // trailing '+'
    }
    let mut out = String::new();
    for t in &tokens[..tokens.len() - 1] {
        let m = MODS.iter().find(|(ui, _, _)| *ui == *t)?; // unknown modifier → None
        out.push_str(m.1);
    }
    // Key names keep their case; single letters normalize to lowercase:
    let key_name = if key_name.chars().count() == 1 {
        key_name.to_lowercase()
    } else {
        key_name.to_string()
    };
    out.push_str(&key_name);
    Some(out)
}

/// `"<Control><Shift>t"` → `"Ctrl+Shift+t"`, the reverse of
/// [`ui_key_to_accel`]. Chord-preserving rather than string-identical
/// (modifiers come back in `MODS` order). `None` for anything
/// [`parse_trigger`] would reject.
pub fn accel_to_ui_key(accel: &str) -> Option<String> {
    let mut rest = accel;
    let mut mods: Vec<ModifierType> = Vec::new();
    while let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?; // unterminated '<' → None
        let token = &rest[..=end + 1]; // "<Control>" — brackets included
        let (_, _, mask) = MODS.iter().find(|(_, accel, _)| *accel == token)?;
        if !mods.contains(mask) {
            mods.push(*mask);
        }
        rest = &stripped[end + 1..];
    }
    if rest.is_empty() || !key_token_is_bindable(rest) {
        return None;
    }
    Some(format!(
        "{}{}",
        modifier_prefix(&mods, |ui, _| ui, "+"),
        normalize_key_name(rest)
    ))
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

/// A captured key press (`key()` + `modifier_state()`), as the settings
/// UI's capture button records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub key: Key,
    pub modifiers: ModifierType,
}

impl KeyChord {
    pub fn new(key: Key, modifiers: ModifierType) -> Self {
        Self { key, modifiers }
    }

    /// The inverse of [`KeyChord::to_ui_key`].
    pub fn from_ui_key(key: &str) -> Option<Self> {
        let tokens: Vec<&str> = key.split('+').collect();
        let key_token = *tokens.last()?;
        let mut modifiers = ModifierType::empty();
        for t in &tokens[..tokens.len() - 1] {
            let (_, _, mask) = MODS.iter().find(|(ui, _, _)| *ui == *t)?;
            modifiers |= *mask;
        }
        Some(Self {
            key: Key::from_name(key_token)?,
            modifiers,
        })
    }

    /// The chord in UI format (`"Ctrl+Shift+t"`); `None` when GDK has no
    /// name for the key.
    pub fn to_ui_key(self) -> Option<String> {
        let name = self.key.name()?;
        let mods = self.active_modifiers();
        Some(format!(
            "{}{}",
            modifier_prefix(&mods, |ui, _| ui, "+"),
            normalize_key_name(&name)
        ))
    }

    /// The chord as a GTK accelerator string (`"<Control><Shift>t"`).
    pub fn to_accel(self) -> Option<String> {
        let name = self.key.name()?;
        let mods = self.active_modifiers();
        Some(format!(
            "{}{}",
            modifier_prefix(&mods, |_, accel| accel, ""),
            normalize_key_name(&name)
        ))
    }

    /// The modifiers Wayterm renders, in [`MODS`] order; button and lock
    /// masks are ignored.
    fn active_modifiers(self) -> Vec<ModifierType> {
        MODS.iter()
            .filter(|(_, _, mask)| self.modifiers.contains(*mask))
            .map(|(_, _, mask)| *mask)
            .collect()
    }

    /// The GTK trigger for this chord. gtk4-rs exposes no key+modifiers
    /// constructor below GTK 4.24, so this goes through the accelerator
    /// string. Requires GTK to be initialized.
    pub fn to_trigger(self) -> Option<gtk4::ShortcutTrigger> {
        parse_trigger(&self.to_ui_key()?)
    }
}

/// Single characters lowercase; key names keep their case.
fn normalize_key_name(name: &str) -> String {
    if name.chars().count() == 1 {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

/// The modifier prefix in either format: `pick` selects the [`MODS`]
/// column, `sep` the joiner.
fn modifier_prefix(
    mods: &[ModifierType],
    pick: impl Fn(&'static str, &'static str) -> &'static str,
    sep: &str,
) -> String {
    let mut out = String::new();
    for (ui, accel, mask) in MODS {
        if mods.contains(mask) {
            out.push_str(pick(ui, accel));
            out.push_str(sep);
        }
    }
    out
}

/// `"Ctrl+Shift+T"` → a `gtk4::ShortcutTrigger`; `None` when the format
/// or the key name is invalid. Requires GTK to be initialized.
pub fn parse_trigger(key: &str) -> Option<gtk4::ShortcutTrigger> {
    let accel = ui_key_to_accel(key)?;
    gtk4::ShortcutTrigger::parse_string(&accel)
}

#[cfg(test)]
mod tests;
