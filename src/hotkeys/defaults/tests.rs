//! The default table's shipping properties: every key fires, nothing that
//! can fire together shares a chord, and the fixed collisions stay fixed.

use std::collections::HashSet;

use super::*;
use crate::hotkeys::registry::{can_co_fire, conflicting_actions};
use crate::hotkeys::trigger::{accel_to_ui_key, ui_key_is_bindable, ui_key_to_accel};
use crate::store::SessionStore;

#[test]
fn every_row_is_registered_verbatim_and_ids_are_unique() {
    let registry = default_registry();
    assert_eq!(registry.len(), 37);
    assert_eq!(DEFAULT_ACTIONS.len(), 37);
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

/// Doc 07's conflict rules over the whole table: the only repeats are
/// `Ctrl+D` and `Delete`, each across two widget contexts.
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
            assert!(a.context.is_widget_context());
            shared.insert(a.default_key);
        }
    }
    assert_eq!(shared, HashSet::from(["Ctrl+D", "Delete"]));
}

/// The collisions the 2026-08 revision fixed must stay fixed.
#[test]
fn revision_collisions_stay_resolved() {
    let registry = default_registry();
    for (a, b) in [
        ("split_vertical", "paste"),
        ("rename_tab", "reconnect"),
        ("toggle_sftp", "search"),
    ] {
        assert_ne!(registry.current_key(a), registry.current_key(b), "{a}/{b}");
    }
}

/// Doc 06's seed rows are informational (never loaded); this pins their
/// known differences from the table: no `goto_tab_N` rows, and `Enter`,
/// which GDK cannot resolve, where the table says `Return`.
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
            ("rename_tab", "Ctrl+Shift+Enter", "global"),
            ("connect_session", "Enter", "session_tree"),
        ]
        .map(|(a, k, c)| (a.into(), k.into(), c.into()))
    );
    let seeded_ids = store.seeded_hotkeys().unwrap().len();
    assert_eq!(seeded_ids, DEFAULT_ACTIONS.len() - 9);
}
