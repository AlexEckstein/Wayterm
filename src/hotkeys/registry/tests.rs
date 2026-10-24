//! Action registry + conflict-detection tests (doc 11 §Action
//! registry, §Conflict detection).
//!
//! The five conflict rules are asserted individually *and* as a full
//! context matrix, because the de-confliction of the default table
//! rests on rule 5 (the session tree and SFTP never co-fire) being exactly
//! as narrow as the doc says — one extra `||` in [`can_co_fire`] would
//! silently re-introduce a collision.

use super::*;

fn registry_with_defaults() -> ActionRegistry {
    crate::hotkeys::defaults::default_registry()
}

fn ids<'a>(actions: impl IntoIterator<Item = &'a Action>) -> Vec<String> {
    let mut ids: Vec<String> = actions.into_iter().map(|a| a.id.clone()).collect();
    ids.sort();
    ids
}

#[test]
fn register_populates_the_doc_fields() {
    let mut registry = ActionRegistry::new();
    registry.register("new_tab", "New Tab", "Ctrl+Shift+T", ActionContext::Global);
    let action = registry.action("new_tab").expect("registered");
    assert_eq!(action.id, "new_tab");
    assert_eq!(action.display_name, "New Tab");
    assert_eq!(action.default_key, "Ctrl+Shift+T");
    assert_eq!(
        action.current_key, "Ctrl+Shift+T",
        "doc: insert with current_key = default_key"
    );
    assert_eq!(action.context, ActionContext::Global);
    assert!(action.enabled);
    assert!(action.is_default);
    assert_eq!(action.context.as_str(), "global");
}

#[test]
fn register_replaces_an_existing_id() {
    let mut registry = ActionRegistry::new();
    registry.register("copy", "Copy", "Ctrl+Shift+C", ActionContext::Terminal);
    registry.register(
        "copy",
        "Copy Selection",
        "Ctrl+Shift+C",
        ActionContext::Terminal,
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry.action("copy").unwrap().display_name,
        "Copy Selection"
    );
}

#[test]
fn empty_registry() {
    let registry = ActionRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(!registry.contains("new_tab"));
    assert!(registry.action("new_tab").is_none());
    assert_eq!(registry.current_key("new_tab"), None);
    assert_eq!(registry.context_of("new_tab"), None);
}

/// Doc 11's `rebind`, transcribed: swap the key, flip the flag.
#[test]
fn rebind_sets_key_and_clears_is_default() {
    let mut registry = registry_with_defaults();
    registry.rebind("copy", "Ctrl+Y").unwrap();
    let action = registry.action("copy").unwrap();
    assert_eq!(action.current_key, "Ctrl+Y");
    assert_eq!(action.default_key, "Ctrl+Shift+C", "default is metadata");
    assert!(!action.is_default);
}

#[test]
fn rebind_back_to_default_restores_is_default() {
    let mut registry = registry_with_defaults();
    registry.rebind("copy", "Ctrl+Y").unwrap();
    registry.rebind("copy", "Ctrl+Shift+C").unwrap();
    assert!(registry.action("copy").unwrap().is_default);
}

