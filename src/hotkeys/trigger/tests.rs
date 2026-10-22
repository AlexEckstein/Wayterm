//! Trigger parsing tests (doc 11 §Trigger parsing).
//!
//! Every doc example is asserted verbatim, and the two rules the doc
//! calls out as fixes — single characters lowercase / key names keep
//! their case, and unknown modifier or trailing `'+'` → `None` — are
//! locked individually so a future edit cannot reintroduce either bug.
//!
//! Nothing here calls [`super::parse_trigger`]: it reaches
//! `gtk_shortcut_trigger_parse_string`, which asserts GTK is
//! initialized, so it is the UI layer's function to exercise. The tests
//! instead pin the layer below it (`ui_key_to_accel`) plus the same GDK
//! key-name table GTK's accelerator parser uses, which is what makes a
//! binding dead in the first place.

use gtk4::gdk::{Key, ModifierType};

use super::*;

/// The doc's four worked examples, verbatim.
#[test]
fn doc_examples() {
    assert_eq!(
        ui_key_to_accel("Ctrl+Shift+T").as_deref(),
        Some("<Control><Shift>t")
    );
    assert_eq!(
        ui_key_to_accel("Ctrl+Alt+Delete").as_deref(),
        Some("<Control><Alt>Delete")
    );
    assert_eq!(ui_key_to_accel("Alt+1").as_deref(), Some("<Alt>1"));
    assert_eq!(ui_key_to_accel("F11").as_deref(), Some("F11"));
}

/// Single characters normalize to lowercase; the doc's original
/// lowercased whole strings and corrupted key names.
#[test]
fn single_characters_are_lowercased() {
    assert_eq!(
        ui_key_to_accel("Ctrl+Shift+T").as_deref(),
        Some("<Control><Shift>t")
    );
    assert_eq!(
        ui_key_to_accel("Ctrl+Shift+t").as_deref(),
        Some("<Control><Shift>t")
    );
    assert_eq!(ui_key_to_accel("X").as_deref(), Some("x"));
    assert_eq!(ui_key_to_accel("1").as_deref(), Some("1"));
}

#[test]
fn named_keys_keep_their_case() {
    for name in [
        "Tab",
        "Delete",
        "Escape",
        "BackSpace",
        "F11",
        "F2",
        "Return",
        "ISO_Enter",
        "KP_Enter",
        "space",
        "slash",
    ] {
        assert_eq!(
            ui_key_to_accel(name).as_deref(),
            Some(name),
            "key name {name:?} must keep its case"
        );
    }
}

#[test]
fn every_modifier_maps() {
    for (ui, accel) in [
        ("Ctrl", "<Control>"),
        ("Shift", "<Shift>"),
        ("Alt", "<Alt>"),
        ("Super", "<Super>"),
        ("Meta", "<Meta>"),
    ] {
        assert_eq!(
            ui_key_to_accel(&format!("{ui}+k")).as_deref(),
            Some(format!("{accel}k").as_str())
        );
    }
}

#[test]
fn all_modifiers_combined() {
    assert_eq!(
        ui_key_to_accel("Ctrl+Shift+Alt+Super+Meta+k").as_deref(),
        Some("<Control><Shift><Alt><Super><Meta>k")
    );
}

#[test]
fn modifier_order_is_preserved_as_written() {
    // The parser does not sort; GTK does that later.
    assert_eq!(
        ui_key_to_accel("Shift+Ctrl+t").as_deref(),
        Some("<Shift><Control>t")
    );
}

#[test]
fn bare_key_without_modifiers() {
    assert_eq!(ui_key_to_accel("F5").as_deref(), Some("F5"));
    assert_eq!(ui_key_to_accel("Delete").as_deref(), Some("Delete"));
}

#[test]
fn trailing_plus_is_none() {
    assert_eq!(ui_key_to_accel("Ctrl+"), None);
    assert_eq!(ui_key_to_accel("Ctrl+Shift+"), None);
    assert_eq!(ui_key_to_accel("+"), None);
}

#[test]
fn empty_string_is_none() {
    assert_eq!(ui_key_to_accel(""), None);
}

