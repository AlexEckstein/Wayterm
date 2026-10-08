//! End-to-end hotkey persistence through the public API: rebind, save to a
//! real SQLite file, start a fresh engine, and get the layout back. The
//! per-rule coverage lives in the `hotkeys` unit tests.

use wayterm::hotkeys::{ConflictPolicy, HotkeyEngine, HotkeyExport};
use wayterm::store::db::SessionStore;

fn file_store(dir: &std::path::Path) -> SessionStore {
    SessionStore::open(&dir.join("wayterm.db")).expect("open the store")
}

fn bindings(engine: &HotkeyEngine) -> Vec<(String, String, bool)> {
    engine
        .registry()
        .ordered_actions()
        .into_iter()
        .map(|a| (a.id.clone(), a.current_key.clone(), a.enabled))
        .collect()
}

#[test]
fn override_then_save_then_load_keeps_the_layout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = file_store(dir.path());

    let mut first = HotkeyEngine::new();
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

    let mut second = HotkeyEngine::new();
    let report = second.load_from_db(&store).expect("load");
    assert_eq!(report.applied.len(), 3);
    assert!(report.skipped.is_empty());
    assert_eq!(second.action("paste").unwrap().current_key, "Ctrl+Shift+C");
    assert_eq!(second.action("new_tab").unwrap().current_key, "Ctrl+Alt+T");
    assert_eq!(second.action("copy").unwrap().current_key, "");
    assert!(!second.action("copy").unwrap().enabled);
}

#[test]
fn a_layout_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = file_store(dir.path());

    let mut engine = HotkeyEngine::new();
    engine.rebind("copy", "Ctrl+Y").expect("rebind");
    engine.rebind("goto_tab_3", "Alt+F3").expect("rebind");
    engine.set_enabled("sftp_upload", false).expect("disable");
    let before = bindings(&engine);
    engine.save_to_db(&store).expect("save");

    // A fresh process: new registry, same database file.
    let mut restarted = HotkeyEngine::new();
    restarted.load_from_db(&store).expect("load");
    assert_eq!(bindings(&restarted), before);
    assert!(!restarted.action("sftp_upload").unwrap().enabled);
}

#[test]
fn an_export_document_is_bindings_only() {
    let mut engine = HotkeyEngine::new();
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