#[test]
fn rebind_unknown_action_errors() {
    let mut registry = registry_with_defaults();
    assert_eq!(
        registry.rebind("nope", "Ctrl+K"),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
    assert!(registry.rebind("nope", "Ctrl+K").is_err());
}

#[test]
fn rebind_checked_rejects_unknown_action() {
    let mut registry = registry_with_defaults();
    assert_eq!(
        registry.rebind_checked("nope", "Ctrl+K", ConflictPolicy::Reject),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
}

#[test]
fn rebind_checked_rejects_unparsable_keys() {
    let mut registry = registry_with_defaults();
    for key in ["Ctrl+Bogus", "Ctrl+", "Hyper+k", "", "Ctrl+Enter"] {
        let err = registry
            .rebind_checked("copy", key, ConflictPolicy::Reject)
            .expect_err("must reject");
        assert!(
            matches!(err, RebindError::UnparsableKey { .. }),
            "{key:?} must be UnparsableKey, got {err:?}"
        );
    }
    assert_eq!(
        registry.action("copy").unwrap().current_key,
        "Ctrl+Shift+C",
        "a rejected rebind must not mutate"
    );
}

#[test]
fn rebind_checked_accepts_a_free_chord() {
    let mut registry = registry_with_defaults();
    assert_eq!(
        registry
            .rebind_checked("copy", "Ctrl+Shift+Y", ConflictPolicy::Reject)
            .unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(registry.current_key("copy"), Some("Ctrl+Shift+Y"));
}

/// Rule 3: a global and a context action with the same chord both fire
/// while that context is active, so that is a conflict.
#[test]
fn rule3_global_and_widget_context_conflict() {
    let mut registry = registry_with_defaults();
    let err = registry
        .rebind_checked("new_tab", "Ctrl+Shift+C", ConflictPolicy::Reject)
        .expect_err("global new_tab taking terminal copy's chord must conflict");
    assert_eq!(
        err,
        RebindError::Conflict {
            key: "Ctrl+Shift+C".to_string(),
            context: ActionContext::Global,
            conflicts: vec!["copy".to_string()],
        }
    );
}

#[test]
fn rule1_two_globals_conflict() {
    let mut registry = registry_with_defaults();
    let err = registry
        .rebind_checked("new_tab", "Ctrl+Shift+N", ConflictPolicy::Reject)
        .expect_err("two globals on one chord");
    assert_eq!(
        err,
        RebindError::Conflict {
            key: "Ctrl+Shift+N".to_string(),
            context: ActionContext::Global,
            conflicts: vec!["new_session".to_string()],
        }
    );
}

#[test]
fn rule2_same_context_conflicts() {
    let mut registry = registry_with_defaults();
    let err = registry
        .rebind_checked("paste", "Ctrl+Shift+C", ConflictPolicy::Reject)
        .expect_err("two terminal actions on one chord");
    assert_eq!(
        err,
        RebindError::Conflict {
            key: "Ctrl+Shift+C".to_string(),
            context: ActionContext::Terminal,
            conflicts: vec!["copy".to_string()],
        }
    );
}

/// Rule 4: terminal shortcuts stay live while the session tree or the
/// SFTP panel has focus, so a terminal chord collides with theirs.
#[test]
fn rule4_terminal_and_panel_contexts_conflict() {
    let mut registry = registry_with_defaults();
    let err = registry
        .rebind_checked("copy", "Ctrl+E", ConflictPolicy::Reject)
        .expect_err("terminal copy taking the tree's edit chord");
    assert_eq!(
        err,
        RebindError::Conflict {
            key: "Ctrl+E".to_string(),
            context: ActionContext::Terminal,
            conflicts: vec!["edit_session".to_string()],
        }
    );
    assert_eq!(
        ids(registry.conflicts_for("sftp_rename", "Ctrl+Shift+V")),
        vec!["paste".to_string()]
    );
}

/// Rule 5: `Ctrl+D` is `duplicate_session` in the tree and
/// `sftp_download` in the sidebar. Focus is in one panel at most — the
/// doc's own worked example, and the reason the shipped table may hold
/// the same chord twice.
#[test]
fn rule5_tree_and_sftp_do_not_conflict() {
    let registry = registry_with_defaults();
    assert_eq!(registry.current_key("duplicate_session"), Some("Ctrl+D"));
    assert_eq!(registry.current_key("sftp_download"), Some("Ctrl+D"));

    // Each context sees only its own holder.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+D",
            ActionContext::SessionTree
        )),
        vec!["duplicate_session".to_string()]
    );
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+D",
            ActionContext::Sftp
        )),
        vec!["sftp_download".to_string()]
    );
    // The terminal co-fires with both panels (rule 4), so it sees both.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+D",
            ActionContext::Terminal
        )),
        vec!["duplicate_session".to_string(), "sftp_download".to_string()]
    );
    // An action never conflicts with the chord it already holds.
    assert!(registry
        .conflicts_for("duplicate_session", "Ctrl+D")
        .is_empty());
}

#[test]
fn rule5_delete_in_tree_and_sftp_is_legal() {
    let registry = registry_with_defaults();
    assert_eq!(registry.current_key("delete_session"), Some("Delete"));
    assert_eq!(registry.current_key("sftp_delete"), Some("Delete"));

    // Rule 5: from either panel, only its own holder shows up.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Delete",
            ActionContext::SessionTree
        )),
        vec!["delete_session".to_string()]
    );
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Delete",
            ActionContext::Sftp
        )),
        vec!["sftp_delete".to_string()]
    );
    // Rule 3: a *global* action taking that chord collides with both,
    // because a global fires while either panel has focus.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Delete",
            ActionContext::Global
        )),
        vec!["delete_session".to_string(), "sftp_delete".to_string()]
    );
}

