//! Trigger parsing and the key-capture side of the hotkey engine
//! (doc 07 §"Trigger Parsing").
//!
//! Wayterm's persisted key format is the doc's *UI format* —
//! `"Ctrl+Shift+T"` — which [`ui_key_to_accel`] converts to GTK
//! accelerator syntax (`"<Control><Shift>t"`) for
//! [`parse_trigger`]. The reverse, [`accel_to_ui_key`], exists so the
//! settings UI can render a binding that came back from GTK (an
//! accelerator name or a captured chord) in the same format the
//! database stores.
//!
//! Two doc rules are load-bearing and are preserved verbatim here:
//!
//! * the parse lowercases **single characters only** — key names keep
//!   their case, because `"Tab"`, `"F11"` and `"Delete"` are
//!   case-sensitive tokens in accelerator strings (the doc's original
//!   lowercased the whole string and corrupted them);
//! * an unknown modifier, or a trailing `'+'`, yields `None`.
//!
//! Nothing in this module needs a display except the functions that
//! hand an accelerator string to GTK ([`parse_trigger`],
//! [`KeyChord::to_trigger`]): they call `gtk4::ShortcutTrigger`, which
//! asserts GTK is initialized. Every string-level function — including
//! the capture side, which is built from `gdk::Key` values a caller
//! already has — is display-free and covered by the tests here.
//!
//! ## Key names
//!
//! `gdk::Key::from_name` is the same lookup GTK's accelerator parser
//! uses and needs no display, so it is what validates a key token
//! ([`key_token_is_bindable`]). It is also why the default table spells
//! the Enter key `Return`, not `Enter`: `gdk_keyval_from_name` has no
//! `Enter` entry, so GTK rejects `"<Control>Enter"` outright and such a
//! binding would silently never fire (see [`crate::hotkeys::defaults`]).

use gdk4::{Key, ModifierType};

/// UI modifier name → GTK accelerator token → GDK modifier mask.
///
/// The doc's `ui_key_to_accel` sketch declares a two-column `MODS`
/// table inside the function; it lives at module scope here as three
/// columns (UI name, accelerator token, mask) so the reverse mapping
/// ([`accel_to_ui_key`]) and the capture side ([`KeyChord`]) read the
/// same rows and the three directions cannot drift apart. The row order
/// is the doc's, and is the order both directions emit modifiers in, so
/// a round-trip through either one is stable.
const MODS: &[(&str, &str, ModifierType)] = &[
    ("Ctrl", "<Control>", ModifierType::CONTROL_MASK),
    ("Shift", "<Shift>", ModifierType::SHIFT_MASK),
    ("Alt", "<Alt>", ModifierType::ALT_MASK),
    ("Super", "<Super>", ModifierType::SUPER_MASK),
    ("Meta", "<Meta>", ModifierType::META_MASK),
];

/// `"Ctrl+Shift+T"` → `"<Control><Shift>t"`.
///
/// Doc 07 §"Trigger Parsing", transcribed line for line — the only
/// edit is the modifier table, hoisted to [`MODS`] above so the reverse
/// mapping and the capture side share it. `None` when the last `'+'`-
/// separated token is empty (a trailing `'+'`, or an empty string) or
/// when any preceding token is not a known modifier.
///
/// Note the doc's format cannot express a literal `'+'` key: `'+'` is
/// the separator, so `"Ctrl++"` parses as a trailing `'+'`. Bind the
/// plus key through the capture side instead, which never parses a
/// string.
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

/// `"<Control><Shift>t"` → `"Ctrl+Shift+t"` — the reverse of
/// [`ui_key_to_accel`].
///
/// Renders a binding that came from GTK (an accelerator name, or a
/// captured chord) in the UI format the database stores, so the
/// settings UI and the persistence path never disagree about a chord.
///
/// `None` when the accelerator is malformed (unterminated `<`), carries
/// a modifier outside Wayterm's vocabulary (`<Hyper>`), names no key,
/// or names a key `gdk` does not know — the same inputs
/// [`parse_trigger`] rejects. The round-trip is
/// *chord*-preserving, not string-identical: `"Ctrl+Shift+T"` comes
/// back as `"Ctrl+Shift+t"`, which parses to the same accelerator, and
/// `"<Shift><Control>t"` renders as `"Ctrl+Shift+t"` whatever order
/// GTK emitted the modifiers in.
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

/// The key token of a UI-format binding: everything after the last
/// `'+'`.
pub fn key_token(key: &str) -> &str {
    key.rsplit('+').next().unwrap_or(key)
}

/// Whether GTK could resolve `token` as a key.
///
/// Checks the same `gdk_keyval_from_name` table GTK's accelerator
/// parser uses, which needs no display. `"Tab"`, `"F11"`, `"Delete"`
/// and `"Return"` resolve; `"Enter"`, `"bogus"` and `""` do not.
pub fn key_token_is_bindable(token: &str) -> bool {
    !token.is_empty() && Key::from_name(token).is_some()
}

