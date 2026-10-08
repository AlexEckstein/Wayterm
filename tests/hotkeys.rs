//! Hotkey engine integration tests (doc 07 + doc 06 persistence).
//!
//! The inline suites cover each module in isolation; this file drives
//! the same ground through the *public* API only, the way the UI task
//! and the app will: build an engine from doc 07's table, rebind,
//! persist to a real SQLite file, start again, and get the layout back.
//! Nothing here opens a display — the whole module tree is
//! display-free by design, which is what makes that possible.

use std::collections::HashSet;

use rusqlite::Connection;
use wayterm::hotkeys::{
    accel_to_ui_key, can_co_fire, conflicting_actions, ui_key_is_bindable, ui_key_to_accel,
    ActionContext, ActionRegistry, ConflictPolicy, HotkeyEngine, HotkeyExport, DEFAULT_ACTIONS,
};
use wayterm::store::db::SessionStore;

fn engine() -> HotkeyEngine {
    HotkeyEngine::new()
}

fn file_store(dir: &std::path::Path) -> (SessionStore, Connection) {
    let path = dir.join("wayterm.db");
    let store = SessionStore::open(&path).expect("open the store");
    let raw = Connection::open(&path).expect("second connection");
    (store, raw)
}

fn bindings(engine: &HotkeyEngine) -> Vec<(String, String, bool)> {
    engine
        .registry()
        .ordered_actions()
        .into_iter()
        .map(|a| (a.id.clone(), a.current_key.clone(), a.is_default))
        .collect()
}

#[test]
fn a_new_engine_is_doc07s_table() {
    let engine = engine();
    assert_eq!(engine.registry().len(), DEFAULT_ACTIONS.len());
    assert_eq!(engine.registry().len(), 37);
    for default in DEFAULT_ACTIONS {
        let action = engine
            .action(default.id)
            .unwrap_or_else(|| panic!("{} must be registered", default.id));
        assert_eq!(action.current_key, default.default_key);
        assert_eq!(action.display_name, default.display_name);
        assert_eq!(action.context_str(), default.context.as_str());
        assert!(action.is_default);
        assert!(action.enabled);
    }
}

#[test]
fn every_default_binding_can_actually_fire() {
    for default in DEFAULT_ACTIONS {
        let key = default.default_key;
        assert!(ui_key_is_bindable(key), "{key:?} must be bindable");
        let accel = ui_key_to_accel(key).unwrap();
        let rendered = accel_to_ui_key(&accel)
            .unwrap_or_else(|| panic!("{accel:?} must render for the settings UI"));
        assert_eq!(
            ui_key_to_accel(&rendered).as_deref(),
            Some(accel.as_str()),
            "{key:?} must survive the settings-UI round-trip"
        );
    }
}

/// The doc's four rules, run over the shipped table through the public
/// API: a shipped default must never double-fire.
#[test]
fn the_shipped_table_never_double_fires() {
    let registry = engine().registry().clone();
    for a in DEFAULT_ACTIONS {
        let holders: Vec<&str> = conflicting_actions(&registry, a.default_key, a.context)
            .iter()
            .map(|c| c.id.as_str())
            .filter(|id| *id != a.id)
            .collect();
        for holder in holders {
            let holder_context = registry.action(holder).unwrap().context;
            assert!(
                !can_co_fire(&holder_context, &a.context),
                "{a:?} and {holder:?} share {} and can fire together",
                a.default_key
            );
        }
    }
}

#[test]
fn the_2026_08_collisions_are_stay_resolved() {
    let registry = engine().registry().clone();
    for (a, b) in [
        ("split_vertical", "paste"),
        ("rename_tab", "reconnect"),
        ("toggle_sftp", "search"),
    ] {
        assert_ne!(
            registry.current_key(a),
            registry.current_key(b),
            "{a} and {b} must not share a chord"
        );
    }
}

#[test]
fn doc07s_rule4_example_holds_across_widget_contexts() {
    let registry = engine().registry().clone();
    assert_eq!(registry.current_key("duplicate_session"), Some("Ctrl+D"));
    assert_eq!(registry.current_key("sftp_download"), Some("Ctrl+D"));
    assert_eq!(
        conflicting_actions(&registry, "Ctrl+D", ActionContext::Terminal).len(),
        0,
        "the terminal never holds Ctrl+D"
    );
    assert_eq!(
        conflicting_actions(&registry, "Ctrl+D", ActionContext::Sftp)
            .iter()
            .map(|a| a.id.as_str())
            .collect::<Vec<_>>(),
        vec!["sftp_download"]
    );
}

