//! Schema, seed, pragma, and CRUD tests for [`SessionStore`] (doc 06).
//!
//! Every constant asserted here — table names, index names, seed row
//! counts, `host_key_policy=strict` — comes from doc 06 §"Database
//! Schema" and §"Default Data". The CRUD tests lock the foreign-key
//! behaviors the schema declares (`folders.parent_id ON DELETE
//! CASCADE`, `sessions.folder_id ON DELETE SET NULL`). Treat any
//! change as a schema break.

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

    /// Raw session row for CRUD tests: (folder_id,) by id. Session
    /// CRUD is Task 5; these tests go around the typed API on
    /// purpose — they exist to pin the schema's ON DELETE behavior.
    fn session_folder_id(&self, id: i64) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT folder_id FROM sessions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
    }

    /// Direct SQL session insert for FK-behavior tests (Task 5 adds
    /// the typed API). Columns per doc 06 §"Database Schema".
    fn insert_session_row(&self, name: &str, folder_id: Option<i64>) -> i64 {
        self.conn
            .execute(
                "INSERT INTO sessions (name, type, folder_id) VALUES (?1, 'ssh', ?2)",
                params![name, folder_id],
            )
            .unwrap();
        self.conn.last_insert_rowid()
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

#[test]
fn insert_and_list_folders() {
    let store = SessionStore::in_memory().unwrap();

    // Empty store: no roots.
    assert!(store.list_folders(None).unwrap().is_empty());

    let prod = store.insert_folder("Production", None).unwrap();
    let staging = store.insert_folder("Staging", None).unwrap();
    let _db = store.insert_folder("prod-db", Some(prod)).unwrap();

    // list_folders(None) returns ROOTS ONLY: prod-db is invisible at
    // the top level even though it exists.
    let roots = store.list_folders(None).unwrap();
    let root_names: Vec<&str> = roots.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(root_names, ["Production", "Staging"]);

    // All roots carry the right ids (and prove ids are returned, not
    // just names).
    assert_eq!(roots[0].id, prod);
    assert_eq!(roots[1].id, staging);

    // list_folders(Some(prod)) shows the child.
    let children = store.list_folders(Some(prod)).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, _db);
    assert_eq!(children[0].name, "prod-db");
    assert_eq!(children[0].parent_id, Some(prod));
}

#[test]
fn list_folders_direct_children_only() {
    let store = SessionStore::in_memory().unwrap();

    // A -> B -> C: listing A must show B but never C (grandchild) —
    // one level per call, level by level, the way the UI tree
    // renders.
    let a = store.insert_folder("A", None).unwrap();
    let b = store.insert_folder("B", Some(a)).unwrap();
    let _c = store.insert_folder("C", Some(b)).unwrap();

    let level1 = store.list_folders(Some(a)).unwrap();
    assert_eq!(level1.len(), 1);
    assert_eq!(level1[0].id, b);

    let level2 = store.list_folders(Some(b)).unwrap();
    assert_eq!(level2.len(), 1);
    assert_eq!(level2[0].name, "C");
}

#[test]
fn rename_folder_is_visible() {
    let store = SessionStore::in_memory().unwrap();

    let id = store.insert_folder("Old", None).unwrap();
    store.rename_folder(id, "New").unwrap();

    let roots = store.list_folders(None).unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].name, "New");

    // Renaming again to the same name is fine (idempotent update).
    store.rename_folder(id, "New").unwrap();
    assert_eq!(store.list_folders(None).unwrap()[0].name, "New");
}

#[test]
fn delete_folder_cascades_and_orphans_sessions() {
    let store = SessionStore::in_memory().unwrap();

    // A -> B -> C, plus a session parked in B and one in C.
    let a = store.insert_folder("A", None).unwrap();
    let b = store.insert_folder("B", Some(a)).unwrap();
    let c = store.insert_folder("C", Some(b)).unwrap();

    let s_in_b = store.insert_session_row("session-in-B", Some(b));
    let s_in_c = store.insert_session_row("session-in-C", Some(c));

    // Sanity: the rows are where we put them.
    assert_eq!(store.session_folder_id(s_in_b), Some(b));
    assert_eq!(store.session_folder_id(s_in_c), Some(c));

    // Delete the top of the subtree.
    store.delete_folder(a).unwrap();

    // Behavior 1 — folders.parent_id ON DELETE CASCADE: B and C
    // are gone with A (all levels, not just direct children).
    assert!(
        store.list_folders(None).unwrap().is_empty(),
        "roots must be empty after deleting A"
    );
    assert!(store.list_folders(Some(b)).unwrap().is_empty());
    assert!(store.list_folders(Some(c)).unwrap().is_empty());
    let n: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM folders", [], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 0, "folders table must be empty after cascade");

    // Behavior 2 — sessions.folder_id ON DELETE SET NULL: sessions
    // survive with folder_id = NULL (moved to root).
    assert_eq!(
        store.session_folder_id(s_in_b),
        None,
        "session in B must survive with folder_id NULL"
    );
    assert_eq!(store.session_folder_id(s_in_c), None);
}

