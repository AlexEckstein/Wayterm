//! Schema, seed, and pragma tests for [`SessionStore`] (doc 06).
//!
//! Every constant asserted here — table names, index names, seed row
//! counts, `host_key_policy=strict` — comes from doc 06 §"Database
//! Schema" and §"Default Data". Treat any change as a schema break.

use std::collections::HashSet;
use std::path::PathBuf;

use rusqlite::OptionalExtension;

use super::*;

/// Names of the `type='table'` rows `open` must create (doc 06).
const TABLES: &[&str] = &[
    "folders",
    "sessions",
    "hotkeys",
    "settings",
    "connection_history",
];

/// Names of the indexes `open` must create (doc 06 §"Indexes").
const INDEXES: &[&str] = &[
    "idx_sessions_folder",
    "idx_sessions_type",
    "idx_sessions_host",
    "idx_folders_parent",
    "idx_hotkeys_action",
    "idx_history_session",
    "idx_history_recent",
];

impl SessionStore {
    /// `type` names from `sqlite_master` for the given `type`,
    /// excluding SQLite's internal `sqlite_*` objects (e.g.
    /// `sqlite_sequence`, auto-created by the AUTOINCREMENT columns
    /// doc 06's schema uses, and `sqlite_autoindex_*`).
    fn schema_objects(&self, type_: &str) -> HashSet<String> {
        let conn = &self.conn;
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite_%'")
            .unwrap();
        let rows = stmt
            .query_map([type_], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn scalar_i64(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .unwrap()
            .flatten()
    }
}

#[test]
fn open_creates_all_tables() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path().join("wayterm.db").as_path()).unwrap();

    let tables = store.schema_objects("table");
    for name in TABLES {
        assert!(tables.contains(*name), "missing table {name}");
    }
    // No accidental extra tables (e.g. from a stray statement).
    assert_eq!(tables.len(), TABLES.len(), "unexpected tables: {tables:?}");
}

#[test]
fn open_creates_all_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path().join("wayterm.db").as_path()).unwrap();

    let indexes = store.schema_objects("index");
    for name in INDEXES {
        assert!(indexes.contains(*name), "missing index {name}");
    }
}

#[test]
fn open_twice_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("wayterm.db");

    // Second open must succeed and must not re-seed or re-create.
    drop(SessionStore::open(path.as_path()).unwrap());
    let store = SessionStore::open(path.as_path()).unwrap();

    // CREATE ... IF NOT EXISTS keeps table count stable...
    assert_eq!(store.schema_objects("table").len(), TABLES.len());
    // ...and the seed probe keeps row counts at one generation.
    assert_eq!(store.scalar_i64("SELECT COUNT(*) FROM hotkeys"), 28);
    assert_eq!(
        store.scalar_i64("SELECT COUNT(*) FROM settings"),
        10,
        "re-open must not duplicate seed settings"
    );
}

#[test]
fn seed_defaults_present() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path().join("wayterm.db").as_path()).unwrap();

    // Doc 06 §"Default Data": 28 hotkeys, 10 settings.
    assert_eq!(store.scalar_i64("SELECT COUNT(*) FROM hotkeys"), 28);
    assert_eq!(store.scalar_i64("SELECT COUNT(*) FROM settings"), 10);

    // Every seeded hotkey is a default binding, enabled.
    assert_eq!(
        store.scalar_i64("SELECT COUNT(*) FROM hotkeys WHERE is_default = 1"),
        28
    );
    assert_eq!(
        store.scalar_i64("SELECT COUNT(*) FROM hotkeys WHERE enabled = 1"),
        28
    );

    // Doc 06 pins the global host-key default; per-session override
    // happens via sessions.host_key_policy (doc 11).
    assert_eq!(
        store.setting("host_key_policy").as_deref(),
        Some("strict"),
        "host_key_policy must seed as 'strict'"
    );
}

#[test]
fn wal_mode_active_after_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path().join("wayterm.db").as_path()).unwrap();

    let mode: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");

    // foreign_keys is armed for this connection too (per-connection
    // pragma; re-armed on every open).
    let fk: i64 = store
        .conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fk, 1);
}

#[test]
fn in_memory_has_same_schema() {
    let store = SessionStore::in_memory().unwrap();

    let tables = store.schema_objects("table");
    for name in TABLES {
        assert!(tables.contains(*name), "missing table {name}");
    }
    assert_eq!(tables.len(), TABLES.len());

    let indexes = store.schema_objects("index");
    for name in INDEXES {
        assert!(indexes.contains(*name), "missing index {name}");
    }

    // Seed data lands in memory stores too.
    assert_eq!(store.scalar_i64("SELECT COUNT(*) FROM hotkeys"), 28);
    assert_eq!(store.scalar_i64("SELECT COUNT(*) FROM settings"), 10);
    assert_eq!(store.setting("host_key_policy").as_deref(), Some("strict"));

    // WAL is impossible on :memory: databases — SQLite reports
    // 'memory'. Assert the documented no-op rather than a false
    // 'wal', so a regression in pragma handling still surfaces.
    let mode: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "memory");

    let fk: i64 = store
        .conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fk, 1);
}
