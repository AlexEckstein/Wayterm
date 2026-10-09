//! Hotkey settings UI tests (doc 07 §"Settings UI", §"Conflict
//! Detection", §"Persistence").
//!
//! Everything here is display-free by design: the tests drive
//! `ui::settings::SettingsView` — row mapping, capture formatting,
//! conflict wiring, reset, and the rebind-then-save flows — plus the
//! engine's load path, exactly the way the GTK layer calls them. No
//! test opens a window or initializes GTK.

use wayterm::hotkeys::{
    ActionContext, ActionRegistry, ConflictPolicy, HotkeyEngine, KeyChord, RebindError,
    DEFAULT_ACTIONS,
};
use wayterm::store::db::SessionStore;
use wayterm::ui::settings::{context_label, SettingsError, SettingsRow, SettingsView};

fn engine() -> HotkeyEngine {
    HotkeyEngine::new()
}

fn memory_store() -> SessionStore {
    SessionStore::in_memory().expect("in-memory store")
}

fn chord(ui_key: &str) -> KeyChord {
    KeyChord::from_ui_key(ui_key).expect("test chord must parse")
}

// --- Row mapping -----------------------------------------------------------

#[test]
fn rows_cover_every_registered_action() {
    let engine = engine();
    let rows = SettingsView::rows(&engine);
    assert_eq!(rows.len(), DEFAULT_ACTIONS.len());
    assert_eq!(rows.len(), engine.registry().len());
}

#[test]
fn row_fields_mirror_the_registry_action() {
    let engine = engine();
    let rows = SettingsView::rows(&engine);
    let row = rows.iter().find(|row| row.id == "copy").expect("copy row");
    let action = engine.action("copy").expect("copy action");
    assert_eq!(row.display_name, action.display_name);
    assert_eq!(row.current_key, action.current_key);
    assert_eq!(row.default_key, action.default_key);
    assert_eq!(row.context, action.context);
    assert!(row.is_default);
    assert!(row.enabled);
}

#[test]
fn rows_arrive_in_doc07_table_order() {
    let engine = engine();
    let rows = SettingsView::rows(&engine);
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let mut expected: Vec<&str> = DEFAULT_ACTIONS.iter().map(|a| a.id).collect();
    expected.sort_by_key(|id| {
        let action = engine.action(id).expect("default action registered");
        (action.context, action.id.clone())
    });
    assert_eq!(ids, expected);
}

#[test]
fn row_reflects_a_rebound_current_key() {
    let mut engine = engine();
    engine
        .rebind_with("copy", "Ctrl+Alt+Y", ConflictPolicy::Reject)
        .expect("rebind");
    let row = SettingsView::rows(&engine)
        .into_iter()
        .find(|row| row.id == "copy")
        .expect("copy row");
    assert_eq!(row.current_key, "Ctrl+Alt+Y");
    assert!(!row.is_default);
}

#[test]
fn row_from_action_builds_all_columns() {
    let engine = engine();
    let action = engine.action("sftp_rename").expect("sftp_rename action");
    let row = SettingsRow::from_action(action);
    assert_eq!(row.display_name, "Rename");
    assert_eq!(row.current_key, "F2");
    assert_eq!(row.default_key, "F2");
    assert_eq!(row.context_label, "SFTP");
    assert!(row.is_default);
}

#[test]
fn context_labels_cover_every_context() {
    assert_eq!(context_label(ActionContext::Global), "Global");
    assert_eq!(context_label(ActionContext::Terminal), "Terminal");
    assert_eq!(context_label(ActionContext::SessionTree), "Session Tree");
    assert_eq!(context_label(ActionContext::Sftp), "SFTP");
    assert_eq!(context_label(ActionContext::RdpTab), "RDP Tab");
    assert_eq!(context_label(ActionContext::VncTab), "VNC Tab");
}