#[test]
fn unknown_modifier_is_none() {
    assert_eq!(ui_key_to_accel("Hyper+k"), None);
    assert_eq!(ui_key_to_accel("Ctrl+Command+t"), None);
    assert_eq!(
        ui_key_to_accel("ctrl+t"),
        None,
        "modifier names are case-sensitive"
    );
    // A modifier name in the key slot is the *key* token, so this is a
    // two-token parse, not an unknown-modifier one — and it is not a
    // binding (see bare_modifier_is_not_a_binding).
    assert_eq!(
        ui_key_to_accel("Ctrl+Shift").as_deref(),
        Some("<Control>Shift")
    );
}

#[test]
fn bare_modifier_is_not_a_binding() {
    // Parses (the token is the key name), but GDK has no key "Ctrl".
    assert_eq!(ui_key_to_accel("Ctrl").as_deref(), Some("Ctrl"));
    assert!(!ui_key_is_bindable("Ctrl"));
    assert!(!ui_key_is_bindable("Shift"));
}

#[test]
fn unknown_key_name_is_not_a_binding() {
    assert_eq!(ui_key_to_accel("Bogus").as_deref(), Some("Bogus"));
    assert!(!ui_key_is_bindable("Bogus"));
    assert!(!ui_key_is_bindable("Ctrl+Bogus"));
    assert!(ui_key_is_bindable("Ctrl+Tab"));
}

#[test]
fn enter_is_not_a_gdk_key_name() {
    // Doc 11's `connect_session` = `Enter` / `rename_tab` =
    // `Ctrl+Shift+Enter` could never fire; the default table spells the
    // same physical key `Return`. See src/hotkeys/defaults.rs.
    assert_eq!(Key::from_name("Enter"), None);
    assert!(!key_token_is_bindable("Enter"));
    assert!(!ui_key_is_bindable("Enter"));
    assert!(!ui_key_is_bindable("Ctrl+Shift+Enter"));
}

#[test]
fn return_is_a_gdk_key_name() {
    assert!(key_token_is_bindable("Return"));
    assert!(ui_key_is_bindable("Return"));
    assert!(ui_key_is_bindable("Ctrl+Shift+Return"));
    assert_eq!(
        Key::from_name("Return")
            .and_then(|k| k.name())
            .map(|n| n.to_string()),
        Some("Return".to_string())
    );
}

#[test]
fn key_token_extraction() {
    assert_eq!(key_token("Ctrl+Shift+T"), "T");
    assert_eq!(key_token("Tab"), "Tab");
    assert_eq!(key_token("Ctrl+"), "");
    assert_eq!(key_token(""), "");
}

#[test]
fn key_token_bindability() {
    assert!(key_token_is_bindable("Tab"));
    assert!(!key_token_is_bindable(""));
    assert!(!key_token_is_bindable("enter"));
    assert!(!key_token_is_bindable("Ctrl"));
}

/// The reverse direction renders the canonical UI spelling.
#[test]
fn accel_to_ui_key_renders_modifiers() {
    assert_eq!(
        accel_to_ui_key("<Control><Shift>t").as_deref(),
        Some("Ctrl+Shift+T")
    );
    assert_eq!(
        accel_to_ui_key("<Control><Alt>Delete").as_deref(),
        Some("Ctrl+Alt+Delete")
    );
    assert_eq!(accel_to_ui_key("<Alt>1").as_deref(), Some("Alt+1"));
    assert_eq!(accel_to_ui_key("F11").as_deref(), Some("F11"));
    assert_eq!(accel_to_ui_key("Delete").as_deref(), Some("Delete"));
}

#[test]
fn accel_to_ui_key_normalizes_modifier_order() {
    // GTK sorts modifiers in accelerator strings; both spellings are
    // the same chord and must render identically.
    assert_eq!(
        accel_to_ui_key("<Shift><Control>t"),
        accel_to_ui_key("<Control><Shift>t")
    );
    assert_eq!(
        accel_to_ui_key("<Shift><Control>t").as_deref(),
        Some("Ctrl+Shift+T")
    );
}

#[test]
fn accel_to_ui_key_renders_letters_upper_case() {
    assert_eq!(accel_to_ui_key("<Control>T").as_deref(), Some("Ctrl+T"));
    assert_eq!(accel_to_ui_key("<Control>t").as_deref(), Some("Ctrl+T"));
    assert_eq!(accel_to_ui_key("<Control>Tab").as_deref(), Some("Ctrl+Tab"));
}

