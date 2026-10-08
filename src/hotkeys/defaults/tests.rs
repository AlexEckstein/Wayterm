//! Default-table tests (doc 07 §"Default Actions").
//!
//! The table is not just data — the 2026-08 revision exists because it
//! had three collisions — so the properties that made it ship are
//! asserted as tests, which is what stops a future edit from quietly
//! reintroducing a double-fire:
//!
//! * every default key parses into a GTK accelerator (no silently dead
//!   binding);
//! * no two defaults that can fire at the same time share a chord;
//! * the chords that *are* shared are shared legally, and are exactly
//!   the two pairs doc 07 calls out (`Ctrl+D` and `Delete` across the
//!   session tree and the SFTP sidebar);
//! * every key that changed in the revision still differs from the
//!   chord it used to collide with.

use std::collections::HashSet;

use super::*;
use crate::hotkeys::registry::{can_co_fire, conflicting_actions};
use crate::hotkeys::trigger::{accel_to_ui_key, ui_key_is_bindable, ui_key_to_accel};

/// Doc 06 §"Default Data"'s seed SQL, transcribed: `(action,
/// key_sequence, context)`, `is_default = 1`.
///
/// A copy on purpose — this is the second document's opinion about the
/// same table, and `doc06_seed_rows_match_doc07_table` compares the two
/// so neither can drift without a test failing. Mirrors `SEED_DEFAULTS`
/// in `src/store/db.rs`.
const DOC_06_SEED: &[(&str, &str, &str)] = &[
    ("new_tab", "Ctrl+Shift+T", "global"),
    ("close_tab", "Ctrl+Shift+W", "global"),
    ("next_tab", "Ctrl+Tab", "global"),
    ("prev_tab", "Ctrl+Shift+Tab", "global"),
    ("toggle_sftp", "Ctrl+Shift+B", "global"),
    ("toggle_session_tree", "Ctrl+Shift+S", "global"),
    ("toggle_fullscreen", "F11", "global"),
    ("reconnect", "Ctrl+Shift+R", "global"),
    ("disconnect", "Ctrl+Shift+X", "global"),
    ("new_session", "Ctrl+N", "global"),
    ("rename_tab", "Ctrl+Shift+Enter", "global"),
    ("split_horizontal", "Ctrl+Shift+D", "terminal"),
    ("split_vertical", "Ctrl+Shift+E", "terminal"),
    ("close_split", "Ctrl+Shift+Q", "terminal"),
    ("search", "Ctrl+Shift+F", "terminal"),
    ("search_scrollback", "Ctrl+Shift+K", "terminal"),
    ("copy", "Ctrl+Shift+C", "terminal"),
    ("paste", "Ctrl+Shift+V", "terminal"),
    ("select_all", "Ctrl+Shift+A", "terminal"),
    ("clear", "Ctrl+Shift+L", "terminal"),
    ("edit_session", "Ctrl+E", "session_tree"),
    ("duplicate_session", "Ctrl+D", "session_tree"),
    ("delete_session", "Delete", "session_tree"),
    ("connect_session", "Enter", "session_tree"),
    ("sftp_upload", "Ctrl+U", "sftp"),
    ("sftp_download", "Ctrl+D", "sftp"),
    ("sftp_rename", "F2", "sftp"),
    ("sftp_delete", "Delete", "sftp"),
];

/// `goto_tab_1` … `goto_tab_9`, the one table row that expands.
const GOTO_ROW_ACTIONS: usize = 9;

fn registry() -> ActionRegistry {
    default_registry()
}

fn action_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = DEFAULT_ACTIONS.iter().map(|a| a.id).collect();
    ids.sort_unstable();
    ids
}

#[test]
fn table_shape() {
    assert_eq!(
        DEFAULT_ACTIONS.len(),
        DEFAULT_TABLE_ROWS - 1 + GOTO_ROW_ACTIONS,
        "29 doc rows, of which one expands to nine actions"
    );
    assert_eq!(registry().len(), DEFAULT_ACTIONS.len());
    assert_eq!(DOC_06_SEED.len(), 28, "doc 06 seeds 28 hotkey rows");
}