/// Whether `key` is a UI-format binding the engine will accept.
///
/// The gate [`ActionRegistry::rebind_checked`](crate::hotkeys::ActionRegistry::rebind_checked)
/// applies before storing a binding: the doc's parse must succeed (no
/// unknown modifier, no trailing `'+'`) *and* the key token must be a
/// name GTK knows — so a binding can never be accepted and then fail
/// silently at install time. Case is irrelevant to the key check: GDK
/// knows both `"T"` and `"t"`, and [`ui_key_to_accel`] normalizes a
/// single character to lowercase either way.
pub fn ui_key_is_bindable(key: &str) -> bool {
    ui_key_to_accel(key).is_some() && key_token_is_bindable(key_token(key))
}

/// A captured key chord: the display-free equivalent of the two
/// `gdk::EventKey` accessors doc 07 §"Trigger Parsing" names
/// (`event.key()` + `event.modifier_state()`).
///
/// The settings UI's capture button builds one of these from the key
/// press it receives, so the capture path is exercised by the tests
/// here without a display or a real event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub key: Key,
    pub modifiers: ModifierType,
}

impl KeyChord {
    /// Build a chord from a `gdk::EventKey`'s `key()` and
    /// `modifier_state()`.
    pub fn new(key: Key, modifiers: ModifierType) -> Self {
        Self { key, modifiers }
    }

    /// Build a chord from a UI-format binding (`"Ctrl+Shift+T"`).
    ///
    /// The persistence-side inverse of [`KeyChord::to_ui_key`], used to
    /// re-derive a GTK trigger for a stored binding without relying on
    /// GTK's own name normalization.
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

    /// The chord as a UI-format string (`"Ctrl+Shift+t"`).
    ///
    /// A single-character key name is lowercased, exactly as
    /// [`ui_key_to_accel`] lowercases it — and as GTK itself does for a
    /// Shift+letter chord — so a captured binding is always a string the
    /// persistence path parses back to the same accelerator.
    ///
    /// `None` when GDK has no canonical name for the key.
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

    /// The modifier masks Wayterm renders, in [`MODS`] order.
    ///
    /// Button and lock masks are not modifiers in this sense and are
    /// dropped: a chord captured while a mouse button was held is still
    /// the same chord.
    fn active_modifiers(self) -> Vec<ModifierType> {
        MODS.iter()
            .filter(|(_, _, mask)| self.modifiers.contains(*mask))
            .map(|(_, _, mask)| *mask)
            .collect()
    }

    /// The GTK trigger for this chord.
    ///
    /// Doc 07 §"Trigger Parsing" says the capture path should build the
    /// trigger straight from key + modifiers and never round-trip a
    /// string. gtk4-rs 0.11 exposes no such constructor below GTK 4.24
    /// — `ShortcutTrigger::create_with_aliases` is
    /// `#[cfg(feature = "v4_24")]` and this crate builds against
    /// `v4_14` — so the capture path renders the accelerator from the
    /// same [`KeyChord`] values and hands it to `parse_string`. The
    /// trigger is identical, and capture cannot disagree with the
    /// persistence path because both derive from one chord.
    ///
    /// Requires GTK to be initialized (the UI task's call site runs
    /// inside a `gtk4::Application`); `None` when the key has no
    /// accelerator name.
    pub fn to_trigger(self) -> Option<gtk4::ShortcutTrigger> {
        parse_trigger(&self.to_ui_key()?)
    }
}

/// Single characters lowercase, multi-character names untouched — the
/// doc 07 §"Trigger Parsing" rule, applied to a key name from either
/// side of the conversion.
fn normalize_key_name(name: &str) -> String {
    if name.chars().count() == 1 {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

/// `"Ctrl+"` / `"<Control>"` prefix for the masks that are present.
///
/// `pick` chooses which column of [`MODS`] to emit and `sep` how to
/// join, because the two formats differ in both ways — the UI names are
/// joined with `'+'`, while the accelerator tokens already carry their
/// own `<>`. One loop and one order for both directions.
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

/// `"Ctrl+Shift+T"` → a `gtk4::ShortcutTrigger`.
///
/// Doc 07 §"Trigger Parsing": parse the UI format into accelerator
/// syntax, then hand it to GTK. `None` when the UI format is malformed
/// (unknown modifier, trailing `'+'`) or when GTK itself rejects the
/// accelerator — which is what a key name GDK does not know looks like
/// from the outside (`"Enter"` is rejected, `"Return"` is accepted).
///
/// Requires GTK to be initialized: `gtk_shortcut_trigger_parse_string`
/// asserts the main thread is initialized, so this function is *not*
/// display-free and the tests here exercise [`ui_key_to_accel`] instead.
pub fn parse_trigger(key: &str) -> Option<gtk4::ShortcutTrigger> {
    let accel = ui_key_to_accel(key)?;
    gtk4::ShortcutTrigger::parse_string(&accel)
}

#[cfg(test)]
mod tests;
