//! Engine tests: the registry wrapper plus the doc 09 `hotkeys`
//! persistence path (doc 11 §Persistence).
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
use crate::hotkeys::registry::ActionContext;
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
    assert_eq!(engine.registry().len(), 34);
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
fn conflicts_for_delegates_to_the_registry() {
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

/// A stored chord that collides under today's rules takes the chord, as
/// an override would, so no two co-firing actions share it after load.
#[test]
fn load_from_db_resolves_a_collision_like_an_override() {
    let store = store();
    // Terminal copy on the tree's Ctrl+E: legal before terminal shortcuts
    // stayed live in the session tree.
    store
        .save_custom_hotkey("copy", "Ctrl+E", "terminal", true)
        .unwrap();
    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    assert_eq!(report.displaced, vec!["edit_session".to_string()]);
    assert_eq!(binding(&engine, "copy"), "Ctrl+E");
    assert!(!engine.action("edit_session").unwrap().is_bound());
}

/// Keys load canonical: a row that spells a default chord differently is
/// that default again, and the next save drops it.
#[test]
fn load_from_db_canonicalizes_keys() {
    let store = store();
    store
        .save_custom_hotkey("new_tab", "Ctrl+Shift+t", "global", true)
        .unwrap();
    store
        .save_custom_hotkey("copy", "Shift+Ctrl+y", "terminal", true)
        .unwrap();
    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert!(report.displaced.is_empty(), "new_tab displaced nothing");
    assert!(engine.action("new_tab").unwrap().is_default);
    assert_eq!(binding(&engine, "copy"), "Ctrl+Shift+Y");
    engine.save_to_db(&store).unwrap();
    assert_eq!(
        custom_rows(&store),
        vec![("copy".to_string(), "Ctrl+Shift+Y".to_string(), true)]
    );
}

/// A later row can rebind what an earlier one displaced; it is then not
/// reported as displaced.
#[test]
fn load_from_db_reports_only_what_stays_displaced() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+E", "terminal", true)
        .unwrap();
    store
        .save_custom_hotkey("edit_session", "F4", "session_tree", true)
        .unwrap();
    let mut engine = engine();
    let report = engine.load_from_db(&store).expect("load");
    assert!(report.displaced.is_empty());
    assert_eq!(binding(&engine, "edit_session"), "F4");
    assert!(engine.action("edit_session").unwrap().is_bound());
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

    // The seeded row survives beside the custom one: doc 09's design is
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

/// Doc 09's `hotkeys` table has no `UNIQUE (action, context)`
/// constraint, so a rebind relies on the store's delete-then-insert
/// guard. Two writers through two connections must still converge on one
/// custom row per action.
#[test]
fn two_writers_leave_one_custom_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (writer_a, raw) = file_store(dir.path());
    let writer_b = SessionStore::open(&dir.path().join("wayterm.db")).expect("second connection");

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

// ---------------------------------------------------------------------------
// install(): the registry → GTK mapping, and one rebuild per mutation. The
// GTK binder itself is exercised under xvfb in `tests/hotkeys.rs`; here a
// recorder stands in for it so the "which bindings were pushed" contract
// runs display-free.
// ---------------------------------------------------------------------------

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::hotkeys::defaults::DEFAULT_ACTIONS;

/// Records every action pushed to it as (id, live accel).
#[derive(Debug, Default)]
struct Recorder {
    pushed: RefCell<Vec<(String, Option<String>)>>,
    uninstalled: Cell<bool>,
}

impl Binder for Recorder {
    fn bind(&self, action: &Action) {
        self.pushed
            .borrow_mut()
            .push((action.id.clone(), live_accel(action)));
    }

    fn uninstall(&self) {
        self.uninstalled.set(true);
    }
}

impl Recorder {
    /// What was pushed since the last call.
    fn take(&self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut self.pushed.borrow_mut())
    }
}

fn installed() -> (HotkeyEngine, Rc<Recorder>) {
    let mut engine = engine();
    let recorder = Rc::new(Recorder::default());
    engine.install_binder(recorder.clone());
    recorder.take();
    (engine, recorder)
}

fn pushed(id: &str, accel: Option<&str>) -> (String, Option<String>) {
    (id.to_string(), accel.map(str::to_string))
}

/// Narrow contexts are tried first, globals last; the passthrough
/// contexts hold no actions at all.
#[test]
fn precedence_runs_panels_then_terminal_then_global() {
    use ActionContext::*;
    assert_eq!(precedence(SessionTree), precedence(Sftp));
    assert!(precedence(Sftp) < precedence(Terminal));
    assert!(precedence(Terminal) < precedence(Global));
    assert!(DEFAULT_ACTIONS
        .iter()
        .all(|d| !matches!(d.context, RdpTab | VncTab)));
}

#[test]
fn every_action_targets_its_window_action() {
    assert_eq!(action_name("copy"), "win.copy");
    assert_eq!(action_name("toggle_sftp"), "win.toggle_sftp");
}

/// The engine must give `toggle_sftp` its documented default accel.
#[test]
fn toggle_sftp_gets_its_default_accel() {
    let engine = engine();
    assert_eq!(
        live_accel(engine.action("toggle_sftp").unwrap()).as_deref(),
        Some("<Control><Shift>b")
    );
}

#[test]
fn every_default_binding_converts_to_an_accel() {
    let engine = engine();
    for action in engine.registry().actions() {
        assert!(live_accel(action).is_some(), "{} has no accel", action.id);
    }
}