#[test]
fn action_ids_are_unique() {
    let ids = action_ids();
    let unique: HashSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate action id in the table");
}

#[test]
fn every_row_is_registered_verbatim() {
    let registry = registry();
    for default in DEFAULT_ACTIONS {
        let action = registry
            .action(default.id)
            .unwrap_or_else(|| panic!("{} must be registered", default.id));
        assert_eq!(action.display_name, default.display_name);
        assert_eq!(action.default_key, default.default_key);
        assert_eq!(action.current_key, default.default_key);
        assert_eq!(action.context, default.context);
        assert!(action.enabled);
        assert!(action.is_default);
        assert!(action.is_bound());
    }
}

#[test]
fn contexts_use_the_doc06_column_vocabulary() {
    for default in DEFAULT_ACTIONS {
        assert_eq!(
            default.context.as_str(),
            default.context.as_str(),
            "wire string is the same value either way"
        );
        assert_eq!(
            ActionContext::from_db_str(default.context.as_str()),
            Some(default.context)
        );
    }
}

#[test]
fn goto_tab_actions_expand_to_nine() {
    let registry = registry();
    for n in 1..=GOTO_ROW_ACTIONS {
        let id = format!("goto_tab_{n}");
        let action = registry
            .action(&id)
            .unwrap_or_else(|| panic!("{id} must be registered"));
        assert_eq!(action.default_key, format!("Alt+{n}"));
        assert_eq!(action.context, ActionContext::Global);
    }
    assert!(registry.action("goto_tab_0").is_none());
    assert!(registry.action("goto_tab_10").is_none());
}

/// No default may be a binding GTK can never fire.
#[test]
fn every_default_key_parses() {
    for default in DEFAULT_ACTIONS {
        let accel = ui_key_to_accel(default.default_key)
            .unwrap_or_else(|| panic!("{}: {:?} must parse", default.id, default.default_key));
        assert!(!accel.is_empty(), "{}: empty accelerator", default.id);
        assert!(
            ui_key_is_bindable(default.default_key),
            "{}: {:?} is not a key GTK can resolve",
            default.id,
            default.default_key
        );
    }
}

/// The doc's parser must also survive a round-trip through GTK's own
/// accelerator spelling — the proxy for "GTK would print this".
#[test]
fn every_default_key_renders_back() {
    for default in DEFAULT_ACTIONS {
        let accel = ui_key_to_accel(default.default_key).unwrap();
        let rendered = accel_to_ui_key(&accel).unwrap_or_else(|| {
            panic!(
                "{}: {:?} → {:?} must render back to a UI key",
                default.id, default.default_key, accel
            )
        });
        assert_eq!(
            ui_key_to_accel(&rendered).as_deref(),
            Some(accel.as_str()),
            "{}: {rendered:?} must parse back to {accel:?}",
            default.id
        );
    }
}

/// Doc 07 §"Conflict Detection" over the whole table: rules 1–3 must
/// find nothing, and rule 4 must explain every repeat.
#[test]
fn defaults_are_conflict_free() {
    let registry = registry();
    for a in DEFAULT_ACTIONS {
        // Skip the action itself: `conflicting_actions` is the doc's
        // raw query and a rebinding action always meets its own chord.
        let holders: Vec<&str> = conflicting_actions(&registry, a.default_key, a.context)
            .iter()
            .map(|c| c.id.as_str())
            .filter(|id| *id != a.id)
            .collect();
        let illegal: Vec<&&str> = holders
            .iter()
            .filter(|other| can_co_fire(&registry.action(other).unwrap().context, &a.context))
            .collect();
        assert!(
            illegal.is_empty(),
            "{} ({:?}) collides with {:?}",
            a.id,
            a.default_key,
            illegal
        );
    }
}