#[test]
fn rows_from_registry_matches_engine_rows() {
    let engine = engine();
    let from_engine = SettingsView::rows(&engine);
    let from_registry = SettingsView::rows_from_registry(engine.registry());
    assert_eq!(from_engine, from_registry);
}

#[test]
fn rows_from_custom_registry_follow_that_registry() {
    let mut registry = ActionRegistry::new();
    registry.register("zoom", "Zoom", "Ctrl+Plus", ActionContext::Global);
    let rows = SettingsView::rows_from_registry(&registry);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "zoom");
    assert_eq!(rows[0].context_label, "Global");
}

// --- Capture formatting ----------------------------------------------------

#[test]
fn capture_formats_a_ctrl_shift_chord() {
    assert_eq!(
        SettingsView::capture_label(chord("Ctrl+Shift+T")),
        Some("Ctrl+Shift+t".to_string())
    );
}

#[test]
fn capture_lowercases_a_single_letter_like_the_parser() {
    assert_eq!(
        SettingsView::capture_label(chord("Ctrl+T")),
        Some("Ctrl+t".to_string())
    );
}

#[test]
fn capture_formats_a_bare_function_key() {
    assert_eq!(
        SettingsView::capture_label(chord("F11")),
        Some("F11".to_string())
    );
}

#[test]
fn capture_formats_delete_and_return() {
    assert_eq!(
        SettingsView::capture_label(chord("Delete")),
        Some("Delete".to_string())
    );
    assert_eq!(
        SettingsView::capture_label(chord("Ctrl+Shift+Return")),
        Some("Ctrl+Shift+Return".to_string())
    );
}

#[test]
fn capture_round_trips_through_the_accel_parser() {
    for ui_key in ["Ctrl+Shift+T", "Alt+1", "F11", "Delete", "Ctrl+D"] {
        let rendered = SettingsView::capture_label(chord(ui_key)).expect("renders");
        assert!(
            wayterm::hotkeys::ui_key_is_bindable(&rendered),
            "{rendered} must stay bindable"
        );
    }
}

#[test]
fn escape_is_the_capture_cancel_key() {
    assert!(SettingsView::is_cancel_key(gdk4::Key::Escape));
    assert!(!SettingsView::is_cancel_key(
        gdk4::Key::from_name("t").expect("t key")
    ));
    assert!(!SettingsView::is_cancel_key(
        gdk4::Key::from_name("Return").expect("Return key")
    ));
}

#[test]
fn bare_modifiers_are_not_chords() {
    for key in [
        gdk4::Key::Shift_L,
        gdk4::Key::Shift_R,
        gdk4::Key::Control_L,
        gdk4::Key::Control_R,
        gdk4::Key::Alt_L,
        gdk4::Key::Alt_R,
        gdk4::Key::Super_L,
        gdk4::Key::Super_R,
        gdk4::Key::Caps_Lock,
    ] {
        assert!(
            SettingsView::is_modifier_only(key),
            "{key:?} is modifier-only"
        );
    }
    assert!(!SettingsView::is_modifier_only(
        gdk4::Key::from_name("t").expect("t key")
    ));
    assert!(!SettingsView::is_modifier_only(gdk4::Key::Escape));
}

// --- Conflict wiring -------------------------------------------------------

#[test]
fn conflicts_names_the_loser_row() {
    let engine = engine();
    // `paste` holds Ctrl+Shift+V in the terminal context; aiming `copy`
    // at the same chord must surface it.
    let losers = SettingsView::conflicts(&engine, "copy", "Ctrl+Shift+V");
    assert_eq!(losers.len(), 1);
    assert_eq!(losers[0].id, "paste");
    assert_eq!(losers[0].display_name, "Paste");
}

#[test]
fn conflicts_excludes_the_action_itself() {
    let engine = engine();
    let losers = SettingsView::conflicts(&engine, "copy", "Ctrl+Shift+C");
    assert!(losers.is_empty());
}