#[test]
fn accel_to_ui_key_rejects_unknown_modifier() {
    assert_eq!(accel_to_ui_key("<Hyper>t"), None);
    assert_eq!(accel_to_ui_key("<Control><Hyper>t"), None);
}

#[test]
fn accel_to_ui_key_rejects_malformed_accelerators() {
    assert_eq!(accel_to_ui_key("<Control"), None);
    assert_eq!(accel_to_ui_key(">>"), None);
    assert_eq!(accel_to_ui_key(""), None);
    assert_eq!(accel_to_ui_key("<Control>"), None, "modifier with no key");
}

#[test]
fn accel_to_ui_key_rejects_keys_gdk_does_not_know() {
    assert_eq!(accel_to_ui_key("bogus"), None);
    assert_eq!(accel_to_ui_key("<Control>Enter"), None);
}

/// Round-tripping preserves the chord, and a canonical key comes back
/// verbatim.
#[test]
fn round_trip_preserves_the_chord() {
    for key in [
        "Ctrl+Shift+T",
        "Ctrl+Shift+W",
        "Ctrl+Tab",
        "Ctrl+Shift+Tab",
        "Alt+1",
        "Alt+9",
        "F11",
        "F2",
        "Delete",
        "Ctrl+E",
        "Ctrl+D",
        "Ctrl+U",
        "Ctrl+Shift+Return",
        "Return",
        "Ctrl+Shift+N",
    ] {
        let accel = ui_key_to_accel(key).unwrap_or_else(|| panic!("{key:?} must parse"));
        let back = accel_to_ui_key(&accel)
            .unwrap_or_else(|| panic!("{accel:?} (from {key:?}) must render"));
        assert_eq!(
            ui_key_to_accel(&back).as_deref(),
            Some(accel.as_str()),
            "{key:?} must round-trip through its accelerator"
        );
        assert_eq!(back, key, "{key:?} is canonical");
    }
}

#[test]
fn round_trip_is_stable_a_second_time() {
    let once = accel_to_ui_key("<Control><Shift>t").unwrap();
    let twice = accel_to_ui_key(&ui_key_to_accel(&once).unwrap()).unwrap();
    assert_eq!(once, twice);
}

/// The accelerator a captured chord binds as: capture stores the UI
/// format, and binding converts it like any other stored key.
fn accel_of(chord: KeyChord) -> Option<String> {
    ui_key_to_accel(&chord.to_ui_key()?)
}

/// Capture side: a `gdk::Key` + modifier mask, i.e. what the settings
/// UI reads off a `gdk::EventKey`.
#[test]
fn chord_renders_ui_key() {
    let chord = KeyChord::new(
        Key::t,
        ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK,
    );
    assert_eq!(chord.to_ui_key().as_deref(), Some("Ctrl+Shift+T"));
}

#[test]
fn chord_renders_accel() {
    let chord = KeyChord::new(
        Key::t,
        ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK,
    );
    assert_eq!(accel_of(chord).as_deref(), Some("<Control><Shift>t"));
}

#[test]
fn chord_without_modifiers() {
    let chord = KeyChord::new(Key::F11, ModifierType::empty());
    assert_eq!(chord.to_ui_key().as_deref(), Some("F11"));
    assert_eq!(accel_of(chord).as_deref(), Some("F11"));
}

#[test]
fn chord_named_key_keeps_case() {
    let chord = KeyChord::new(Key::Tab, ModifierType::CONTROL_MASK);
    assert_eq!(chord.to_ui_key().as_deref(), Some("Ctrl+Tab"));
    assert_eq!(
        KeyChord::new(Key::Delete, ModifierType::empty())
            .to_ui_key()
            .as_deref(),
        Some("Delete")
    );
}

#[test]
fn chord_every_modifier() {
    let all = ModifierType::CONTROL_MASK
        | ModifierType::SHIFT_MASK
        | ModifierType::ALT_MASK
        | ModifierType::SUPER_MASK
        | ModifierType::META_MASK;
    let chord = KeyChord::new(Key::k, all);
    assert_eq!(
        chord.to_ui_key().as_deref(),
        Some("Ctrl+Shift+Alt+Super+Meta+K")
    );
    assert_eq!(
        accel_of(chord).as_deref(),
        Some("<Control><Shift><Alt><Super><Meta>k")
    );
}