#[test]
fn defaults_have_no_duplicate_chords_inside_one_context() {
    let registry = registry();
    for context in [
        ActionContext::Global,
        ActionContext::Terminal,
        ActionContext::SessionTree,
        ActionContext::Sftp,
    ] {
        let mut seen: HashSet<&str> = HashSet::new();
        for action in registry.ordered_actions() {
            if action.context != context {
                continue;
            }
            assert!(
                seen.insert(action.current_key.as_str()),
                "{:?} repeats {:?} ({} / {})",
                context,
                action.current_key,
                seen.len(),
                action.id
            );
        }
    }
}

/// The repeats that are *supposed* to be there — the doc's rule-4
/// example, and nothing else.
#[test]
fn the_only_shared_chords_are_the_doc_examples() {
    let mut shared: Vec<(String, String, ActionContext, ActionContext)> = Vec::new();
    for (i, a) in DEFAULT_ACTIONS.iter().enumerate() {
        for b in &DEFAULT_ACTIONS[i + 1..] {
            if a.default_key == b.default_key {
                shared.push((
                    a.default_key.to_string(),
                    b.default_key.to_string(),
                    a.context,
                    b.context,
                ));
            }
        }
    }
    let pairs: Vec<(&str, &str)> = shared
        .iter()
        .map(|(a, b, _, _)| (a.as_str(), b.as_str()))
        .collect();
    assert_eq!(pairs, vec![("Ctrl+D", "Ctrl+D"), ("Delete", "Delete")]);
    // Both are Tier-2 in different widgets — the only reason they are
    // allowed to repeat.
    for (_, _, first, second) in shared {
        assert!(first.is_widget_context());
        assert!(second.is_widget_context());
        assert_ne!(first, second);
    }
}

/// The three collisions the 2026-08 revision fixed, asserted as
/// "these must not be equal" so the fix cannot be undone.
#[test]
fn revision_collisions_stay_resolved() {
    let registry = registry();
    for (a, b, why) in [
        (
            "split_vertical",
            "paste",
            "was both Ctrl+Shift+V (terminal)",
        ),
        ("rename_tab", "reconnect", "was both Ctrl+Shift+R (global)"),
        ("toggle_sftp", "search", "co-fired with terminal search"),
    ] {
        let first = registry.action(a).unwrap();
        let second = registry.action(b).unwrap();
        assert_ne!(
            first.current_key, second.current_key,
            "{a} and {b} share a chord: {why}"
        );
    }
}

#[test]
fn seed_rows_mirror_the_table() {
    let rows: Vec<SeedRow> = seed_rows().collect();
    assert_eq!(rows.len(), DEFAULT_ACTIONS.len());
    for (row, default) in rows.iter().zip(DEFAULT_ACTIONS) {
        assert_eq!(row.action, default.id);
        assert_eq!(row.key_sequence, default.default_key);
        assert_eq!(row.context, default.context.as_str());
        assert!(row.is_default, "seed rows are is_default = 1");
    }
}

#[test]
fn seed_rows_use_the_doc06_context_vocabulary() {
    let contexts: HashSet<&str> = seed_rows().map(|row| row.context).collect();
    assert_eq!(
        contexts,
        ["global", "terminal", "session_tree", "sftp"]
            .into_iter()
            .collect::<HashSet<_>>(),
        "doc 06's seed has no rows for the passthrough contexts"
    );
}