#[test]
fn delete_childless_folder_is_noop() {
    let store = SessionStore::in_memory().unwrap();

    let id = store.insert_folder("Solo", None).unwrap();
    store.delete_folder(id).unwrap();

    assert!(store.list_folders(None).unwrap().is_empty());
    // The id is really gone, not just filtered out of the list.
    let n: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// Session CRUD (Task 5)
// ---------------------------------------------------------------------------

use crate::store::model::{
    AuthMethod, ForwardKind, ForwardRule, HostKeyPolicy, RdpOptions, SessionConfig, SessionType,
    SshOptions, VncOptions,
};

/// Build a fully-populated SSH session config for round-trip tests.
fn sample_ssh_config(folder_id: Option<i64>) -> SessionConfig {
    SessionConfig {
        id: 0,
        name: "prod-db".to_string(),
        session_type: SessionType::SSH,
        folder_id,
        host: Some("db.prod.lan".to_string()),
        port: Some(22),
        username: Some("admin".to_string()),
        auth_method: Some(AuthMethod::Key),
        key_path: Some("~/.ssh/id_ed25519".to_string()),
        host_key_policy: Some(HostKeyPolicy::AcceptNew),
        jump_session_id: None,
        forward_rules: Some(vec![
            ForwardRule {
                kind: ForwardKind::Local,
                bind_addr: "127.0.0.1".to_string(),
                bind_port: 8080,
                remote_host: "web1.lan".to_string(),
                remote_port: 80,
                enabled: true,
            },
            ForwardRule {
                kind: ForwardKind::Local,
                bind_addr: "127.0.0.1".to_string(),
                bind_port: 9090,
                remote_host: "web2.lan".to_string(),
                remote_port: 9090,
                enabled: false,
            },
        ]),
        tmux_enabled: true,
        tmux_session_name: Some("wayterm-main".to_string()),
        ssh_options: Some(SshOptions {
            compression: true,
            keepalive_interval: Some(30),
            keepalive_count: Some(3),
        }),
        rdp_domain: Some("CORP".to_string()),
        rdp_width: Some(1920),
        rdp_height: Some(1080),
        rdp_color_depth: Some(32),
        rdp_compression: true,
        rdp_options: Some(RdpOptions {
            audio: true,
            clipboard: false,
            fonts: true,
            printer: false,
        }),
        vnc_color_depth: Some(24),
        vnc_options: Some(VncOptions {
            quality: 7,
            scaling: true,
            view_only: false,
        }),
        serial_device: Some("/dev/ttyUSB0".to_string()),
        serial_baud_rate: Some(115200),
        serial_data_bits: Some(8),
        serial_parity: Some("none".to_string()),
        serial_stop_bits: Some(1),
        serial_flow_control: Some("none".to_string()),
        local_shell: Some("/bin/bash".to_string()),
        local_working_dir: Some("/srv".to_string()),
        preferred_backend: Some("vte".to_string()),
        color_scheme: Some("Dark".to_string()),
        font_family: Some("Monospace".to_string()),
        font_size: Some(12.5),
        opacity: Some(0.9),
        notes: Some("primary db".to_string()),
        last_connected: Some("2026-10-02T12:00:00Z".to_string()),
        connect_count: 17,
    }
}

/// Field-by-field equality for `SessionConfig` (no PartialEq because of floats).
fn assert_session_eq(left: &SessionConfig, right: &SessionConfig) {
    assert_eq!(left.id, right.id);
    assert_eq!(left.name, right.name);
    assert_eq!(left.session_type, right.session_type);
    assert_eq!(left.folder_id, right.folder_id);
    assert_eq!(left.host, right.host);
    assert_eq!(left.port, right.port);
    assert_eq!(left.username, right.username);
    assert_eq!(left.auth_method, right.auth_method);
    assert_eq!(left.key_path, right.key_path);
    assert_eq!(left.host_key_policy, right.host_key_policy);
    assert_eq!(left.jump_session_id, right.jump_session_id);
    assert_eq!(left.forward_rules, right.forward_rules);
    assert_eq!(left.tmux_enabled, right.tmux_enabled);
    assert_eq!(left.tmux_session_name, right.tmux_session_name);
    assert_eq!(left.ssh_options, right.ssh_options);
    assert_eq!(left.rdp_domain, right.rdp_domain);
    assert_eq!(left.rdp_width, right.rdp_width);
    assert_eq!(left.rdp_height, right.rdp_height);
    assert_eq!(left.rdp_color_depth, right.rdp_color_depth);
    assert_eq!(left.rdp_compression, right.rdp_compression);
    assert_eq!(left.rdp_options, right.rdp_options);
    assert_eq!(left.vnc_color_depth, right.vnc_color_depth);
    assert_eq!(left.vnc_options, right.vnc_options);
    assert_eq!(left.serial_device, right.serial_device);
    assert_eq!(left.serial_baud_rate, right.serial_baud_rate);
    assert_eq!(left.serial_data_bits, right.serial_data_bits);
    assert_eq!(left.serial_parity, right.serial_parity);
    assert_eq!(left.serial_stop_bits, right.serial_stop_bits);
    assert_eq!(left.serial_flow_control, right.serial_flow_control);
    assert_eq!(left.local_shell, right.local_shell);
    assert_eq!(left.local_working_dir, right.local_working_dir);
    assert_eq!(left.preferred_backend, right.preferred_backend);
    assert_eq!(left.color_scheme, right.color_scheme);
    assert_eq!(left.font_family, right.font_family);
    assert_eq!(left.font_size, right.font_size);
    assert_eq!(left.opacity, right.opacity);
    assert_eq!(left.notes, right.notes);
}

#[test]
fn session_crud_roundtrip() {
    let store = SessionStore::in_memory().unwrap();

    let mut config = sample_ssh_config(None);
    let id = store.insert_session(&config).unwrap();

    let loaded = store.get_session(id).unwrap();
    config.id = id;
    assert_session_eq(&loaded, &config);
}

#[test]
fn list_sessions_filters_by_folder() {
    let store = SessionStore::in_memory().unwrap();

    let folder = store.insert_folder("Lab", None).unwrap();

    let mut root_cfg = sample_ssh_config(None);
    root_cfg.name = "root-session".to_string();
    let root_id = store.insert_session(&root_cfg).unwrap();

    let mut folder_cfg = sample_ssh_config(Some(folder));
    folder_cfg.name = "folder-session".to_string();
    let folder_id = store.insert_session(&folder_cfg).unwrap();

    let root_sessions = store.list_sessions(None).unwrap();
    assert_eq!(root_sessions.len(), 1);
    assert_eq!(root_sessions[0].id, root_id);

    let folder_sessions = store.list_sessions(Some(folder)).unwrap();
    assert_eq!(folder_sessions.len(), 1);
    assert_eq!(folder_sessions[0].id, folder_id);
}

#[test]
fn update_session_persists() {
    let store = SessionStore::in_memory().unwrap();

    let mut config = sample_ssh_config(None);
    let id = store.insert_session(&config).unwrap();
    config.id = id;

    config.name = "prod-db-renamed".to_string();
    config.host = Some("db2.prod.lan".to_string());
    config.port = Some(2222);
    config.auth_method = Some(AuthMethod::KeyboardInteractive);
    config.tmux_enabled = false;
    config.forward_rules = Some(vec![ForwardRule {
        kind: ForwardKind::Local,
        bind_addr: "127.0.0.1".to_string(),
        bind_port: 3000,
        remote_host: "logs.prod.lan".to_string(),
        remote_port: 80,
        enabled: true,
    }]);

    store.update_session(&config).unwrap();
    let loaded = store.get_session(id).unwrap();
    assert_session_eq(&loaded, &config);
}

#[test]
fn delete_session_removes_row() {
    let store = SessionStore::in_memory().unwrap();

    let config = sample_ssh_config(None);
    let id = store.insert_session(&config).unwrap();

    store.delete_session(id).unwrap();

    assert!(store.get_session(id).is_err());
    assert!(store.list_sessions(None).unwrap().is_empty());
}

#[test]
fn touch_connected_increments() {
    let store = SessionStore::in_memory().unwrap();

    let config = sample_ssh_config(None);
    let id = store.insert_session(&config).unwrap();

    store.touch_connected(id).unwrap();
    store.touch_connected(id).unwrap();

    let loaded = store.get_session(id).unwrap();
    assert_eq!(loaded.connect_count, 2);
    assert!(loaded.last_connected.is_some());
}

#[test]
fn json_columns_survive_roundtrip() {
    let store = SessionStore::in_memory().unwrap();

    let mut config = sample_ssh_config(None);
    let id = store.insert_session(&config).unwrap();
    config.id = id;

    let loaded = store.get_session(id).unwrap();
    assert_session_eq(&loaded, &config);
}