#[test]
fn can_co_fire_matrix() {
    use ActionContext::*;
    // (a, b, expected) — doc rules 1–4 collapse into "either side is
    // global, the contexts are equal, or a terminal meets a panel";
    // rule 5 is everything else.
    let cases = [
        (Global, Global, true),
        (Global, Terminal, true),
        (Global, SessionTree, true),
        (Global, Sftp, true),
        (Global, RdpTab, true),
        (Global, VncTab, true),
        (Terminal, Global, true),
        (SessionTree, Global, true),
        (Sftp, Global, true),
        (Terminal, Terminal, true),
        (SessionTree, SessionTree, true),
        (Sftp, Sftp, true),
        (RdpTab, RdpTab, true),
        (VncTab, VncTab, true),
        (Terminal, SessionTree, true),
        (Terminal, Sftp, true),
        (SessionTree, Sftp, false),
        (Terminal, RdpTab, false),
        (Sftp, VncTab, false),
        (RdpTab, VncTab, false),
    ];
    for (a, b, expected) in cases {
        assert_eq!(can_co_fire(&a, &b), expected, "{a:?} vs {b:?}");
        assert_eq!(
            can_co_fire(&b, &a),
            expected,
            "{a:?} vs {b:?} must be symmetric"
        );
    }
}

#[test]
fn can_co_fire_is_reflexive_and_symmetric() {
    for a in ActionContext::ALL {
        assert!(can_co_fire(&a, &a));
        for b in ActionContext::ALL {
            assert_eq!(can_co_fire(&a, &b), can_co_fire(&b, &a));
        }
    }
}

/// Doc 11: "overriding clears (unbinds) the loser".
#[test]
fn override_unbinds_the_loser() {
    let mut registry = registry_with_defaults();
    let displaced = registry
        .rebind_checked("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
        .expect("override never errors on conflict");
    assert_eq!(displaced, vec!["copy".to_string()]);

    let winner = registry.action("new_tab").unwrap();
    assert_eq!(winner.current_key, "Ctrl+Shift+C");
    assert!(!winner.is_default);

    let loser = registry.action("copy").unwrap();
    assert_eq!(loser.current_key, "", "the loser is unbound");
    assert!(!loser.enabled);
    assert!(!loser.is_bound());
}

#[test]
fn override_lets_the_new_owner_take_the_chord_back() {
    let mut registry = registry_with_defaults();
    registry
        .rebind_checked("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
        .unwrap();

    // `copy` can no longer take it under Reject: rule 3 — the global
    // `new_tab` fires whenever the terminal has focus.
    assert!(matches!(
        registry.rebind_checked("copy", "Ctrl+Shift+C", ConflictPolicy::Reject),
        Err(RebindError::Conflict { .. })
    ));

    // With Override the terminal wins it back, and the global loses its chord.
    let displaced = registry
        .rebind_checked("copy", "Ctrl+Shift+C", ConflictPolicy::Override)
        .expect("override never errors on conflict");
    assert_eq!(displaced, vec!["new_tab".to_string()]);
    assert!(!registry.action("new_tab").unwrap().is_bound());
    assert!(registry.action("copy").unwrap().is_bound());
}

#[test]
fn rebinding_to_the_same_chord_is_not_a_conflict() {
    let mut registry = registry_with_defaults();
    registry
        .rebind_checked("copy", "Ctrl+Shift+C", ConflictPolicy::Reject)
        .expect("an action never conflicts with itself");
}

#[test]
fn rebinding_a_disabled_action_re_enables_it() {
    let mut registry = registry_with_defaults();
    registry.set_enabled("copy", false).unwrap();
    assert!(!registry.action("copy").unwrap().is_bound());
    registry
        .rebind_checked("copy", "Ctrl+Shift+Y", ConflictPolicy::Reject)
        .unwrap();
    let action = registry.action("copy").unwrap();
    assert!(action.enabled);
    assert!(action.is_bound());
}

#[test]
fn a_disabled_action_holds_no_chord() {
    let mut registry = registry_with_defaults();
    registry.set_enabled("copy", false).unwrap();
    assert!(!registry.action("copy").unwrap().is_bound());
    // Disabled, so `paste` may take the same chord without an override…
    registry
        .rebind_checked("paste", "Ctrl+Shift+C", ConflictPolicy::Reject)
        .expect("a disabled action must not conflict");
    // …and the only holder left is the one that just took it.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+Shift+C",
            ActionContext::Terminal
        )),
        vec!["paste".to_string()]
    );
}

#[test]
fn an_unbound_action_holds_no_chord() {
    let mut registry = registry_with_defaults();
    registry
        .rebind_checked("new_tab", "Ctrl+Shift+C", ConflictPolicy::Override)
        .unwrap();
    // The unbound `copy` holds nothing…
    assert!(!registry.action("copy").unwrap().is_bound());
    // …and from either context the only holder left is the winner. The
    // terminal sees it because of rule 3, not because copy still does.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+Shift+C",
            ActionContext::Global
        )),
        vec!["new_tab".to_string()]
    );
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+Shift+C",
            ActionContext::Terminal
        )),
        vec!["new_tab".to_string()]
    );
}

