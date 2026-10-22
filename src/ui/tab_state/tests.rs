//! Unit tests for the display-free tab-persistence model
//! ([`super`], doc 03 §Restoring tabs Phase 4).
//!
//! No GTK here: these lock the wire format and the placement rules,
//! which is why they can run headless in CI.

use super::*;

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[test]
fn tabs_to_json_roundtrips() {
    let tabs = vec![
        TabState::session(7, "prod-db", 1_700_000_000_000, 0),
        TabState::detached("scratch", 1_700_000_000_500, 1),
    ];

    let json = tabs_to_json(&tabs).unwrap();
    assert_eq!(tabs_from_json(&json).unwrap(), tabs);
}

#[test]
fn json_uses_the_documented_field_names() {
    let json = tabs_to_json(&[TabState::session(3, "db", 42, 0)]).unwrap();
    assert_eq!(
        json,
        r#"[{"session_id":3,"label":"db","created_at":42,"order":0}]"#
    );
}

#[test]
fn json_omits_nothing_for_a_detached_tab() {
    // `session_id: null` must survive as null, not collapse to a
    // session tab: a detached tab restores unconnected, and a
    // session-bound one reconnects on click.
    let json = tabs_to_json(&[TabState::detached("scratch", 1, 0)]).unwrap();
    assert_eq!(
        json,
        r#"[{"session_id":null,"label":"scratch","created_at":1,"order":0}]"#
    );
    assert_eq!(tabs_from_json(&json).unwrap()[0].session_id, None);
}

#[test]
fn unknown_fields_are_ignored() {
    // Forward compatibility: a list written by a newer Wayterm still
    // restores in this one, as far as the fields here are concerned.
    let json = r#"[{"session_id":5,"label":"db","created_at":9,"order":0,
                   "pane_split":3,"future_flag":true,"geometry":{"x":1}}]"#;

    let tabs = tabs_from_json(json).unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].session_id, Some(5));
    assert_eq!(tabs[0].label, "db");
    assert_eq!(tabs[0].created_at, 9);
}

#[test]
fn missing_fields_fall_back_to_defaults() {
    // Backward compatibility: a row written before `created_at` (or
    // `order`) existed still restores instead of failing the launch.
    let tabs = tabs_from_json(r#"[{"session_id":5,"label":"db"}]"#).unwrap();

    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].session_id, Some(5));
    assert_eq!(tabs[0].label, "db");
    assert_eq!(tabs[0].created_at, 0);
    assert_eq!(tabs[0].order, 0);
}

#[test]
fn empty_list_roundtrips() {
    let json = tabs_to_json(&[]).unwrap();
    assert_eq!(json, "[]");
    assert!(tabs_from_json(&json).unwrap().is_empty());
}

#[test]
fn empty_list_survives_whitespace_and_newlines() {
    // `to_string_pretty` output must parse back the same way.
    let tabs = vec![TabState::session(1, "a", 10, 0)];
    let pretty = serde_json::to_string_pretty(&tabs).unwrap();
    let reparsed = tabs_from_json(&pretty).unwrap();
    assert_eq!(reparsed, tabs);
}

