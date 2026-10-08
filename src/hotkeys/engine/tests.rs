//! Engine tests: the registry wrapper plus the doc 06 `hotkeys`
//! persistence path (doc 07 §"Persistence").
//!
//! Persistence is tested against real SQLite — an in-memory store for
//! the load/save semantics, and a file-backed pair for the two cases
//! that need a second writer or raw SQL: the guard that keeps a rebind
//! from accumulating duplicate `is_default = 0` rows, and the proof that
//! a seeded `is_default = 1` row never loads back over the compiled-in
//! default.

use std::path::Path;

use rusqlite::Connection;

use super::*;
use crate::hotkeys::transfer::HotkeyImportReport;
use crate::store::db::SessionStore;

fn store() -> SessionStore {
    SessionStore::in_memory().expect("in-memory store")
}

fn engine() -> HotkeyEngine {
    HotkeyEngine::new()
}

/// A file-backed store plus a raw second connection to the same file.
fn file_store(dir: &Path) -> (SessionStore, Connection) {
    let path = dir.join("wayterm.db");
    let store = SessionStore::open(&path).expect("open store");
    let raw = Connection::open(&path).expect("raw connection");
    (store, raw)
}

fn custom_rows(store: &SessionStore) -> Vec<(String, String, bool)> {
    store.load_custom_hotkeys().expect("load custom hotkeys")
}

