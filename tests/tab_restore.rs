//! Integration tests for the tab-persistence store seam (doc 10 Phase 4,
//! "Remember open tabs on exit, restore on launch").
//!
//! [`wayterm::ui::tab_state`] unit-tests the wire format itself; this file
//! covers the other half of the contract — that the list actually lands in
//! SQLite and comes back out the way a relaunched app needs it, through the
//! same constructors the UI uses:
//!
//! * **in-memory store** — the shape the model tests can use cheaply;
//! * **file store** — the real thing, and the only one that can answer the
//!   question that matters ("what happens on the next launch?"), since a
//!   relaunch is a fresh `open()` of the same file;
//! * **a duplicated connection** — how [`SessionStore::duplicate`]-based
//!   GTK wiring reaches the same file (doc 06's UI note), so the row the
//!   window writes is the row the next launch reads.
//!
//! No GTK and no display: nothing here constructs a widget.

use rusqlite::Connection;
use wayterm::store::model::{SessionConfig, SessionType};
use wayterm::store::SessionStore;
use wayterm::ui::tab_state::{sorted_for_restore, TabState};

/// A store on a real file, plus the path it lives at (for the tests that
/// need a second, non-`SessionStore` connection onto the same database).
struct FileStore {
    store: SessionStore,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn file_store() -> FileStore {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayterm.sqlite3");
    let store = SessionStore::open(&path).unwrap();
    FileStore {
        store,
        path,
        _dir: dir,
    }
}

/// Read the raw `open_tabs` row, bypassing the store's own parsing.
fn raw_open_tabs(path: &std::path::Path) -> Option<String> {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'open_tabs'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Write a raw value into the `open_tabs` row, bypassing the store's
/// serialization (to plant rows the app would never write).
fn plant_raw_open_tabs(path: &std::path::Path, value: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('open_tabs', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [value],
    )
    .unwrap();
}

fn labels_of(tabs: &[TabState]) -> Vec<&str> {
    tabs.iter().map(|t| t.label.as_str()).collect()
}

// ---------------------------------------------------------------------------
// Round-trips
// ---------------------------------------------------------------------------

#[test]
fn in_memory_store_roundtrips_the_open_tab_list() {
    let store = SessionStore::in_memory().unwrap();
    let tabs = vec![
        TabState::session(7, "prod-db", 1_700_000_000_000, 0),
        TabState::detached("scratch", 1_700_000_000_500, 1),
    ];

    store.save_open_tabs(&tabs).unwrap();
    assert_eq!(store.load_open_tabs().unwrap(), tabs);
}

#[test]
fn file_store_roundtrips_the_open_tab_list() {
    let files = file_store();
    let tabs = vec![
        TabState::session(3, "web1", 111, 0),
        TabState::session(4, "web2", 222, 1),
    ];

    files.store.save_open_tabs(&tabs).unwrap();
    assert_eq!(files.store.load_open_tabs().unwrap(), tabs);
}

#[test]
fn the_list_survives_a_relaunch() {
    // The whole feature in one test: save on the way out, drop the
    // connection, and read it back from a *fresh* `open()` of the same
    // file — which is what the next launch does.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayterm.sqlite3");

    let tabs = vec![
        TabState::session(1, "prod-db", 1_700_000_000_000, 0),
        TabState::session(2, "web1", 1_700_000_100_000, 1),
        TabState::detached("scratch", 1_700_000_200_000, 2),
    ];
    {
        let store = SessionStore::open(&path).unwrap();
        store.save_open_tabs(&tabs).unwrap();
    }

    let relaunched = SessionStore::open(&path).unwrap();
    assert_eq!(relaunched.load_open_tabs().unwrap(), tabs);
}

#[test]
fn unicode_labels_survive_the_store() {
    let store = SessionStore::in_memory().unwrap();
    let tabs = vec![TabState::session(
        1,
        "prod \"db\" \u{1f5a5} – ünïcode",
        5,
        0,
    )];

    store.save_open_tabs(&tabs).unwrap();
    assert_eq!(store.load_open_tabs().unwrap(), tabs);
}

#[test]
fn detached_tabs_roundtrip_without_a_session_id() {
    // A tab with nothing behind it must not come back bound to session 0:
    // `None` and `Some(0)` would restore differently (one is looked up in
    // `sessions`, the other is not).
    let store = SessionStore::in_memory().unwrap();
    let tabs = vec![TabState::detached("scratch", 9, 0)];

    store.save_open_tabs(&tabs).unwrap();
    let loaded = store.load_open_tabs().unwrap();
    assert_eq!(loaded[0].session_id, None);
}

// ---------------------------------------------------------------------------
// Write semantics
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_store_has_no_saved_tabs() {
    // No `open_tabs` row at all is the same thing as "nothing to
    // restore" — the row only appears once the app writes it.
    let store = SessionStore::in_memory().unwrap();
    assert!(store.load_open_tabs().unwrap().is_empty());
}

#[test]
fn saving_twice_replaces_rather_than_appends() {
    // The `settings` key is a primary key, so a second save must upsert.
    // If it did not, the second save would fail (or, with a plain
    // INSERT, duplicate the user's tabs).
    let store = SessionStore::in_memory().unwrap();
    store
        .save_open_tabs(&[TabState::session(1, "first", 1, 0)])
        .unwrap();
    store
        .save_open_tabs(&[TabState::session(2, "second", 2, 0)])
        .unwrap();

    let loaded = store.load_open_tabs().unwrap();
    assert_eq!(labels_of(&loaded), ["second"]);
}

#[test]
fn a_run_of_saves_converges_on_the_last_one() {
    // Debounced saves fire repeatedly over a session; only the newest
    // list may survive.
    let store = SessionStore::in_memory().unwrap();
    for round in 0..25 {
        let tabs = (0..=round)
            .map(|i| TabState::session(i as i64, format!("tab-{i}"), i as i64, i as i64))
            .collect::<Vec<_>>();
        store.save_open_tabs(&tabs).unwrap();
    }

    let loaded = store.load_open_tabs().unwrap();
    assert_eq!(loaded.len(), 25, "the last save wins, wholesale");
    assert_eq!(loaded.last().unwrap().label, "tab-24");
}

#[test]
fn saving_an_empty_list_clears_the_row_but_keeps_it() {
    // "No tabs open" is a state worth remembering, and it is stored as
    // `[]` rather than deleting the row — so a deliberately-emptied
    // store stays distinguishable from one that was never seeded.
    let files = file_store();
    files
        .store
        .save_open_tabs(&[TabState::session(1, "prod-db", 1, 0)])
        .unwrap();
    files.store.save_open_tabs(&[]).unwrap();

    assert!(files.store.load_open_tabs().unwrap().is_empty());
    assert_eq!(raw_open_tabs(&files.path).as_deref(), Some("[]"));
}

#[test]
fn saving_tabs_leaves_the_doc06_seed_settings_alone() {
    // The row is an upsert into a table doc 06 already populates. A
    // missing key qualifier would happily wipe the defaults.
    let files = file_store();
    files
        .store
        .save_open_tabs(&[TabState::session(1, "prod-db", 1, 0)])
        .unwrap();

    let conn = Connection::open(&files.path).unwrap();
    let policy: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'host_key_policy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(policy, "strict", "doc 06's seeded default must survive");

    let seeded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM settings WHERE key <> 'open_tabs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        seeded, 10,
        "doc 06 seeds 10 settings (doc 06 §Default Data)"
    );
}

// ---------------------------------------------------------------------------
// Read semantics
// ---------------------------------------------------------------------------

#[test]
fn malformed_json_surfaces_as_an_error_not_an_empty_list() {
    // The distinction is the whole point of returning `Result`: an empty
    // list is a *successful* launch with nothing saved, and a broken row
    // must not be mistaken for one.
    let files = file_store();
    plant_raw_open_tabs(&files.path, "{not json");

    assert!(files.store.load_open_tabs().is_err());
}

#[test]
fn a_json_object_instead_of_a_list_is_an_error() {
    let files = file_store();
    plant_raw_open_tabs(&files.path, r#"{"session_id": 1}"#);

    assert!(files.store.load_open_tabs().is_err());
}

#[test]
fn an_unknown_field_in_the_row_is_tolerated() {
    // A row written by a newer Wayterm (a split-pane layout, say) must
    // still restore the fields this version understands.
    let files = file_store();
    plant_raw_open_tabs(
        &files.path,
        r#"[{"session_id":5,"label":"prod-db","created_at":9,"order":0,
             "pane_split":3,"geometry":{"w":80,"h":24}}]"#,
    );