#[test]
fn conflicts_ignores_other_tier2_contexts() {
    let engine = engine();
    // `copy` holds Ctrl+Shift+C in the terminal context; aiming the
    // sftp-context `sftp_upload` at the same chord is fine (doc 07
    // rule 4: different Tier-2 contexts never co-fire).
    let losers = SettingsView::conflicts(&engine, "sftp_upload", "Ctrl+Shift+C");
    assert!(losers.is_empty());
}

#[test]
fn conflicts_reports_a_global_against_a_widget_chord() {
    let engine = engine();
    // `new_tab` is global on Ctrl+Shift+T; a terminal action aiming at
    // that chord co-fires (rule 3) and must be flagged.
    let losers = SettingsView::conflicts(&engine, "copy", "Ctrl+Shift+T");
    assert_eq!(losers.len(), 1);
    assert_eq!(losers[0].id, "new_tab");
}

#[test]
fn override_prompt_names_chord_and_loser() {
    let engine = engine();
    let losers = SettingsView::conflicts(&engine, "copy", "Ctrl+Shift+V");
    let prompt = SettingsView::override_prompt("Ctrl+Shift+V", &losers);
    assert_eq!(
        prompt,
        "Ctrl+Shift+V is already bound to 'Paste'. Override?"
    );
}

#[test]
fn override_prompt_joins_several_losers() {
    let losers = vec![
        SettingsRow::from_action(&engine().action("copy").expect("copy").clone()),
        SettingsRow::from_action(&engine().action("paste").expect("paste").clone()),
    ];
    let prompt = SettingsView::override_prompt("Ctrl+Shift+Z", &losers);
    assert!(prompt.starts_with("Ctrl+Shift+Z is already bound to "));
    assert!(prompt.contains("'Copy Selection'"));
    assert!(prompt.contains("'Paste'"));
    assert!(prompt.ends_with("Override?"));
}

#[test]
fn override_clears_the_loser() {
    let mut engine = engine();
    let store = memory_store();
    let displaced = SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Shift+V",
        ConflictPolicy::Override,
    )
    .expect("override applies");
    assert_eq!(displaced, vec!["paste".to_string()]);
    let loser = engine.action("paste").expect("loser still registered");
    assert_eq!(loser.current_key, "");
    assert!(!loser.enabled);
    assert_eq!(
        engine.action("copy").expect("winner").current_key,
        "Ctrl+Shift+V"
    );
}

#[test]
fn decline_leaves_everything_unchanged() {
    let mut engine = engine();
    let before = engine.action("paste").expect("paste").clone();
    let err = engine
        .rebind_with("copy", "Ctrl+Shift+V", ConflictPolicy::Reject)
        .expect_err("reject policy refuses");
    assert!(matches!(err, RebindError::Conflict { .. }));
    assert_eq!(
        engine.action("copy").expect("copy").current_key,
        "Ctrl+Shift+C"
    );
    assert_eq!(engine.action("paste").expect("paste"), &before);
}

// --- Reset -----------------------------------------------------------------

#[test]
fn reset_flips_is_default_back() {
    let mut engine = engine();
    let store = memory_store();
    SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Alt+Y",
        ConflictPolicy::Reject,
    )
    .expect("rebind");
    assert!(!engine.action("copy").expect("copy").is_default);
    SettingsView::reset_action(&mut engine, &store, "copy").expect("reset");
    let action = engine.action("copy").expect("copy");
    assert_eq!(action.current_key, action.default_key);
    assert!(action.is_default);
    assert!(action.enabled);
}

#[test]
fn reset_restores_a_cleared_loser() {
    let mut engine = engine();
    let store = memory_store();
    SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Shift+V",
        ConflictPolicy::Override,
    )
    .expect("override");
    SettingsView::reset_action(&mut engine, &store, "paste").expect("reset loser");
    let loser = engine.action("paste").expect("paste");
    assert_eq!(loser.current_key, "Ctrl+Shift+V");
    assert!(loser.is_default);
    assert!(loser.enabled);
}

// --- Persistence round-trips -----------------------------------------------