fn binding(engine: &HotkeyEngine, id: &str) -> String {
    engine
        .registry()
        .current_key(id)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn new_engine_starts_from_doc07_defaults() {
    let engine = engine();
    assert_eq!(engine.registry().len(), 37);
    assert_eq!(binding(&engine, "copy"), "Ctrl+Shift+C");
    assert!(engine.action("copy").unwrap().is_default);
    assert_eq!(
        engine.action("new_tab").unwrap().context,
        ActionContext::Global
    );
}

#[test]
fn with_registry_uses_the_callers_registry() {
    let mut registry = ActionRegistry::new();
    registry.register("only", "Only", "Ctrl+Shift+T", ActionContext::Terminal);
    let engine = HotkeyEngine::with_registry(registry);
    assert_eq!(engine.registry().len(), 1);
    assert!(engine.action("only").is_some());
    assert!(engine.action("copy").is_none());
}

#[test]
fn registry_accessors() {
    let mut engine = engine();
    assert_eq!(engine.actions().len(), engine.registry().len());
    engine
        .registry_mut()
        .action_mut("copy")
        .unwrap()
        .display_name = "Copy".into();
    assert_eq!(engine.action("copy").unwrap().display_name, "Copy");
    assert!(engine.action("nope").is_none());
}

#[test]
fn actions_come_back_in_doc_order() {
    let ids: Vec<String> = engine().actions().iter().map(|a| a.id.clone()).collect();
    let globals = ids
        .iter()
        .take_while(|id| {
            engine()
                .action(id)
                .map(|a| a.context == ActionContext::Global)
                .unwrap_or(false)
        })
        .count();
    assert_eq!(globals, 20);
}

#[test]
fn rebind_rejects_a_conflict() {
    let mut engine = engine();
    assert_eq!(
        engine.rebind("new_tab", "Ctrl+Shift+C"),
        Err(RebindError::Conflict {
            key: "Ctrl+Shift+C".to_string(),
            context: ActionContext::Global,
            conflicts: vec!["copy".to_string()],
        })
    );
    assert_eq!(binding(&engine, "new_tab"), "Ctrl+Shift+T", "unchanged");
}

#[test]
fn rebind_with_override_displaces_and_reports() {
    let mut engine = engine();
    let displaced = engine
        .rebind_with("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
        .unwrap();
    assert_eq!(displaced, vec!["copy".to_string()]);
    assert_eq!(binding(&engine, "new_tab"), "Ctrl+Shift+C");
    assert_eq!(binding(&engine, "copy"), "");
}

#[test]
fn rebind_unknown_action() {
    let mut engine = engine();
    assert_eq!(
        engine.rebind("nope", "Ctrl+Alt+Z"),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
    assert_eq!(
        engine.reset_to_default("nope"),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
    assert_eq!(
        engine.set_enabled("nope", false),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
}

#[test]
fn rebind_unparsable_key() {
    let mut engine = engine();
    assert!(matches!(
        engine.rebind("copy", "Ctrl+Nonsense"),
        Err(RebindError::UnparsableKey { .. })
    ));
    assert_eq!(binding(&engine, "copy"), "Ctrl+Shift+C");
}

#[test]
fn reset_and_enable_through_the_engine() {
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").unwrap();
    engine.set_enabled("copy", false).unwrap();
    engine.reset_to_default("copy").unwrap();
    let action = engine.action("copy").unwrap();
    assert!(action.is_default);
    assert!(action.enabled);
}

#[test]
fn conflict_queries_delegate_to_the_registry() {
    let engine = engine();
    assert_eq!(
        engine
            .conflicts_for("paste", "Ctrl+Shift+C")
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        vec!["copy".to_string()]
    );
    assert!(engine
        .conflicts_for("duplicate_session", "Ctrl+D")
        .is_empty());
    assert_eq!(
        engine
            .conflicting_actions("Ctrl+D", ActionContext::Sftp)
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        vec!["sftp_download".to_string()]
    );
    assert!(!engine.can_co_fire(ActionContext::Terminal, ActionContext::Sftp));
    assert!(engine.can_co_fire(ActionContext::Global, ActionContext::Sftp));
    assert!(engine.can_co_fire(ActionContext::Sftp, ActionContext::Sftp));
}

/// A fresh database is full of `is_default = 1` seed rows, and loading
/// must leave every compiled-in default exactly as it was.
#[test]
fn load_from_db_ignores_seed_rows() {
    let store = store();
    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert!(report.applied.is_empty());
    assert!(report.skipped.is_empty());
    assert_eq!(binding(&engine, "copy"), "Ctrl+Shift+C");
    assert_eq!(binding(&engine, "new_tab"), "Ctrl+Shift+T");
}

/// Even a *hand-edited* seed row must not reach the registry: the
/// filter is `is_default = 0`, not "is the row's key still current".
#[test]
fn load_from_db_ignores_edited_seed_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, raw) = file_store(dir.path());
    raw.execute(
        "UPDATE hotkeys SET key_sequence = 'Ctrl+Alt+Z' WHERE action = 'copy' AND is_default = 1",
        [],
    )
    .expect("tamper with the seed");

    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert!(report.applied.is_empty());
    assert_eq!(
        binding(&engine, "copy"),
        "Ctrl+Shift+C",
        "a seed row must never load over the compiled-in default"
    );
}

#[test]
fn load_from_db_applies_custom_rows() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", true)
        .unwrap();
    store
        .save_custom_hotkey("sftp_upload", "Ctrl+Alt+U", "sftp", true)
        .unwrap();

    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert_eq!(report.applied.len(), 2);
    assert!(report.skipped.is_empty());
    assert_eq!(binding(&engine, "copy"), "Ctrl+Y");
    assert_eq!(binding(&engine, "sftp_upload"), "Ctrl+Alt+U");
    assert!(!engine.action("copy").unwrap().is_default);
    assert_eq!(
        engine.action("copy").unwrap().default_key,
        "Ctrl+Shift+C",
        "the compiled-in default is metadata, never overwritten"
    );
}

#[test]
fn load_from_db_skips_unknown_action_ids() {
    let store = store();
    store
        .save_custom_hotkey("no_such_action", "Ctrl+Alt+Z", "global", true)
        .unwrap();
    store
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", true)
        .unwrap();

    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load must not fail");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    assert_eq!(report.skipped, vec!["no_such_action".to_string()]);
    assert_eq!(binding(&engine, "copy"), "Ctrl+Y");
}

#[test]
fn load_from_db_skips_rows_gtk_could_not_fire() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+Bogus", "terminal", true)
        .unwrap();
    store
        .save_custom_hotkey("paste", "Ctrl+Shift+Enter", "terminal", true)
        .unwrap();
    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert!(report.applied.is_empty());
    assert_eq!(
        report.skipped,
        vec!["copy".to_string(), "paste".to_string()]
    );
    assert_eq!(binding(&engine, "copy"), "Ctrl+Shift+C");
    assert_eq!(binding(&engine, "paste"), "Ctrl+Shift+V");
}

/// An override leaves the loser unbound; that empty `key_sequence` is
/// data, not corruption, and must survive a restart.
#[test]
fn load_from_db_restores_an_unbound_disabled_action() {
    let store = store();
    store
        .save_custom_hotkey("copy", "", "terminal", false)
        .unwrap();
    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    let action = engine.action("copy").unwrap();
    assert_eq!(action.current_key, "");
    assert!(!action.enabled);
    assert!(!action.is_bound());
}

#[test]
fn load_from_db_restores_a_disabled_but_bound_action() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", false)
        .unwrap();
    let mut engine = engine();
    engine.load_from_db(&store).expect("load");
    let action = engine.action("copy").unwrap();
    assert_eq!(action.current_key, "Ctrl+Y");
    assert!(!action.enabled);
    assert!(!action.is_bound());
}