#[test]
fn rebinding_a_taken_chord_is_refused() {
    let mut engine = engine();
    let err = engine
        .rebind("new_tab", "Ctrl+Shift+C")
        .expect_err("global new_tab must not steal the terminal's copy chord");
    assert!(err.to_string().contains("copy"));
    assert_eq!(
        engine.action("new_tab").unwrap().current_key,
        "Ctrl+Shift+T"
    );
}

#[test]
fn override_then_save_then_load_keeps_the_layout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, _raw) = file_store(dir.path());

    let mut first = engine();
    assert_eq!(
        first
            .rebind_with("paste", "Ctrl+Shift+C", ConflictPolicy::Override)
            .expect("override"),
        vec!["copy".to_string()]
    );
    first.rebind("new_tab", "Ctrl+Alt+T").expect("free chord");
    // Three rows: the two rebinds plus `copy`'s own unbound state, which
    // is what makes the override survive a restart.
    assert_eq!(first.save_to_db(&store).expect("save"), 3);

    let mut second = engine();
    let report = second.load_from_db(&store).expect("load");
    assert_eq!(report.applied.len(), 3);
    assert!(report.skipped.is_empty());
    assert_eq!(second.action("paste").unwrap().current_key, "Ctrl+Shift+C");
    assert_eq!(second.action("new_tab").unwrap().current_key, "Ctrl+Alt+T");
    assert_eq!(second.action("copy").unwrap().current_key, "");
    assert!(!second.action("copy").unwrap().enabled);
}

#[test]
fn seeded_rows_never_load_over_the_defaults() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, raw) = file_store(dir.path());
    // Tamper with the shipped default, the way a hand-edited database
    // would: doc 07 says seed rows are never loaded back.
    raw.execute(
        "UPDATE hotkeys SET key_sequence = 'Ctrl+Alt+Z' WHERE action = 'copy' AND is_default = 1",
        [],
    )
    .expect("tamper");

    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert!(report.applied.is_empty());
    assert_eq!(engine.action("copy").unwrap().current_key, "Ctrl+Shift+C");
}

#[test]
fn a_layout_from_a_newer_build_still_loads() {
    let store = SessionStore::in_memory().expect("in-memory store");
    store
        .save_custom_hotkey("holographic_new_tab", "Ctrl+Alt+N", "global", true)
        .expect("insert");
    store
        .save_custom_hotkey("copy", "Ctrl+Alt+C", "terminal", true)
        .expect("insert");

    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load must not fail");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    assert_eq!(report.skipped, vec!["holographic_new_tab".to_string()]);
    assert_eq!(engine.action("copy").unwrap().current_key, "Ctrl+Alt+C");
}

#[test]
fn a_corrupt_binding_is_skipped_rather_than_stored_dead() {
    let store = SessionStore::in_memory().expect("in-memory store");
    store
        .save_custom_hotkey("copy", "Ctrl+Bogus", "terminal", true)
        .expect("insert");
    store
        .save_custom_hotkey("paste", "Ctrl+Shift+Enter", "terminal", true)
        .expect("insert");

    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert_eq!(report.skipped.len(), 2);
    assert_eq!(engine.action("copy").unwrap().current_key, "Ctrl+Shift+C");
    assert_eq!(engine.action("paste").unwrap().current_key, "Ctrl+Shift+V");
}

/// Doc 06's `hotkeys` table has no `UNIQUE (action, context)`
/// constraint, so the store's delete-then-insert guard is what keeps two
/// writers from stacking duplicate custom rows.
#[test]
fn two_writers_converge_on_one_custom_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (writer_a, raw) = file_store(dir.path());
    let writer_b = writer_a.duplicate().expect("second connection");

    for (index, writer) in [&writer_a, &writer_b].into_iter().enumerate() {
        writer
            .save_custom_hotkey("copy", &format!("Ctrl+Alt+{index}"), "terminal", true)
            .expect("save");
    }

    let rows: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM hotkeys WHERE action = 'copy' AND is_default = 0",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(rows, 1);
}

#[test]
fn reset_to_default_is_persisted_as_row_deletion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, raw) = file_store(dir.path());

    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").expect("rebind");
    engine.save_to_db(&store).expect("save");
    engine.reset_to_default("copy").expect("reset");
    engine.save_to_db(&store).expect("save");

    let custom: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM hotkeys WHERE is_default = 0",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(custom, 0);

    let mut restarted = HotkeyEngine::new();
    restarted.load_from_db(&store).expect("load");
    assert_eq!(
        restarted.action("copy").unwrap().current_key,
        "Ctrl+Shift+C"
    );
    assert!(restarted.action("copy").unwrap().is_default);
}

