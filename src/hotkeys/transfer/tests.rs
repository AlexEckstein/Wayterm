//! Export/import tests (doc 07 §"Persistence", last paragraph).
//!
//! The point of the file format is that a layout can move between
//! machines, so these tests care about two things: a round-trip must
//! reproduce the layout exactly, and anything the importing build does
//! not know about must be skipped rather than fatal — a document
//! written by a newer Wayterm has to degrade, not explode.

use super::*;
use crate::hotkeys::defaults::default_registry;
use crate::hotkeys::registry::ActionRegistry;

fn registry() -> ActionRegistry {
    default_registry()
}

fn entry(action: &str, key: &str, context: &str) -> String {
    format!(
        r#"{{"action":"{action}","display_name":"{action}","key":"{key}","context":"{context}","enabled":true,"is_default":false}}"#
    )
}

fn document(entries: &[String]) -> String {
    format!(r#"{{"version":1,"bindings":[{}]}}"#, entries.join(","))
}

fn bindings(json: &str) -> Vec<String> {
    let export: HotkeyExport = serde_json::from_str(json).expect("export parses");
    export.bindings.into_iter().map(|b| b.action).collect()
}

#[test]
fn an_untouched_registry_exports_nothing() {
    let json = export_json(&registry()).expect("export");
    assert!(bindings(&json).is_empty());
    assert!(json.contains("\"version\": 1"));
}

#[test]
fn export_carries_only_deviations() {
    let mut registry = registry();
    registry.rebind("copy", "Ctrl+Y").unwrap();
    registry.set_enabled("paste", false).unwrap();
    let json = export_json(&registry).expect("export");
    assert_eq!(bindings(&json), vec!["copy", "paste"]);
}

#[test]
fn export_entries_carry_the_full_binding() {
    let mut registry = registry();
    registry.rebind("copy", "Ctrl+Y").unwrap();
    let export: HotkeyExport = serde_json::from_str(&export_json(&registry).unwrap()).unwrap();
    assert_eq!(export.version, HOTKEY_EXPORT_VERSION);
    let binding = &export.bindings[0];
    assert_eq!(binding.action, "copy");
    assert_eq!(binding.display_name, "Copy Selection");
    assert_eq!(binding.key, "Ctrl+Y");
    assert_eq!(binding.context, "terminal");
    assert!(binding.enabled);
    assert!(!binding.is_default);
}

#[test]
fn export_preserves_table_order() {
    let mut registry = registry();
    registry.rebind("paste", "Ctrl+Y").unwrap();
    registry.rebind("copy", "Ctrl+Shift+J").unwrap();
    assert_eq!(
        bindings(&export_json(&registry).unwrap()),
        vec!["copy", "paste"]
    );
}

#[test]
fn export_of_an_unbound_action() {
    let mut registry = registry();
    registry
        .rebind_checked("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
        .unwrap();
    let export: HotkeyExport = serde_json::from_str(&export_json(&registry).unwrap()).unwrap();
    let copy = export.bindings.iter().find(|b| b.action == "copy").unwrap();
    assert_eq!(copy.key, "");
    assert!(!copy.enabled);
    assert!(!copy.is_default);
}

#[test]
fn import_round_trip() {
    let mut first = registry();
    first.rebind("copy", "Ctrl+Y").unwrap();
    first.rebind("new_tab", "Ctrl+Alt+T").unwrap();
    first.set_enabled("sftp_upload", false).unwrap();
    let json = export_json(&first).unwrap();

    let mut second = registry();
    let report = import_json(&mut second, &json).expect("import");
    assert_eq!(report.applied.len(), 3);
    assert!(report.skipped.is_empty());
    assert!(report.displaced.is_empty());
    assert_eq!(second.current_key("copy"), Some("Ctrl+Y"));
    assert_eq!(second.current_key("new_tab"), Some("Ctrl+Alt+T"));
    assert!(!second.action("sftp_upload").unwrap().enabled);
    assert_eq!(export_json(&second).unwrap(), json);
}

#[test]
fn import_skips_unknown_action_ids() {
    let mut registry = registry();
    let json = document(&[
        entry("copy", "Ctrl+Y", "terminal"),
        entry("from_the_future", "Ctrl+Alt+F9", "global"),
    ]);
    let report = import_json(&mut registry, &json).expect("import");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    assert_eq!(report.skipped, vec!["from_the_future".to_string()]);
    assert_eq!(registry.current_key("copy"), Some("Ctrl+Y"));
}

#[test]
fn import_skips_a_wrong_context() {
    let mut registry = registry();
    // `copy` is a terminal action; a document claiming otherwise is
    // stale or hand-edited, and its conflicts were never resolved.
    let json = document(&[entry("copy", "Ctrl+Y", "global")]);
    let report = import_json(&mut registry, &json).expect("import");
    assert_eq!(report.skipped, vec!["copy".to_string()]);
    assert_eq!(registry.current_key("copy"), Some("Ctrl+Shift+C"));
}

#[test]
fn import_skips_an_unbindable_key() {
    let mut registry = registry();
    let json = document(&[entry("copy", "Ctrl+Bogus", "terminal")]);
    let report = import_json(&mut registry, &json).expect("import");
    assert_eq!(report.skipped, vec!["copy".to_string()]);
    assert_eq!(registry.current_key("copy"), Some("Ctrl+Shift+C"));
}

#[test]
fn import_overrides_conflicts_and_reports_them() {
    let mut registry = registry();
    let json = document(&[entry("new_tab", "Ctrl+Shift+C", "global")]);
    let report = import_json(&mut registry, &json).expect("import");
    assert_eq!(report.applied, vec!["new_tab".to_string()]);
    assert_eq!(report.displaced, vec!["copy".to_string()]);
    assert_eq!(registry.current_key("new_tab"), Some("Ctrl+Shift+C"));
    assert_eq!(registry.current_key("copy"), Some(""));
}

#[test]
fn import_applies_an_unbind() {
    let mut registry = registry();
    let json = r#"{"version":1,"bindings":[
        {"action":"copy","display_name":"Copy Selection","key":"","context":"terminal","enabled":false,"is_default":false}
    ]}"#;
    let report = import_json(&mut registry, json).expect("import");
    assert_eq!(report.applied, vec!["copy".to_string()]);
    let action = registry.action("copy").unwrap();
    assert_eq!(action.current_key, "");
    assert!(!action.enabled);
}