#[test]
fn chord_ignores_non_modifier_masks() {
    // A key press during a mouse button1 press still carries that mask.
    let chord = KeyChord::new(
        Key::c,
        ModifierType::CONTROL_MASK | ModifierType::BUTTON1_MASK,
    );
    assert_eq!(chord.to_ui_key().as_deref(), Some("Ctrl+C"));
}

#[test]
fn chord_round_trips_through_ui_key() {
    for (ui_key, expected) in [
        ("Ctrl+Shift+t", "Ctrl+Shift+T"),
        ("Ctrl+Tab", "Ctrl+Tab"),
        ("Alt+1", "Alt+1"),
        ("F2", "F2"),
        ("Delete", "Delete"),
        ("Ctrl+u", "Ctrl+U"),
        ("Ctrl+Shift+Return", "Ctrl+Shift+Return"),
    ] {
        let chord = KeyChord::from_ui_key(ui_key)
            .unwrap_or_else(|| panic!("{ui_key:?} must build a chord from its gdk key"));
        assert_eq!(chord.to_ui_key().as_deref(), Some(expected));
        assert_eq!(accel_of(chord), ui_key_to_accel(ui_key));
        assert_eq!(
            accel_of(KeyChord::from_ui_key(expected).unwrap()),
            accel_of(chord)
        );
    }
}

/// Equal chords compare equal however they arrive: a letter's case,
/// Shift+Tab's `ISO_Left_Tab`, and lock or button masks do not count.
#[test]
fn chords_are_canonical() {
    let ctrl_shift = ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK;
    assert_eq!(
        KeyChord::new(Key::t, ctrl_shift),
        KeyChord::new(Key::T, ctrl_shift | ModifierType::LOCK_MASK)
    );
    assert_eq!(
        KeyChord::new(Key::ISO_Left_Tab, ctrl_shift)
            .to_ui_key()
            .as_deref(),
        Some("Ctrl+Shift+Tab")
    );
    assert_eq!(
        KeyChord::from_ui_key("Shift+Ctrl+t"),
        KeyChord::from_ui_key("Ctrl+Shift+T")
    );
    // A shifted symbol stays as GTK reports it: that is the keyval GTK
    // matches a Shift chord against.
    assert_eq!(
        KeyChord::new(Key::exclam, ctrl_shift)
            .to_ui_key()
            .as_deref(),
        Some("Ctrl+Shift+exclam")
    );
}

#[test]
fn canonical_ui_key_examples() {
    for (key, canonical) in [
        ("Ctrl+Shift+t", Some("Ctrl+Shift+T")),
        ("Shift+Ctrl+T", Some("Ctrl+Shift+T")),
        ("Alt+Ctrl+Delete", Some("Ctrl+Alt+Delete")),
        ("Ctrl+Shift+ISO_Left_Tab", Some("Ctrl+Shift+Tab")),
        ("F11", Some("F11")),
        ("Return", Some("Return")),
        ("Ctrl+Bogus", None),
        ("Enter", None),
        ("Ctrl+", None),
        ("", None),
    ] {
        assert_eq!(canonical_ui_key(key).as_deref(), canonical, "{key:?}");
    }
}

#[test]
fn chord_from_ui_key_rejects_unknown_modifier() {
    assert_eq!(KeyChord::from_ui_key("Hyper+k"), None);
    assert_eq!(KeyChord::from_ui_key("Enter"), None);
}

#[test]
fn bindable_defaults() {
    // Every chord shape the default table uses.
    for key in [
        "Ctrl+Shift+T",
        "Ctrl+Tab",
        "Ctrl+Shift+Tab",
        "Alt+1",
        "F11",
        "Ctrl+Shift+N",
        "Return",
        "Ctrl+Shift+Return",
        "Delete",
        "F2",
        "Ctrl+E",
        "Ctrl+D",
        "Ctrl+U",
        "Ctrl+Shift+V",
    ] {
        assert!(ui_key_is_bindable(key), "{key:?} must be bindable");
    }
}

/// `parse_trigger` returns before touching GTK for a malformed UI
/// format, so this much is testable headless.
#[test]
fn parse_trigger_short_circuits_on_malformed_input() {
    assert!(parse_trigger("Ctrl+").is_none());
    assert!(parse_trigger("Hyper+k").is_none());
    assert!(parse_trigger("").is_none());
}