#[test]
fn rebind_survives_a_save_and_reload() {
    let mut engine = engine();
    let store = memory_store();
    SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Alt+Y",
        ConflictPolicy::Reject,
    )
    .expect("rebind");
    let mut fresh = HotkeyEngine::new();
    let report = fresh.load_from_db(&store).expect("load");
    assert!(report.applied.contains(&"copy".to_string()));
    assert_eq!(
        fresh.action("copy").expect("copy").current_key,
        "Ctrl+Alt+Y"
    );
}

#[test]
fn reset_deletes_the_row_so_the_default_applies_again() {
    let mut engine = engine();
    let store = memory_store();
    SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Alt+Y",
        ConflictPolicy::Reject,
    )
    .expect("rebind");
    SettingsView::reset_action(&mut engine, &store, "copy").expect("reset");
    let mut fresh = HotkeyEngine::new();
    fresh.load_from_db(&store).expect("load");
    let action = fresh.action("copy").expect("copy");
    assert_eq!(action.current_key, "Ctrl+Shift+C");
    assert!(action.is_default);
}

#[test]
fn override_loser_state_survives_a_reload() {
    let mut engine = engine();
    let store = memory_store();
    SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Shift+V",
        ConflictPolicy::Override,
    )
    .expect("override");
    let mut fresh = HotkeyEngine::new();
    fresh.load_from_db(&store).expect("load");
    let loser = fresh.action("paste").expect("paste");
    assert_eq!(loser.current_key, "");
    assert!(!loser.enabled);
}

// --- Error paths -----------------------------------------------------------

#[test]
fn rebind_of_an_unknown_action_errors_and_saves_nothing() {
    let mut engine = engine();
    let store = memory_store();
    let err = SettingsView::apply_rebind(
        &mut engine,
        &store,
        "no_such_action",
        "Ctrl+Alt+Z",
        ConflictPolicy::Reject,
    )
    .expect_err("unknown action");
    assert!(
        matches!(
            err,
            SettingsError::Rebind(RebindError::UnknownAction { .. })
        ),
        "unexpected: {err:?}"
    );
    assert!(store.load_custom_hotkeys().expect("read rows").is_empty());
}

#[test]
fn rebind_of_an_unparsable_key_errors_and_saves_nothing() {
    let mut engine = engine();
    let store = memory_store();
    let err = SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Bogus",
        ConflictPolicy::Reject,
    )
    .expect_err("unparsable key");
    assert!(
        matches!(
            err,
            SettingsError::Rebind(RebindError::UnparsableKey { .. })
        ),
        "unexpected: {err:?}"
    );
    assert_eq!(
        engine.action("copy").expect("copy").current_key,
        "Ctrl+Shift+C"
    );
    assert!(store.load_custom_hotkeys().expect("read rows").is_empty());
}

#[test]
fn conflicting_rebind_under_reject_errors_and_saves_nothing() {
    let mut engine = engine();
    let store = memory_store();
    let err = SettingsView::apply_rebind(
        &mut engine,
        &store,
        "copy",
        "Ctrl+Shift+V",
        ConflictPolicy::Reject,
    )
    .expect_err("conflict");
    assert!(
        matches!(err, SettingsError::Rebind(RebindError::Conflict { .. })),
        "unexpected: {err:?}"
    );
    assert!(store.load_custom_hotkeys().expect("read rows").is_empty());
}

#[test]
fn reset_of_an_unknown_action_errors() {
    let mut engine = engine();
    let store = memory_store();
    let err = SettingsView::reset_action(&mut engine, &store, "no_such_action")
        .expect_err("unknown action");
    assert!(
        matches!(
            err,
            SettingsError::Rebind(RebindError::UnknownAction { .. })
        ),
        "unexpected: {err:?}"
    );
}

#[test]
fn conflicts_for_an_unknown_action_is_empty() {
    let engine = engine();
    assert!(SettingsView::conflicts(&engine, "no_such_action", "Ctrl+Shift+V").is_empty());
}