#[test]
fn load_from_db_is_idempotent() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", true)
        .unwrap();
    let mut engine = engine();
    let first = engine.load_from_db(&store).expect("load");
    let second = engine.load_from_db(&store).expect("load");
    assert_eq!(first, second);
    assert_eq!(binding(&engine, "copy"), "Ctrl+Y");
}

#[test]
fn save_writes_only_the_deviations() {
    let store = store();
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").unwrap();
    engine.set_enabled("paste", false).unwrap();

    assert_eq!(engine.save_to_db(&store).expect("save"), 2);
    let rows = custom_rows(&store);
    assert_eq!(rows.len(), 2);
    let copy = rows.iter().find(|r| r.0 == "copy").unwrap();
    assert_eq!(copy.1, "Ctrl+Y");
    assert!(copy.2);
    let paste = rows.iter().find(|r| r.0 == "paste").unwrap();
    assert_eq!(
        paste.1, "Ctrl+Shift+V",
        "the chord stays, only enabled flips"
    );
    assert!(!paste.2);
}

#[test]
fn save_removes_the_row_of_a_reset_action() {
    let store = store();
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").unwrap();
    engine.save_to_db(&store).expect("save");
    assert_eq!(custom_rows(&store).len(), 1);

    engine.reset_to_default("copy").unwrap();
    assert_eq!(engine.save_to_db(&store).expect("save"), 0);
    assert!(custom_rows(&store).is_empty());

    let mut fresh = HotkeyEngine::new();
    fresh.load_from_db(&store).expect("load");
    assert_eq!(binding(&fresh, "copy"), "Ctrl+Shift+C");
}

#[test]
fn save_is_idempotent() {
    let store = store();
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").unwrap();
    engine.save_to_db(&store).expect("save");
    engine.save_to_db(&store).expect("save");
    assert_eq!(custom_rows(&store).len(), 1);
}

#[test]
fn save_never_touches_the_seed_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, raw) = file_store(dir.path());
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Y").unwrap();
    engine.save_to_db(&store).expect("save");

    // The seeded row survives beside the custom one: doc 06's design is
    // "is_default = 0 rows override at load time", not "replace the row".
    let count: i64 = raw
        .query_row("SELECT COUNT(*) FROM hotkeys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 29, "28 seeded rows + 1 custom row");
    let defaults: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM hotkeys WHERE is_default = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(defaults, 28, "the seed is untouched");
    let custom: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM hotkeys WHERE is_default = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(custom, 1);
}