#[test]
fn malformed_json_is_an_error_not_an_empty_list() {
    // The distinction matters: an empty list means "nothing saved",
    // which the UI would restore silently, hiding a broken row.
    assert!(tabs_from_json("not json").is_err());
    assert!(
        tabs_from_json(r#"{"session_id":1}"#).is_err(),
        "an object is not a list"
    );
    assert!(tabs_from_json("").is_err());
}

#[test]
fn labels_with_unicode_and_quotes_survive() {
    let tabs = vec![TabState::session(
        1,
        "prod \"db\" \u{1f5a5} – ünïcode",
        5,
        0,
    )];
    let json = tabs_to_json(&tabs).unwrap();
    assert_eq!(tabs_from_json(&json).unwrap(), tabs);
}

#[test]
fn tab_state_constructors_set_what_they_say() {
    let bound = TabState::session(12, "web1", 7, 3);
    assert_eq!(bound.session_id, Some(12));
    assert_eq!(bound.label, "web1");

    let detached = TabState::detached("scratch", 7, 3);
    assert_eq!(detached.session_id, None);
    assert_eq!(detached.label, "scratch");
    assert_eq!(detached.order, 3);
}

// ---------------------------------------------------------------------------
// Placement / ordering
// ---------------------------------------------------------------------------

fn page(session_id: Option<i64>, label: &str, created_at: i64) -> PageInfo {
    PageInfo {
        session_id,
        label: label.to_string(),
        created_at,
    }
}

#[test]
fn states_from_pages_numbers_tabs_from_zero() {
    let pages = [
        page(None, "Home", 1),
        page(Some(4), "prod-db", 2),
        page(Some(9), "web1", 3),
    ];

    let tabs = states_from_pages(&pages, Some(0));
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].order, 0);
    assert_eq!(tabs[1].order, 1);
    assert_eq!(tabs[0].label, "prod-db");
    assert_eq!(tabs[1].session_id, Some(9));
}

#[test]
fn states_from_pages_skips_home_by_index_not_position() {
    // The home tab is skipped wherever it is — a notebook whose tabs
    // were reordered must still save the two session tabs, not the
    // welcome page.
    let pages = [
        page(Some(4), "prod-db", 2),
        page(None, "Home", 1),
        page(Some(9), "web1", 3),
    ];

    let tabs = states_from_pages(&pages, Some(1));
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].label, "prod-db");
    assert_eq!(tabs[1].label, "web1");
    assert_eq!(tabs.iter().map(|t| t.order).collect::<Vec<_>>(), [0, 1]);
}

#[test]
fn states_from_pages_with_no_home_persists_every_page() {
    let pages = [page(Some(1), "a", 1), page(Some(2), "b", 2)];
    let tabs = states_from_pages(&pages, None);
    assert_eq!(tabs.len(), 2);
}

#[test]
fn states_from_pages_out_of_range_home_index_keeps_everything() {
    // Defensive: a stale index must not silently eat a real tab.
    let pages = [page(None, "Home", 1), page(Some(4), "prod-db", 2)];
    assert_eq!(states_from_pages(&pages, Some(7)).len(), 2);
}

#[test]
fn states_from_pages_carries_timestamps_through() {
    let pages = [
        page(None, "Home", 1),
        page(Some(4), "prod-db", 1_700_000_000_123),
    ];
    let tabs = states_from_pages(&pages, Some(0));
    assert_eq!(tabs[0].created_at, 1_700_000_000_123);
}

#[test]
fn sorted_for_restore_uses_order_first() {
    // Creation order and order disagree on purpose: the tab bar the
    // user left behind wins, which is what `order` records.
    let tabs = vec![
        TabState::session(1, "third", 100, 2),
        TabState::session(2, "first", 300, 0),
        TabState::session(3, "second", 200, 1),
    ];

    let sorted = sorted_for_restore(&tabs);
    let labels: Vec<&str> = sorted.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["first", "second", "third"]);
}

#[test]
fn sorted_for_restore_breaks_ties_by_creation_time() {
    // Two tabs saved in the same millisecond: without a tie-break the
    // restored order would be whatever the sort felt like, and the user
    // would see their tabs shuffle between runs.
    let tabs = vec![
        TabState::session(1, "later", 200, 0),
        TabState::session(2, "earlier", 100, 0),
    ];

    let sorted = sorted_for_restore(&tabs);
    let labels: Vec<&str> = sorted.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["earlier", "later"]);
}