#[test]
fn conflicting_actions_reports_every_holder() {
    let registry = registry_with_defaults();
    // Ctrl+D: one holder per panel; the terminal co-fires with both.
    assert_eq!(
        ids(conflicting_actions(
            &registry,
            "Ctrl+D",
            ActionContext::Terminal
        )),
        vec!["duplicate_session".to_string(), "sftp_download".to_string()]
    );
    // Alt+1..Alt+9 are the goto_tab chords, each with exactly one holder.
    for n in 1..=9 {
        let holders = conflicting_actions(&registry, &format!("Alt+{n}"), ActionContext::Global);
        assert_eq!(ids(holders), vec![format!("goto_tab_{n}")]);
    }
}

#[test]
fn conflicting_actions_on_an_unused_chord_is_empty() {
    let registry = registry_with_defaults();
    assert!(conflicting_actions(&registry, "Ctrl+Alt+Alt+F9", ActionContext::Global).is_empty());
}

#[test]
fn reset_to_default() {
    let mut registry = registry_with_defaults();
    registry.rebind("copy", "Ctrl+Y").unwrap();
    registry.set_enabled("copy", false).unwrap();
    registry.reset_to_default("copy").unwrap();
    let action = registry.action("copy").unwrap();
    assert_eq!(action.current_key, "Ctrl+Shift+C");
    assert!(action.enabled);
    assert!(action.is_default);
}