    let loaded = files.store.load_open_tabs().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0], TabState::session(5, "prod-db", 9, 0));
}

// ---------------------------------------------------------------------------
// Order
// ---------------------------------------------------------------------------

#[test]
fn restore_order_is_tab_bar_order_not_creation_order() {
    // `order` is the notebook position; `created_at` is the tab's age.
    // They disagree here on purpose, and the user must get their tab bar
    // back — not their tabs in the order they happened to be opened.
    let store = SessionStore::in_memory().unwrap();
    store
        .save_open_tabs(&[
            TabState::session(1, "third", 100, 2),
            TabState::session(2, "first", 300, 0),
            TabState::session(3, "second", 200, 1),
        ])
        .unwrap();

    let restored = sorted_for_restore(&store.load_open_tabs().unwrap());
    assert_eq!(labels_of(&restored), ["first", "second", "third"]);
}

#[test]
fn tab_bar_order_survives_a_relaunch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayterm.sqlite3");
    let saved = vec![
        TabState::session(1, "web2", 100, 1),
        TabState::session(2, "web1", 200, 0),
    ];
    {
        let store = SessionStore::open(&path).unwrap();
        store.save_open_tabs(&saved).unwrap();
    }

    let relaunched = SessionStore::open(&path).unwrap();
    let restored = sorted_for_restore(&relaunched.load_open_tabs().unwrap());
    assert_eq!(labels_of(&restored), ["web1", "web2"]);
}