#[test]
fn import_of_an_empty_document_changes_nothing() {
    let mut registry = registry();
    let before = export_json(&registry).unwrap();
    let report = import_json(&mut registry, r#"{"version":1,"bindings":[]}"#).expect("import");
    assert!(report.applied.is_empty());
    assert_eq!(export_json(&registry).unwrap(), before);
}

#[test]
fn import_reports_malformed_json() {
    let mut registry = registry();
    assert!(import_json(&mut registry, "not json").is_err());
    assert!(import_json(&mut registry, "{}").is_err());
}

#[test]
fn import_tolerates_an_unknown_document_version() {
    // Forward compatibility: extra fields are ignored, unknown action
    // ids are skipped, so a newer document partially applies.
    let json = r#"{"version":99,"future_field":true,"bindings":[
        {"action":"copy","display_name":"Copy Selection","key":"Ctrl+Y","context":"terminal","enabled":true,"is_default":false,"future":1}
    ]}"#;
    let mut registry = registry();
    let report = import_json(&mut registry, json).expect("import");
    assert_eq!(report.applied, vec!["copy".to_string()]);
}

#[test]
fn import_applies_to_an_empty_registry() {
    let mut registry = ActionRegistry::new();
    registry.register("only", "Only", "F5", ActionContext::Sftp);
    let json = document(&[entry("only", "F6", "sftp")]);
    let report = import_json(&mut registry, &json).expect("import");
    assert_eq!(report.applied, vec!["only".to_string()]);
    assert_eq!(registry.current_key("only"), Some("F6"));
}

#[test]
fn exported_binding_is_ordinary_serde() {
    let binding = ExportedBinding {
        action: "copy".to_string(),
        display_name: "Copy Selection".to_string(),
        key: "Ctrl+Y".to_string(),
        context: "terminal".to_string(),
        enabled: true,
        is_default: false,
    };
    let json = serde_json::to_string(&binding).unwrap();
    assert!(json.contains("\"action\":\"copy\""));
    assert_eq!(
        serde_json::from_str::<ExportedBinding>(&json).unwrap(),
        binding
    );
}