/// Doc 06's `hotkeys` table has no `UNIQUE (action, context)`
/// constraint, so a rebind relies on the store's delete-then-insert
/// guard. Two writers through two connections must still converge on one
/// custom row per action.
#[test]
fn two_writers_leave_one_custom_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (writer_a, raw) = file_store(dir.path());
    let writer_b = writer_a.duplicate().expect("second connection");

    writer_a
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", true)
        .unwrap();
    writer_b
        .save_custom_hotkey("copy", "Ctrl+Alt+Y", "terminal", true)
        .unwrap();

    let rows: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM hotkeys WHERE action = 'copy' AND is_default = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "the second write must replace the first");

    // …and the surviving row is one of the two, never a blend.
    let survivors = custom_rows(&writer_a);
    assert_eq!(survivors.len(), 1);
    assert!(
        survivors[0].1 == "Ctrl+Y" || survivors[0].1 == "Ctrl+Alt+Y",
        "unexpected survivor {:?}",
        survivors[0]
    );
}

#[test]
fn delete_custom_hotkey_reports_what_it_removed() {
    let store = store();
    assert_eq!(store.delete_custom_hotkey("copy").unwrap(), 0);
    store
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", true)
        .unwrap();
    assert_eq!(store.delete_custom_hotkey("copy").unwrap(), 1);
    assert!(custom_rows(&store).is_empty());
}

#[test]
fn save_and_load_round_trip_through_a_fresh_engine() {
    let store = store();
    let mut first = engine();
    first.rebind("new_tab", "Ctrl+Alt+T").unwrap();
    // `paste` takes `copy`'s chord and unbindles it…
    assert_eq!(
        first
            .rebind_with("paste", "Ctrl+Shift+C", ConflictPolicy::Override)
            .unwrap(),
        vec!["copy".to_string()]
    );
    // …and `copy` then takes a free chord of its own.
    first.rebind("copy", "Ctrl+Y").unwrap();
    first.save_to_db(&store).expect("save");

    let mut second = engine();
    let report = second.load_from_db(&store).expect("load");
    assert_eq!(report.applied.len(), 3);
    assert_eq!(binding(&second, "new_tab"), "Ctrl+Alt+T");
    assert_eq!(binding(&second, "paste"), "Ctrl+Shift+C");
    assert_eq!(binding(&second, "copy"), "Ctrl+Y");
    assert!(second.action("copy").unwrap().enabled);
    assert!(second.action("copy").unwrap().is_bound());
}

#[test]
fn export_json_only_carries_deviations() {
    let mut engine = engine();
    assert_eq!(
        engine.export_json().unwrap(),
        "{\n  \"version\": 1,\n  \"bindings\": []\n}"
    );
    engine.rebind("copy", "Ctrl+Y").unwrap();
    let json = engine.export_json().unwrap();
    assert!(json.contains("\"copy\""));
    assert!(!json.contains("\"paste\""));
}

#[test]
fn import_json_skips_unknown_ids() {
    let mut engine = engine();
    let json = r#"{"version":1,"bindings":[
        {"action":"copy","display_name":"Copy Selection","key":"Ctrl+Y","context":"terminal","enabled":true,"is_default":false},
        {"action":"from_the_future","display_name":"Future","key":"Ctrl+Alt+F9","context":"global","enabled":true,"is_default":false}
    ]}"#;
    let report: HotkeyImportReport = engine.import_json(json).expect("import");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    assert_eq!(report.skipped, vec!["from_the_future".to_string()]);
    assert_eq!(binding(&engine, "copy"), "Ctrl+Y");
}

#[test]
fn export_import_round_trip() {
    let mut first = engine();
    first.rebind("copy", "Ctrl+Y").unwrap();
    first.set_enabled("paste", false).unwrap();
    let json = first.export_json().unwrap();

    let mut second = engine();
    let report = second.import_json(&json).unwrap();
    assert_eq!(report.applied.len(), 2);
    assert_eq!(binding(&second, "copy"), "Ctrl+Y");
    assert!(!second.action("paste").unwrap().enabled);
    assert_eq!(
        second.export_json().unwrap(),
        json,
        "import then export is a fixed point"
    );
}