#[test]
fn sorted_for_restore_is_deterministic_for_identical_timestamps() {
    let tabs = vec![
        TabState::session(9, "b", 100, 0),
        TabState::session(2, "a", 100, 0),
    ];

    // Session id is the last tie-break, so the result does not depend
    // on input order.
    let first: Vec<i64> = sorted_for_restore(&tabs)
        .iter()
        .filter_map(|t| t.session_id)
        .collect();
    let flipped: Vec<TabState> = tabs.iter().rev().cloned().collect();
    let reversed: Vec<i64> = sorted_for_restore(&flipped)
        .iter()
        .filter_map(|t| t.session_id)
        .collect();
    assert_eq!(first, reversed);
    assert_eq!(first, [2, 9]);
}

#[test]
fn sorted_for_restore_handles_empty_and_single() {
    assert!(sorted_for_restore(&[]).is_empty());
    let one = vec![TabState::session(1, "only", 1, 5)];
    assert_eq!(sorted_for_restore(&one).len(), 1);
}

#[test]
fn sorted_for_restore_does_not_mutate_its_input() {
    let tabs = vec![
        TabState::session(1, "b", 1, 1),
        TabState::session(2, "a", 1, 0),
    ];
    let _ = sorted_for_restore(&tabs);
    assert_eq!(tabs[0].label, "b", "input order must be left alone");
}

#[test]
fn restore_page_index_places_session_tabs_after_home() {
    // The welcome tab is rebuilt at launch, so restored tabs start at
    // page 1.
    assert_eq!(restore_page_index(0), 1);
    assert_eq!(restore_page_index(1), 2);
    assert_eq!(restore_page_index(9), 10);
}

#[test]
fn renumber_makes_order_dense() {
    let mut tabs = vec![
        TabState::session(1, "a", 1, 7),
        TabState::session(2, "b", 2, 40),
        TabState::session(3, "c", 3, 900),
    ];

    renumber(&mut tabs);
    assert_eq!(tabs.iter().map(|t| t.order).collect::<Vec<_>>(), [0, 1, 2]);
    // Renumbering must not disturb identity.
    assert_eq!(tabs[0].session_id, Some(1));
    assert_eq!(tabs[2].label, "c");
}

#[test]
fn renumber_on_empty_is_a_noop() {
    let mut tabs: Vec<TabState> = Vec::new();
    renumber(&mut tabs);
    assert!(tabs.is_empty());
}

#[test]
fn the_scan_and_the_restore_agree_on_order() {
    // The whole save/restore contract: scan a notebook (drop home),
    // then read the saved list back the way a launch does.
    let pages = [
        page(None, "Home", 1),
        page(Some(4), "b-second", 200),
        page(Some(9), "a-first", 100),
        page(None, "scratch", 300),
    ];

    let saved = states_from_pages(&pages, Some(0));
    let reopened = sorted_for_restore(&saved);

    let labels: Vec<&str> = reopened.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["b-second", "a-first", "scratch"]);
    assert_eq!(
        reopened.iter().map(|t| t.order).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        reopened[2].session_id, None,
        "a detached tab stays detached"
    );
}

#[test]
fn a_scanned_list_survives_the_wire_format() {
    // Scan → persist → parse → reopen: what the app does across a
    // restart, minus the display.
    let pages = [
        page(None, "Home", 1),
        page(Some(4), "prod-db", 100),
        page(None, "scratch", 200),
        page(Some(9), "web1", 300),
    ];
    let saved = states_from_pages(&pages, Some(0));
    let json = tabs_to_json(&saved).unwrap();
    let reloaded = tabs_from_json(&json).unwrap();

    assert_eq!(reloaded, saved);
    assert_eq!(
        sorted_for_restore(&reloaded),
        saved,
        "already in tab-bar order"
    );
    assert_eq!(reloaded.len(), 3, "home is not part of the saved list");
}

#[test]
fn now_ms_is_a_plausible_unix_timestamp_in_milliseconds() {
    // Only a sanity check: the value goes into the saved row.
    let now = now_ms();
    assert!(
        now > 1_600_000_000_000,
        "expected ms since epoch, got {now}"
    );
    assert!(now_ms() >= now, "the clock must not go backwards");
}