#[test]
fn reset_to_default_unknown_action_errors() {
    let mut registry = registry_with_defaults();
    assert_eq!(
        registry.reset_to_default("nope"),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
    assert_eq!(
        registry.set_enabled("nope", false),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
}

/// The persistence path (doc 11 §Persistence): apply to the current
/// binding, honoring the stored `enabled` flag, with no conflict check —
/// a stored layout was resolved when it was made.
#[test]
fn apply_loaded_binding_mutates_current_key() {
    let mut registry = registry_with_defaults();
    registry
        .apply_loaded_binding("copy", "Ctrl+Y", true)
        .unwrap();
    assert_eq!(registry.current_key("copy"), Some("Ctrl+Y"));
    assert!(!registry.action("copy").unwrap().is_default);
    assert_eq!(
        registry.action("copy").unwrap().default_key,
        "Ctrl+Shift+C",
        "doc: rebind the current binding, not default_key metadata"
    );
}

#[test]
fn apply_loaded_binding_restores_disabled_and_unbound_rows() {
    let mut registry = registry_with_defaults();
    registry.apply_loaded_binding("copy", "", false).unwrap();
    let action = registry.action("copy").unwrap();
    assert_eq!(action.current_key, "");
    assert!(!action.enabled);
    assert!(!action.is_bound());
}

#[test]
fn apply_loaded_binding_accepts_an_inert_key() {
    // Load is permissive by design; the engine is the layer that skips
    // a non-empty row GTK could never fire.
    let mut registry = registry_with_defaults();
    registry
        .apply_loaded_binding("copy", "Ctrl+Bogus", true)
        .unwrap();
    assert_eq!(registry.current_key("copy"), Some("Ctrl+Bogus"));
}

#[test]
fn apply_loaded_binding_unknown_action_errors() {
    let mut registry = registry_with_defaults();
    assert_eq!(
        registry.apply_loaded_binding("nope", "Ctrl+K", true),
        Err(RebindError::UnknownAction {
            id: "nope".to_string()
        })
    );
}

#[test]
fn ordered_actions_follows_the_doc_table_order() {
    let registry = registry_with_defaults();
    let ordered: Vec<&str> = registry
        .ordered_actions()
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    let globals: Vec<&str> = ordered
        .iter()
        .copied()
        .take_while(|id| registry.action(id).unwrap().context == ActionContext::Global)
        .collect();
    assert_eq!(globals.len(), 20, "20 global actions in doc 11's table");
    // Within a context, ids sort; the first global is close_tab.
    assert_eq!(globals[0], "close_tab");
    assert_eq!(
        ordered[globals.len()],
        "clear",
        "terminal actions follow the globals"
    );
    assert_eq!(ordered.len(), registry.len());
    // Deterministic across calls (map iteration order must not leak).
    assert_eq!(
        ordered,
        registry
            .ordered_actions()
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn custom_actions_are_the_deviations() {
    let mut registry = registry_with_defaults();
    assert!(registry.custom_actions().is_empty());
    registry.rebind("copy", "Ctrl+Y").unwrap();
    registry.set_enabled("paste", false).unwrap();
    assert_eq!(ids(registry.custom_actions()), vec!["copy", "paste"]);
}

#[test]
fn conflicts_for_ignores_unknown_actions() {
    let registry = registry_with_defaults();
    assert!(registry.conflicts_for("nope", "Ctrl+Shift+C").is_empty());
}

#[test]
fn action_mut_exposes_one_action() {
    let mut registry = registry_with_defaults();
    registry.action_mut("copy").unwrap().display_name = "Copy It".into();
    assert_eq!(registry.action("copy").unwrap().display_name, "Copy It");
    assert!(registry.action_mut("nope").is_none());
}

#[test]
fn action_context_wire_strings() {
    for context in ActionContext::ALL {
        assert_eq!(
            ActionContext::from_db_str(context.as_str()),
            Some(context),
            "{context:?} must round-trip through its wire string"
        );
    }
    assert_eq!(ActionContext::Global.as_str(), "global");
    assert_eq!(ActionContext::Terminal.as_str(), "terminal");
    assert_eq!(ActionContext::SessionTree.as_str(), "session_tree");
    assert_eq!(ActionContext::Sftp.as_str(), "sftp");
    assert_eq!(ActionContext::RdpTab.as_str(), "rdp_tab");
    assert_eq!(ActionContext::VncTab.as_str(), "vnc_tab");
    assert_eq!(ActionContext::from_db_str("nonsense"), None);
    assert_eq!(ActionContext::from_db_str(""), None);
    assert_eq!(ActionContext::from_db_str("Global"), None);
}

#[test]
fn focus_contexts_are_the_two_panels() {
    assert!(ActionContext::SessionTree.is_focus_context());
    assert!(ActionContext::Sftp.is_focus_context());
    assert!(!ActionContext::Terminal.is_focus_context());
    assert!(!ActionContext::Global.is_focus_context());
    assert!(!ActionContext::RdpTab.is_focus_context());
    assert!(!ActionContext::VncTab.is_focus_context());
}

/// A chord is a key, not a spelling: the capture dialog renders
/// `Ctrl+Shift+t`, the table says `Ctrl+Shift+T`, and they collide.
#[test]
fn chords_compare_by_key_not_by_spelling() {
    let mut registry = registry_with_defaults();
    for spelling in ["Ctrl+Shift+t", "Shift+Ctrl+T", "Shift+Ctrl+t"] {
        assert_eq!(
            ids(registry.conflicts_for("copy", spelling)),
            vec!["new_tab".to_string()],
            "{spelling}"
        );
    }
    assert_eq!(
        ids(registry.conflicts_for("next_tab", "Ctrl+Shift+ISO_Left_Tab")),
        vec!["prev_tab".to_string()],
        "Shift+Tab arrives as ISO_Left_Tab"
    );
    let err = registry
        .rebind_checked("copy", "Ctrl+Shift+t", ConflictPolicy::Reject)
        .expect_err("a lower-case capture of new_tab's chord");
    assert!(matches!(err, RebindError::Conflict { .. }));
}

#[test]
fn keys_are_stored_canonical_so_a_recaptured_default_is_the_default() {
    let mut registry = registry_with_defaults();
    registry.rebind("new_tab", "Ctrl+Alt+t").unwrap();
    assert_eq!(registry.current_key("new_tab"), Some("Ctrl+Alt+T"));
    registry.rebind("new_tab", "Shift+Ctrl+t").unwrap();
    assert_eq!(registry.current_key("new_tab"), Some("Ctrl+Shift+T"));
    assert!(registry.action("new_tab").unwrap().is_default);
    assert!(registry.custom_actions().is_empty());
    // Anything unparsable is kept as given: it can never fire.
    registry.rebind("new_tab", "Ctrl+Nonsense").unwrap();
    assert_eq!(registry.current_key("new_tab"), Some("Ctrl+Nonsense"));
}

#[test]
fn error_messages_name_the_problem() {
    let err = RebindError::UnknownAction {
        id: "nope".to_string(),
    };
    assert!(err.to_string().contains("nope"));
    let err = RebindError::UnparsableKey {
        key: "Ctrl+Bogus".to_string(),
        reason: "unknown key name \"Bogus\"".to_string(),
    };
    assert!(err.to_string().contains("Bogus"));
    let err = RebindError::Conflict {
        key: "Ctrl+N".to_string(),
        context: ActionContext::Global,
        conflicts: vec!["new_session".to_string()],
    };
    assert!(err.to_string().contains("new_session"));
    assert!(err.to_string().contains("Ctrl+N"));
}