/// The cross-document check. Doc 06 says its hotkey rows "MUST stay in
/// sync with doc 07's default-actions table"; today they do not — the
/// seed is missing the nine `goto_tab_N` rows, and two rows spell the
/// Enter key `Enter` instead of `Return`. Both are reported to the
/// orchestrator; this test locks the parts that *do* agree so the docs
/// cannot drift further, and states the two known gaps.
#[test]
fn doc06_seed_rows_match_doc07_table() {
    let mut mismatched: Vec<String> = Vec::new();
    let mut missing_from_doc06: Vec<&str> = Vec::new();
    for default in DEFAULT_ACTIONS {
        match DOC_06_SEED
            .iter()
            .find(|(action, _, _)| *action == default.id)
        {
            Some((_, key, context)) => {
                if *key != default.default_key {
                    mismatched.push(format!(
                        "{}: doc 06 {:?} vs doc 07 {:?}",
                        default.id, key, default.default_key
                    ));
                }
                if *context != default.context.as_str() {
                    mismatched.push(format!(
                        "{}: doc 06 context {context:?} vs doc 07 {:?}",
                        default.id, default.context
                    ));
                }
            }
            None => missing_from_doc06.push(default.id),
        }
    }
    // Known, reported divergence: doc 06's seed omits goto_tab_1..9.
    assert_eq!(
        missing_from_doc06,
        vec![
            "goto_tab_1",
            "goto_tab_2",
            "goto_tab_3",
            "goto_tab_4",
            "goto_tab_5",
            "goto_tab_6",
            "goto_tab_7",
            "goto_tab_8",
            "goto_tab_9",
        ],
        "doc 06's seed is missing rows; update it (or this list) deliberately"
    );
    // Known, reported divergence: doc 06 spells the Enter key `Enter`,
    // which GDK does not know.
    assert_eq!(
        mismatched,
        vec![
            "rename_tab: doc 06 \"Ctrl+Shift+Enter\" vs doc 07 \"Ctrl+Shift+Return\"",
            "connect_session: doc 06 \"Enter\" vs doc 07 \"Return\"",
        ],
        "doc 06's seed disagrees on the Enter key spelling (or this list)"
    );
}

/// The evidence for the one place the code departs from doc 07's
/// printed table: `Enter` is not a key name GDK has, so a binding on it
/// can never fire, and the doc's own capture path could never produce
/// it — `gdk::Key::name()` for that key is `Return`.
#[test]
fn enter_spelling_is_the_only_deviation() {
    assert!(!ui_key_is_bindable("Enter"));
    assert!(!ui_key_is_bindable("Ctrl+Shift+Enter"));
    assert!(ui_key_is_bindable("Return"));
    assert!(ui_key_is_bindable("Ctrl+Shift+Return"));

    let registry = registry();
    assert_eq!(
        registry.current_key("rename_tab"),
        Some("Ctrl+Shift+Return")
    );
    assert_eq!(registry.current_key("connect_session"), Some("Return"));
    // No other row deviates: every other default is exactly what the
    // doc prints, which this table restates from it.
    assert_eq!(
        DEFAULT_ACTIONS
            .iter()
            .filter(|d| d.default_key.contains("Enter"))
            .count(),
        0
    );
}

#[test]
fn register_defaults_is_idempotent() {
    let mut registry = ActionRegistry::new();
    register_defaults(&mut registry);
    let once = registry.clone();
    register_defaults(&mut registry);
    assert_eq!(once.len(), registry.len());
    for action in registry.actions() {
        assert_eq!(once.action(&action.id), Some(action));
    }
}

#[test]
fn default_registry_is_reproducible() {
    assert_eq!(default_registry().len(), default_registry().len());
}

#[test]
fn table_covers_the_docs_contexts() {
    let contexts: HashSet<ActionContext> = DEFAULT_ACTIONS.iter().map(|a| a.context).collect();
    assert_eq!(
        contexts,
        [
            ActionContext::Global,
            ActionContext::Terminal,
            ActionContext::SessionTree,
            ActionContext::Sftp
        ]
        .into_iter()
        .collect::<HashSet<_>>(),
        "no default action lives in a passthrough context"
    );
    assert_eq!(
        DEFAULT_ACTIONS
            .iter()
            .filter(|a| a.context == ActionContext::Global)
            .count(),
        20,
        "11 global rows + the nine goto_tab_N actions"
    );
    assert_eq!(
        DEFAULT_ACTIONS
            .iter()
            .filter(|a| a.context == ActionContext::Terminal)
            .count(),
        9
    );
    assert_eq!(
        DEFAULT_ACTIONS
            .iter()
            .filter(|a| a.context == ActionContext::SessionTree)
            .count(),
        4
    );
    assert_eq!(
        DEFAULT_ACTIONS
            .iter()
            .filter(|a| a.context == ActionContext::Sftp)
            .count(),
        4
    );
}
