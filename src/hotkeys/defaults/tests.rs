//! The default table's shipping properties: every key fires, nothing that
//! can fire together shares a chord, and the fixed collisions stay fixed.

use std::collections::HashSet;

use super::*;
use crate::hotkeys::registry::{can_co_fire, conflicting_actions};
use crate::hotkeys::trigger::{
    accel_to_ui_key, canonical_ui_key, ui_key_is_bindable, ui_key_to_accel, KeyChord,
};
use crate::store::SessionStore;

#[test]
fn every_row_is_registered_verbatim_and_ids_are_unique() {
    let registry = default_registry();
    assert_eq!(registry.len(), 34);
    assert_eq!(DEFAULT_ACTIONS.len(), 34);
    for default in DEFAULT_ACTIONS {
        let action = registry.action(default.id).unwrap();
        assert_eq!(action.display_name, default.display_name);
        assert_eq!(action.current_key, default.default_key);
        assert_eq!(action.context, default.context);
        assert!(action.enabled && action.is_default);
    }
    for n in 1..=9 {
        assert_eq!(
            registry.current_key(&format!("goto_tab_{n}")),
            Some(format!("Alt+{n}").as_str())
        );
    }
}

#[test]
fn every_default_key_fires_and_renders_back() {
    for default in DEFAULT_ACTIONS {
        let key = default.default_key;
        assert!(ui_key_is_bindable(key), "{key:?} is not a key GTK resolves");
        let accel = ui_key_to_accel(key).unwrap();
        let rendered = accel_to_ui_key(&accel).unwrap();
        assert_eq!(ui_key_to_accel(&rendered), Some(accel), "{key:?}");
    }
}

/// Doc 11's conflict rules over the whole table: the only repeats are
/// `Ctrl+D` and `Delete`, each shared by the session tree and SFTP.
#[test]
fn defaults_never_double_fire() {
    let registry = default_registry();
    let mut shared = HashSet::new();
    for a in DEFAULT_ACTIONS {
        for holder in conflicting_actions(&registry, a.default_key, a.context) {
            if holder.id != a.id {
                assert!(
                    !can_co_fire(&holder.context, &a.context),
                    "{} and {} share {}",
                    a.id,
                    holder.id,
                    a.default_key
                );
            }
        }
        let repeats = DEFAULT_ACTIONS
            .iter()
            .filter(|b| b.default_key == a.default_key)
            .count();
        if repeats > 1 {
            assert!(a.context.is_focus_context());
            shared.insert(a.default_key);
        }
    }
    assert_eq!(shared, HashSet::from(["Ctrl+D", "Delete"]));
}

/// The collisions the 2026-08 revision fixed must stay fixed.
#[test]
fn revision_collisions_stay_resolved() {
    let registry = default_registry();
    for (a, b) in [("rename_tab", "reconnect"), ("toggle_sftp", "search")] {
        assert_ne!(registry.current_key(a), registry.current_key(b), "{a}/{b}");
    }
}

/// Global and terminal chords are taken before the terminal sees them, so
/// none may be a bare `Ctrl+<letter>`: those are the terminal's control
/// characters (`Ctrl+N` is the shell's next-history, vim's completion).
#[test]
fn no_global_or_terminal_default_takes_a_control_character() {
    for default in DEFAULT_ACTIONS {
        if default.context.is_focus_context() {
            continue;
        }
        let chord = KeyChord::from_ui_key(default.default_key).unwrap();
        let letter = chord
            .key
            .to_unicode()
            .is_some_and(|c| c.is_ascii_alphabetic());
        assert!(
            !(letter && chord.modifiers == gtk4::gdk::ModifierType::CONTROL_MASK),
            "{} takes the control character {}",
            default.id,
            default.default_key
        );
    }
}

/// The table is written canonically, so a captured chord compares equal.
#[test]
fn every_default_key_is_canonical() {
    for default in DEFAULT_ACTIONS {
        assert_eq!(
            canonical_ui_key(default.default_key).as_deref(),
            Some(default.default_key)
        );
    }
}

/// The 2026-10 revision dropped the split actions (no split panes exist)
/// and moved `new_session` off `Ctrl+N`.
#[test]
fn the_2026_10_revision_stays() {
    let registry = default_registry();
    for gone in ["split_horizontal", "split_vertical", "close_split"] {
        assert!(!registry.contains(gone), "{gone}");
    }
    assert_eq!(registry.current_key("new_session"), Some("Ctrl+Shift+N"));
}

/// Doc 09's seed rows are informational (never loaded); this pins their
/// known differences from the table: no `goto_tab_N` rows; `Enter`, which
/// GDK cannot resolve, where the table says `Return`; and the 2026-10
/// revision's dropped split actions and old `Ctrl+N`.
#[test]
fn the_seed_rows_differ_from_the_table_only_as_documented() {
    assert!(!ui_key_is_bindable("Enter") && ui_key_is_bindable("Return"));
    let store = SessionStore::in_memory().unwrap();
    let seeded: Vec<(String, String, String)> = store
        .seeded_hotkeys()
        .unwrap()
        .into_iter()
        .filter(|(action, key, context)| {
            !DEFAULT_ACTIONS
                .iter()
                .any(|d| d.id == action && d.default_key == key && d.context.as_str() == context)
        })
        .collect();
    assert_eq!(
        seeded,
        [
            ("new_session", "Ctrl+N", "global"),
            ("rename_tab", "Ctrl+Shift+Enter", "global"),
            ("split_horizontal", "Ctrl+Shift+D", "terminal"),
            ("split_vertical", "Ctrl+Shift+E", "terminal"),
            ("close_split", "Ctrl+Shift+Q", "terminal"),
            ("connect_session", "Enter", "session_tree"),
        ]
        .map(|(a, k, c)| (a.into(), k.into(), c.into()))
    );
    let seeded_ids = store.seeded_hotkeys().unwrap().len();
    assert_eq!(seeded_ids, DEFAULT_ACTIONS.len() - 9 + 3);
}