// ---------------------------------------------------------------------------
// The seam the UI actually uses
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_connection_writes_what_the_next_launch_reads() {
    // `MainWindow` hands tab persistence a `store.duplicate()`, because
    // GTK closures cannot share the one `Connection` (doc 06). This is
    // the test that says that arrangement actually persists.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayterm.sqlite3");
    let store = SessionStore::open(&path).unwrap();

    let for_the_window = store.duplicate().unwrap();
    for_the_window
        .save_open_tabs(&[
            TabState::session(1, "prod-db", 1, 0),
            TabState::session(2, "web1", 2, 1),
        ])
        .unwrap();

    let relaunched = SessionStore::open(&path).unwrap();
    assert_eq!(
        labels_of(&relaunched.load_open_tabs().unwrap()),
        ["prod-db", "web1"]
    );
}

#[test]
fn session_ids_roundtrip_against_real_session_rows() {
    // The saved ids have to be usable with the rest of the store, since
    // restore resolves each one through `get_session`.
    let store = SessionStore::in_memory().unwrap();
    let prod_db = store
        .insert_session(&SessionConfig {
            name: "prod-db".to_string(),
            session_type: SessionType::SSH,
            host: Some("db.prod.lan".to_string()),
            ..Default::default()
        })
        .unwrap();
    let web1 = store
        .insert_session(&SessionConfig {
            name: "web1".to_string(),
            session_type: SessionType::SSH,
            host: Some("web1.lan".to_string()),
            ..Default::default()
        })
        .unwrap();

    store
        .save_open_tabs(&[
            TabState::session(prod_db, "prod-db", 1, 0),
            TabState::session(web1, "web1", 2, 1),
        ])
        .unwrap();

    let restored = store.load_open_tabs().unwrap();
    for tab in &restored {
        let config = store.get_session(tab.session_id.unwrap()).unwrap();
        assert_eq!(
            config.name, tab.label,
            "saved id must resolve to its session"
        );
    }
}

#[test]
fn a_tab_whose_session_was_deleted_keeps_its_id() {
    // `sessions` rows are user data and get deleted. The tab list is not
    // a foreign-key child, so the tab survives with a dangling id — which
    // restore handles by reopening it unconnected. If the save cascaded,
    // closing a session in the tree would silently drop the tab.
    let store = SessionStore::in_memory().unwrap();
    let prod_db = store
        .insert_session(&SessionConfig {
            name: "prod-db".to_string(),
            session_type: SessionType::SSH,
            ..Default::default()
        })
        .unwrap();

    store
        .save_open_tabs(&[TabState::session(prod_db, "prod-db", 1, 0)])
        .unwrap();
    store.delete_session(prod_db).unwrap();

    let loaded = store.load_open_tabs().unwrap();
    assert_eq!(
        loaded.len(),
        1,
        "deleting a session must not delete its tab"
    );
    assert_eq!(loaded[0].session_id, Some(prod_db));
    assert!(
        store.get_session(prod_db).is_err(),
        "and restore can tell the tab is unconnectable"
    );
}

// ---------------------------------------------------------------------------
// Doc 06's rule: nothing secret reaches this row
// ---------------------------------------------------------------------------

#[test]
fn the_row_contains_no_connection_details() {
    // Only tab identity belongs here: a host, username, or port in this
    // row would put connection details in a second, unencrypted place,
    // and doc 06 keeps credentials in the OS keyring only.
    let files = file_store();
    let session_id = files
        .store
        .insert_session(&SessionConfig {
            name: "prod-db".to_string(),
            session_type: SessionType::SSH,
            host: Some("db.prod.lan".to_string()),
            port: Some(2222),
            username: Some("admin".to_string()),
            ..Default::default()
        })
        .unwrap();

    files
        .store
        .save_open_tabs(&[TabState::session(session_id, "prod-db", 1, 0)])
        .unwrap();

    let raw = raw_open_tabs(&files.path).expect("the row must exist");
    assert!(!raw.contains("db.prod.lan"), "host leaked: {raw}");
    assert!(!raw.contains("admin"), "username leaked: {raw}");
    assert!(!raw.contains("2222"), "port leaked: {raw}");
    assert_eq!(
        raw,
        r#"[{"session_id":1,"label":"prod-db","created_at":1,"order":0}]"#
    );
}