#[test]
fn a_layout_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, _raw) = file_store(dir.path());

    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").expect("rebind");
    engine.rebind("goto_tab_3", "Alt+F3").expect("rebind");
    engine.set_enabled("sftp_upload", false).expect("disable");
    let before = bindings(&engine);
    engine.save_to_db(&store).expect("save");

    // A fresh process: new registry, same database file.
    let mut restarted = HotkeyEngine::new();
    restarted.load_from_db(&store).expect("load");
    let after = bindings(&restarted);
    assert_eq!(
        before.iter().map(|(_, k, _)| k).collect::<Vec<_>>(),
        after.iter().map(|(_, k, _)| k).collect::<Vec<_>>()
    );
    assert!(!restarted.action("sftp_upload").unwrap().enabled);
}

#[test]
fn export_import_moves_a_layout_between_engines() {
    let mut source = engine();
    source
        .rebind("delete_session", "Shift+Delete")
        .expect("rebind");
    assert_eq!(
        source
            .rebind_with("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
            .expect("override"),
        vec!["copy".to_string()]
    );
    let json = source.export_json().expect("export");

    let mut target = HotkeyEngine::new();
    let report = target.import_json(&json).expect("import");
    // new_tab, the unbound copy, and delete_session.
    assert_eq!(report.applied.len(), 3);
    assert_eq!(report.displaced, vec!["copy".to_string()]);

    assert_eq!(
        target.action("new_tab").unwrap().current_key,
        "Ctrl+Shift+C"
    );
    assert_eq!(
        target.action("delete_session").unwrap().current_key,
        "Shift+Delete"
    );
    assert_eq!(target.action("copy").unwrap().current_key, "");
}

#[test]
fn import_skips_what_this_build_does_not_know() {
    let mut engine = engine();
    let json = r#"{"version":1,"bindings":[
        {"action":"copy","display_name":"Copy Selection","key":"Ctrl+Y","context":"terminal","enabled":true,"is_default":false},
        {"action":"quantum_new_tab","display_name":"Quantum New Tab","key":"Ctrl+Alt+Q","context":"global","enabled":true,"is_default":false}
    ]}"#;
    let report = engine.import_json(json).expect("import");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    assert_eq!(report.skipped, vec!["quantum_new_tab".to_string()]);
    assert!(!engine.export_json().expect("export").contains("quantum"));
}

#[test]
fn an_export_document_is_bindings_only() {
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").expect("rebind");
    let json = engine.export_json().expect("export");
    let export: HotkeyExport = serde_json::from_str(&json).expect("export parses");
    assert_eq!(export.version, 1);
    for binding in &export.bindings {
        // No connection parameters, no credential material — doc 06
        // keeps those out of everything but the keyring.
        let json = serde_json::to_string(binding).expect("serialize");
        for forbidden in ["password", "key_path", "username", "host", "session_id"] {
            assert!(
                !json.contains(forbidden),
                "an exported binding must not mention {forbidden}"
            );
        }
    }
}

#[test]
fn a_registry_can_be_built_by_hand() {
    let mut registry = ActionRegistry::new();
    registry.register(
        "custom",
        "Custom Action",
        "Ctrl+Alt+Shift+P",
        ActionContext::Sftp,
    );
    let mut engine = HotkeyEngine::with_registry(registry);
    assert_eq!(engine.registry().len(), 1);
    assert!(!engine.export_json().expect("export").contains("\"custom\""));
    engine.rebind("custom", "Ctrl+Alt+J").expect("rebind");
    assert!(engine.export_json().expect("export").contains("\"custom\""));
}

#[test]
fn contexts_wire_to_doc06s_vocabulary() {
    let registry = engine().registry().clone();
    let contexts: HashSet<&str> = registry.actions().map(|a| a.context_str()).collect();
    assert_eq!(
        contexts,
        ["global", "terminal", "session_tree", "sftp"]
            .into_iter()
            .collect::<HashSet<_>>()
    );
    for context in [
        ActionContext::Global,
        ActionContext::Terminal,
        ActionContext::SessionTree,
        ActionContext::Sftp,
        ActionContext::RdpTab,
        ActionContext::VncTab,
    ] {
        assert_eq!(
            ActionContext::from_db_str(context.as_str()),
            Some(context),
            "{context:?} must round-trip through the hotkeys.context column"
        );
    }
}