#[test]
fn disabled_and_unbound_actions_hold_no_accel() {
    let mut engine = engine();
    engine.set_enabled("copy", false).unwrap();
    assert_eq!(live_accel(engine.action("copy").unwrap()), None);
    engine
        .rebind_with("new_tab", "Ctrl+Shift+V", ConflictPolicy::Override)
        .unwrap();
    assert_eq!(live_accel(engine.action("paste").unwrap()), None);
}

#[test]
fn install_pushes_every_action_once() {
    let mut engine = engine();
    let recorder = Rc::new(Recorder::default());
    engine.install_binder(recorder.clone());
    let mut ids: Vec<String> = recorder.take().into_iter().map(|(id, _)| id).collect();
    ids.sort();
    let mut expected: Vec<String> = DEFAULT_ACTIONS.iter().map(|d| d.id.to_string()).collect();
    expected.sort();
    assert_eq!(ids, expected);
}

#[test]
fn reinstall_uninstalls_the_previous_binder() {
    let (mut engine, first) = installed();
    let second = Rc::new(Recorder::default());
    engine.install_binder(second.clone());
    assert!(first.uninstalled.get());
    assert!(!second.uninstalled.get());
    assert_eq!(second.take().len(), DEFAULT_ACTIONS.len());
    engine.rebind("copy", "Ctrl+Alt+C").unwrap();
    assert!(
        first.take().is_empty(),
        "the old binder sees no more changes"
    );
    assert_eq!(second.take().len(), 1);
}

#[test]
fn rebind_rebuilds_only_that_binding() {
    let (mut engine, recorder) = installed();
    engine.rebind("copy", "Ctrl+Alt+C").unwrap();
    assert_eq!(
        recorder.take(),
        vec![pushed("copy", Some("<Control><Alt>c"))]
    );
    engine.rebind("new_tab", "Ctrl+Alt+T").unwrap();
    assert_eq!(
        recorder.take(),
        vec![pushed("new_tab", Some("<Control><Alt>t"))]
    );
}

#[test]
fn an_override_rebuilds_the_winner_and_the_losers() {
    let (mut engine, recorder) = installed();
    engine
        .rebind_with("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
        .unwrap();
    assert_eq!(
        recorder.take(),
        vec![
            pushed("new_tab", Some("<Control><Shift>c")),
            pushed("copy", None),
        ]
    );
}

#[test]
fn a_refused_rebind_rebuilds_nothing() {
    let (mut engine, recorder) = installed();
    assert!(engine.rebind("new_tab", "Ctrl+Shift+C").is_err());
    assert!(engine.rebind("copy", "Ctrl+Nonsense").is_err());
    assert!(engine.rebind("nope", "Ctrl+Alt+Z").is_err());
    assert!(engine.reset_to_default("nope").is_err());
    assert!(engine.set_enabled("nope", false).is_err());
    assert!(recorder.take().is_empty());
}

#[test]
fn reset_and_enable_rebuild_only_that_binding() {
    let (mut engine, recorder) = installed();
    engine.rebind("copy", "Ctrl+Alt+C").unwrap();
    recorder.take();

    engine.set_enabled("copy", false).unwrap();
    assert_eq!(recorder.take(), vec![pushed("copy", None)]);
    engine.set_enabled("copy", true).unwrap();
    assert_eq!(
        recorder.take(),
        vec![pushed("copy", Some("<Control><Alt>c"))]
    );
    engine.reset_to_default("copy").unwrap();
    assert_eq!(
        recorder.take(),
        vec![pushed("copy", Some("<Control><Shift>c"))]
    );
}

#[test]
fn load_from_db_rebuilds_what_it_displaced() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+E", "terminal", true)
        .unwrap();
    let (mut engine, recorder) = installed();
    engine.load_from_db(&store).expect("load");
    assert_eq!(
        recorder.take(),
        vec![
            pushed("copy", Some("<Control>e")),
            pushed("edit_session", None)
        ]
    );
}

#[test]
fn load_from_db_rebuilds_the_applied_rows() {
    let store = store();
    store
        .save_custom_hotkey("copy", "Ctrl+Y", "terminal", true)
        .unwrap();
    store
        .save_custom_hotkey("no_such_action", "Ctrl+Alt+Z", "global", true)
        .unwrap();
    let (mut engine, recorder) = installed();
    engine.load_from_db(&store).expect("load");
    assert_eq!(recorder.take(), vec![pushed("copy", Some("<Control>y"))]);
}

#[test]
fn import_json_rebuilds_what_it_displaced() {
    let (mut engine, recorder) = installed();
    let json = r#"{"version":1,"bindings":[
        {"action":"new_tab","display_name":"New Tab","key":"Ctrl+Shift+C","context":"global","enabled":true,"is_default":false}
    ]}"#;
    engine.import_json(json).expect("import");
    let pushed = recorder.take();
    assert!(pushed.contains(&("new_tab".to_string(), Some("<Control><Shift>c".to_string()))));
    assert!(pushed.contains(&("copy".to_string(), None)));
}

#[test]
fn an_uninstalled_engine_still_mutates() {
    let mut engine = engine();
    engine.rebind("copy", "Ctrl+Alt+C").unwrap();
    assert_eq!(binding(&engine, "copy"), "Ctrl+Alt+C");
}
