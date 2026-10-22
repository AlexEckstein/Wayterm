//! SQLite persistence for folders, sessions, hotkeys, settings (doc 09),
//! and macros (doc 12). No credential material ever passes through this
//! layer.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;

use crate::macros::model::{steps_from_json, steps_to_json, Macro};
use crate::store::model::{Folder, HostKeyPolicy, SessionConfig};
use crate::ui::tab_state::{self, TabState};

/// The settings row holding the global host-key policy (doc 09).
pub const HOST_KEY_POLICY_SETTING: &str = "host_key_policy";

/// A SQLite-backed store of session folders and saved sessions. Not
/// `Send`: the UI shares one through an `Rc` on the main thread.
pub struct SessionStore {
    /// The SQLite connection; one per store.
    conn: Connection,
}

impl SessionStore {
    /// Open (creating if needed) the database at `path`: pragmas, schema,
    /// and the first-run seed. Idempotent on an existing database.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        initialize(&mut conn)?;
        Ok(Self { conn })
    }

    /// An in-memory store with the same schema and seed (tests).
    pub fn in_memory() -> rusqlite::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        initialize(&mut conn)?;
        Ok(Self { conn })
    }

    /// Create a folder under `parent_id` (`None` = root); returns its id.
    pub fn insert_folder(&self, name: &str, parent_id: Option<i64>) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO folders (name, parent_id) VALUES (?1, ?2)",
            params![name, parent_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Direct children of `parent_id` (`None` = root), by `sort_order` then
    /// name.
    pub fn list_folders(&self, parent_id: Option<i64>) -> rusqlite::Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, parent_id, sort_order, icon FROM folders \
             WHERE parent_id IS ?1 ORDER BY sort_order ASC, name ASC",
        )?;
        let rows = stmt.query_map([parent_id], row_to_folder)?;
        rows.collect()
    }

    /// `QueryReturnedNoRows` if `id` does not exist.
    pub fn get_folder(&self, id: i64) -> rusqlite::Result<Folder> {
        self.conn.query_row(
            "SELECT id, name, parent_id, sort_order, icon FROM folders WHERE id = ?1",
            [id],
            row_to_folder,
        )
    }

    /// Set a folder's name and icon (`None` = the default folder icon).
    pub fn update_folder(&self, id: i64, name: &str, icon: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE folders SET name = ?1, icon = ?2, updated_at = datetime('now') WHERE id = ?3",
            params![name, icon, id],
        )?;
        Ok(())
    }

    /// Move a folder under `parent` (`None` = root), sub-folders and
    /// sessions along with it. Returns false, changing nothing, if `parent`
    /// is the folder itself or one of its sub-folders (or `id` does not
    /// exist).
    pub fn move_folder(&self, id: i64, parent: Option<i64>) -> rusqlite::Result<bool> {
        // `ancestors` is `parent` and every folder above it; UNION (not
        // UNION ALL) also ends the walk on an already-cyclic chain.
        let changed = self.conn.execute(
            "WITH RECURSIVE ancestors(id) AS ( \
                 SELECT ?2 \
                 UNION SELECT f.parent_id FROM folders f JOIN ancestors a ON f.id = a.id \
             ) \
             UPDATE folders SET parent_id = ?2, updated_at = datetime('now') \
             WHERE id = ?1 AND ?1 NOT IN (SELECT id FROM ancestors WHERE id IS NOT NULL)",
            params![id, parent],
        )?;
        Ok(changed > 0)
    }

    /// Delete a folder. The schema cascades to sub-folders and moves the
    /// sessions inside to the root (`ON DELETE SET NULL`).
    pub fn delete_folder(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM folders WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Insert a session and return its id.
    pub fn insert_session(&self, config: &SessionConfig) -> rusqlite::Result<i64> {
        let sql = format!(
            "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES ({})",
            placeholders(SESSION_COLUMN_COUNT)
        );
        self.conn
            .execute(&sql, params_from_iter(session_values(config)?))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// `QueryReturnedNoRows` if `id` does not exist.
    pub fn get_session(&self, id: i64) -> rusqlite::Result<SessionConfig> {
        self.conn
            .query_row("SELECT * FROM sessions WHERE id = ?1", [id], row_to_session)
    }

    /// Sessions directly in `folder_id` (`None` = root), by `sort_order`
    /// then name.
    pub fn list_sessions(&self, folder_id: Option<i64>) -> rusqlite::Result<Vec<SessionConfig>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM sessions WHERE folder_id IS ?1 ORDER BY sort_order ASC, name ASC",
        )?;
        let rows = stmt.query_map([folder_id], row_to_session)?;
        rows.collect()
    }

    /// Persist `config` over the row with its id. `connect_count` and
    /// `last_connected` are left alone (see [`Self::touch_connected`]).
    pub fn update_session(&self, config: &SessionConfig) -> rusqlite::Result<()> {
        let sql = format!(
            "UPDATE sessions SET ({SESSION_COLUMNS}) = ({}), updated_at = datetime('now') \
             WHERE id = ?",
            placeholders(SESSION_COLUMN_COUNT)
        );
        let mut values = session_values(config)?.to_vec();
        values.push(config.id.into());
        self.conn.execute(&sql, params_from_iter(values))?;
        Ok(())
    }

    /// Delete a session; its history goes with it.
    pub fn delete_session(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Bump `connect_count` and stamp `last_connected`. Nothing in the app
    /// calls it (doc 09 §Limitations).
    pub fn touch_connected(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_connected = datetime('now'), \
             connect_count = connect_count + 1 WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Every macro (doc 12), by name — case-insensitive — then id.
    pub fn list_macros(&self) -> rusqlite::Result<Vec<Macro>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, steps FROM macros ORDER BY name COLLATE NOCASE ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_macro)?;
        rows.collect()
    }

    /// Insert a macro (its `id` is ignored) and return the new id.
    pub fn insert_macro(&self, m: &Macro) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO macros (name, steps) VALUES (?1, ?2)",
            params![m.name, macro_steps(m)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Persist `m` over the row with its id.
    pub fn update_macro(&self, m: &Macro) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE macros SET name = ?1, steps = ?2, updated_at = datetime('now') \
             WHERE id = ?3",
            params![m.name, macro_steps(m)?, m.id],
        )?;
        Ok(())
    }

    /// Delete a macro.
    pub fn delete_macro(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM macros WHERE id = ?1", [id])?;
        Ok(())
    }

    /// User-customized hotkey rows (`is_default = 0`), oldest first. Seeded
    /// `is_default = 1` rows never load over the compiled-in defaults
    /// (doc 11 §Persistence).
    pub fn load_custom_hotkeys(&self) -> rusqlite::Result<Vec<CustomHotkeyRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT action, key_sequence, enabled FROM hotkeys \
             WHERE is_default = 0 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i32>(2)? != 0))
        })?;
        rows.collect()
    }

    /// Write one customized hotkey row, replacing any earlier one for the
    /// action. The schema has no `UNIQUE (action, context)` yet, so the
    /// delete and insert share a `BEGIN IMMEDIATE` transaction to keep two
    /// writers from interleaving into duplicates.
    pub fn save_custom_hotkey(
        &self,
        action: &str,
        key_sequence: &str,
        context: &str,
        enabled: bool,
    ) -> rusqlite::Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM hotkeys WHERE action = ?1 AND is_default = 0",
            [action],
        )?;
        tx.execute(
            "INSERT INTO hotkeys (action, key_sequence, context, enabled, is_default) \
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![action, key_sequence, context, enabled],
        )?;
        tx.commit()
    }

    /// Drop an action's customized row (it is back at its default);
    /// returns the number of rows removed.
    pub fn delete_custom_hotkey(&self, action: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM hotkeys WHERE action = ?1 AND is_default = 0",
            [action],
        )
    }

    /// One `settings` row; `Ok(None)` when absent.
    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
    }

    /// One `settings` row; `None` when absent or unreadable (logged), for
    /// the readers whose default covers both.
    pub fn setting(&self, key: &str) -> Option<String> {
        self.get_setting(key).unwrap_or_else(|err| {
            eprintln!("Wayterm: failed to read setting '{key}': {err}");
            None
        })
    }

    /// A boolean settings row (see [`flag_enabled`]). A row that cannot be
    /// read counts as missing, i.e. on.
    pub fn flag(&self, key: &str) -> bool {
        flag_enabled(self.setting(key).as_deref())
    }

    /// The global host-key policy (doc 04), which a session without its
    /// own follows. A missing, unreadable or unknown row is strict.
    pub fn host_key_policy(&self) -> HostKeyPolicy {
        match self
            .setting(HOST_KEY_POLICY_SETTING)
            .as_deref()
            .map(str::trim)
        {
            Some("accept-new") => HostKeyPolicy::AcceptNew,
            Some("off") => HostKeyPolicy::Off,
            _ => HostKeyPolicy::Strict,
        }
    }

    /// Upsert one `settings` row.
    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Persist the open-tab list (doc 03 §Restoring tabs) as one JSON settings row —
    /// it is always read and written whole. `[]` is stored, not deleted, so
    /// "no tabs open" is remembered too. Tab identity only, no credentials.
    pub fn save_open_tabs(&self, tabs: &[TabState]) -> rusqlite::Result<()> {
        let json = tab_state::tabs_to_json(tabs).map_err(to_sql_error)?;
        self.set_setting(tab_state::OPEN_TABS_SETTING_KEY, &json)
    }

    /// The saved open-tab list; empty when never saved. Malformed JSON is an
    /// error, not an empty list, so the caller can tell the two apart.
    pub fn load_open_tabs(&self) -> rusqlite::Result<Vec<TabState>> {
        match self.get_setting(tab_state::OPEN_TABS_SETTING_KEY)? {
            None => Ok(Vec::new()),
            Some(json) => tab_state::tabs_from_json(&json).map_err(from_sql_error),
        }
    }

    /// The stored RDP server-certificate fingerprint for `(host, port)`
    /// (`rdp_trust` table); `Ok(None)` when never seen. Backs the TOFU
    /// resolver in `rdp::cert`.
    pub fn rdp_trust(&self, host: &str, port: u16) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT fingerprint FROM rdp_trust WHERE host = ?1 AND port = ?2",
                params![host, i64::from(port)],
                |row| row.get(0),
            )
            .optional()
    }

    /// Record (insert or replace) the fingerprint for `(host, port)`.
    /// `first_seen` is stamped on first insert and kept on later updates.
    pub fn rdp_trust_record(
        &self,
        host: &str,
        port: u16,
        fingerprint: &str,
    ) -> rusqlite::Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO rdp_trust (host, port, fingerprint, first_seen) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(host, port) DO UPDATE SET fingerprint = excluded.fingerprint",
            params![host, i64::from(port), fingerprint, now],
        )?;
        Ok(())
    }
}

#[cfg(test)]
impl SessionStore {
    /// The seeded `is_default = 1` hotkey rows as `(action, key, context)`.
    pub(crate) fn seeded_hotkeys(&self) -> rusqlite::Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT action, key_sequence, context FROM hotkeys WHERE is_default = 1 ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    }
}

/// Parse a boolean settings value. Every flag Wayterm stores defaults to on,
/// so a missing row is on, and only an explicit `0`/`false`/`no`/`off` (any
/// case, surrounding blanks ignored) turns one off — a typo never silently
/// disables a safety prompt.
pub fn flag_enabled(raw: Option<&str>) -> bool {
    !raw.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// `(action, key_sequence, enabled)` of one `is_default = 0` hotkey row.
pub type CustomHotkeyRow = (String, String, bool);

/// Pragmas, every idempotent schema step, and the seed on a fresh store.
fn initialize(conn: &mut Connection) -> rusqlite::Result<()> {
    // `foreign_keys` is per connection; WAL persists in the file.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(MIGRATION_001)?;
    conn.execute_batch(MIGRATION_002)?;
    migrate_003(conn)?;
    migrate_004(conn)?;
    conn.execute_batch(MIGRATION_005)?;
    // `hotkeys` has no UNIQUE constraint, so re-running the seed would
    // duplicate rows: seed exactly once, atomically, keyed on an empty
    // `settings` table.
    let seeded: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM settings)", [], |row| {
        row.get(0)
    })?;
    if !seeded {
        let tx = conn.transaction()?;
        tx.execute_batch(SEED_DEFAULTS)?;
        tx.commit()?;
    }
    Ok(())
}

/// `sessions` columns written by insert/update, in parameter order.
const SESSION_COLUMNS: &str = "name, type, folder_id, sort_order, \
    host, port, username, auth_method, host_key_policy, \
    key_path, tmux_enabled, tmux_session_name, ssh_options, jump_session_id, forward_rules, \
    rdp_domain, rdp_width, rdp_height, rdp_color_depth, rdp_compression, rdp_options, \
    vnc_color_depth, vnc_options, \
    serial_device, serial_baud_rate, serial_data_bits, serial_parity, serial_stop_bits, \
    serial_flow_control, local_shell, local_working_dir, \
    terminal_backend, color_scheme, font_family, font_size, opacity, icon";
/// How many columns [`SESSION_COLUMNS`] lists.
const SESSION_COLUMN_COUNT: usize = 37;

/// `count` comma-separated `?` placeholders.
fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// [`SESSION_COLUMNS`]' values for `config`. Enum and struct columns hold
/// their serde JSON (doc 09); `type` included, which is why it is stored
/// quoted.
fn session_values(c: &SessionConfig) -> rusqlite::Result<[Value; SESSION_COLUMN_COUNT]> {
    Ok([
        c.name.clone().into(),
        json(Some(&c.session_type))?,
        c.folder_id.into(),
        0.into(), // sort_order: not editable yet
        c.host.clone().into(),
        c.port.into(),
        c.username.clone().into(),
        json(c.auth_method.as_ref())?,
        json(c.host_key_policy.as_ref())?,
        c.key_path.clone().into(),
        c.tmux_enabled.into(),
        c.tmux_session_name.clone().into(),
        json(c.ssh_options.as_ref())?,
        c.jump_session_id.into(),
        json(c.forward_rules.as_ref())?,
        c.rdp_domain.clone().into(),
        c.rdp_width.into(),
        c.rdp_height.into(),
        c.rdp_color_depth.into(),
        c.rdp_compression.into(),
        json(c.rdp_options.as_ref())?,
        c.vnc_color_depth.into(),
        json(c.vnc_options.as_ref())?,
        c.serial_device.clone().into(),
        c.serial_baud_rate.into(),
        c.serial_data_bits.into(),
        c.serial_parity.clone().into(),
        c.serial_stop_bits.into(),
        c.serial_flow_control.clone().into(),
        c.local_shell.clone().into(),
        c.local_working_dir.clone().into(),
        c.preferred_backend.clone().into(),
        c.color_scheme.clone().into(),
        c.font_family.clone().into(),
        c.font_size.into(),
        c.opacity.into(),
        c.icon.clone().into(),
    ])
}

/// `value` as a JSON text column, or NULL.
fn json<T: Serialize>(value: Option<&T>) -> rusqlite::Result<Value> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map(Value::from)
        .map_err(to_sql_error)
}

/// The `macros.steps` column for `m`.
fn macro_steps(m: &Macro) -> rusqlite::Result<String> {
    steps_to_json(&m.steps).map_err(to_sql_error)
}

/// A JSON encode error as a rusqlite error.
fn to_sql_error(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// A JSON decode error as a rusqlite error.
fn from_sql_error(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

/// Decode a nullable JSON column.
fn optional_json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row,
    name: &str,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(name)?
        .map(|text| serde_json::from_str(&text).map_err(from_sql_error))
        .transpose()
}

/// Schema v1 — doc 09 §Schema, plus `IF NOT EXISTS`.
const MIGRATION_001: &str = r#"
-- Session folders for tree organization
CREATE TABLE IF NOT EXISTS folders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    parent_id   INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    sort_order  INTEGER DEFAULT 0,
    icon        TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Saved sessions (SSH, RDP, VNC, serial, local shell)
CREATE TABLE IF NOT EXISTS sessions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id       INTEGER REFERENCES folders(id) ON DELETE SET NULL,
    name            TEXT NOT NULL,
    type            TEXT NOT NULL,           -- 'ssh', 'rdp', 'vnc', 'serial', 'local'
    sort_order      INTEGER DEFAULT 0,

    -- Common connection fields
    host            TEXT,
    port            INTEGER,
    username        TEXT,
    auth_method     TEXT,                     -- 'password', 'key', 'agent', 'keyboard-interactive', 'none'
    host_key_policy TEXT,                     -- NULL = use global setting; 'strict', 'accept-new', 'off'

    -- SSH-specific
    key_path        TEXT,
    tmux_enabled    INTEGER DEFAULT 0,
    tmux_session_name TEXT,
    ssh_options     TEXT,                     -- JSON: compression, keepalive, etc.
    jump_session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
                                              -- ProxyJump-style hop: this session connects
                                              -- THROUGH the referenced session (doc 04).
                                              -- v1: max depth 1, enforced in UI + connect logic.
    forward_rules   TEXT,                     -- JSON array, doc 04 §Port forwarding:
                                              -- [{type:"local", bind:"127.0.0.1:8080",
                                              --   remote_host:"web1.lan", remote_port:80, enabled:true}]

    -- RDP-specific
    rdp_domain      TEXT,
    rdp_width       INTEGER,
    rdp_height      INTEGER,
    rdp_color_depth INTEGER,
    rdp_compression INTEGER DEFAULT 1,
    rdp_options     TEXT,                     -- JSON: audio, clipboard, fonts, etc.

    -- VNC-specific
    vnc_color_depth INTEGER,
    vnc_options     TEXT,                     -- JSON: quality, scaling, etc.

    -- Serial-specific
    serial_device    TEXT,
    serial_baud_rate INTEGER,
    serial_data_bits INTEGER DEFAULT 8,
    serial_parity    TEXT DEFAULT 'none',
    serial_stop_bits INTEGER DEFAULT 1,
    serial_flow_control TEXT DEFAULT 'none',

    -- Local shell
    local_shell      TEXT,
    local_working_dir TEXT,

    -- Terminal appearance (per-session override, empty = use global)
    terminal_backend TEXT,
    color_scheme     TEXT,
    font_family      TEXT,
    font_size        REAL,
    opacity          REAL,
    icon             TEXT,                    -- icon name; NULL = the type's default

    -- Metadata
    last_connected   TEXT,
    connect_count    INTEGER DEFAULT 0,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Global hotkey bindings
CREATE TABLE IF NOT EXISTS hotkeys (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    action      TEXT NOT NULL,
    key_sequence TEXT NOT NULL,
    context     TEXT DEFAULT 'global',
    enabled     INTEGER DEFAULT 1,
    is_default  INTEGER DEFAULT 1
);

-- Global settings (key-value)
CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT
);

-- Connection history / recent sessions
CREATE TABLE IF NOT EXISTS connection_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    connected_at TEXT NOT NULL DEFAULT (datetime('now')),
    duration    INTEGER,
    bytes_sent  INTEGER,
    bytes_received INTEGER
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_sessions_folder ON sessions(folder_id);
CREATE INDEX IF NOT EXISTS idx_sessions_type ON sessions(type);
CREATE INDEX IF NOT EXISTS idx_sessions_host ON sessions(host);
CREATE INDEX IF NOT EXISTS idx_folders_parent ON folders(parent_id);
CREATE INDEX IF NOT EXISTS idx_hotkeys_action ON hotkeys(action);
CREATE INDEX IF NOT EXISTS idx_history_session ON connection_history(session_id);
CREATE INDEX IF NOT EXISTS idx_history_recent ON connection_history(connected_at DESC);
"#;

/// Schema v2 — RDP server-certificate trust (TOFU fingerprints, one row per
/// endpoint). Idempotent like v1 (`IF NOT EXISTS`), so existing databases
/// migrate on open.
const MIGRATION_002: &str = r#"
CREATE TABLE IF NOT EXISTS rdp_trust (
    host        TEXT NOT NULL,
    port        INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    first_seen  INTEGER NOT NULL,
    PRIMARY KEY (host, port)
);
"#;

/// Schema v3 — `sessions.icon` (doc 09). `CREATE TABLE IF NOT EXISTS`
/// leaves an existing table alone, and `ADD COLUMN` has no `IF NOT
/// EXISTS`, so the column is added only where it is missing.
fn migrate_003(conn: &Connection) -> rusqlite::Result<()> {
    if !has_column(conn, "sessions", "icon")? {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN icon TEXT;")?;
    }
    Ok(())
}

/// Schema v4 — drop `sessions.notes` (doc 09). A database created before
/// v4 still has the column; one created since never did.
fn migrate_004(conn: &Connection) -> rusqlite::Result<()> {
    if has_column(conn, "sessions", "notes")? {
        conn.execute_batch("ALTER TABLE sessions DROP COLUMN notes;")?;
    }
    Ok(())
}

/// Whether `table` has `column`.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        [table, column],
        |row| row.get(0),
    )
}

/// Schema v5 — keystroke macros (doc 12): `steps` is a JSON array of
/// `{"delay_ms": …, "keys": [bytes]}`. Idempotent like v1.
const MIGRATION_005: &str = r#"
CREATE TABLE IF NOT EXISTS macros (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    steps       TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

/// First-run seed (doc 13 §Settings rows). The hotkey rows are
/// informational (`is_default = 1` rows never load); doc 11's table in
/// `hotkeys::defaults` is the source of truth.
const SEED_DEFAULTS: &str = r#"
-- Default hotkeys — MUST stay in sync with doc 11's default-actions table.
-- (These rows are the seed data for is_default=1; user rebinding writes
-- is_default=0 rows which override at load time.)
INSERT INTO hotkeys (action, key_sequence, context, is_default) VALUES
('new_tab',           'Ctrl+Shift+T',    'global',       1),
('close_tab',         'Ctrl+Shift+W',    'global',       1),
('next_tab',          'Ctrl+Tab',        'global',       1),
('prev_tab',          'Ctrl+Shift+Tab',  'global',       1),
('toggle_sftp',       'Ctrl+Shift+B',    'global',       1),
('toggle_session_tree','Ctrl+Shift+S',   'global',       1),
('toggle_fullscreen', 'F11',             'global',       1),
('reconnect',         'Ctrl+Shift+R',    'global',       1),
('disconnect',        'Ctrl+Shift+X',    'global',       1),
('new_session',       'Ctrl+N',          'global',       1),
('rename_tab',        'Ctrl+Shift+Enter','global',       1),
('split_horizontal',  'Ctrl+Shift+D',    'terminal',     1),
('split_vertical',    'Ctrl+Shift+E',    'terminal',     1),
('close_split',       'Ctrl+Shift+Q',    'terminal',     1),
('search',            'Ctrl+Shift+F',    'terminal',     1),
('search_scrollback', 'Ctrl+Shift+K',    'terminal',     1),
('copy',              'Ctrl+Shift+C',    'terminal',     1),
('paste',             'Ctrl+Shift+V',    'terminal',     1),
('select_all',        'Ctrl+Shift+A',    'terminal',     1),
('clear',             'Ctrl+Shift+L',    'terminal',     1),
('edit_session',      'Ctrl+E',          'session_tree', 1),
('duplicate_session', 'Ctrl+D',         'session_tree', 1),
('delete_session',    'Delete',          'session_tree', 1),
('connect_session',   'Enter',          'session_tree', 1),
('sftp_upload',       'Ctrl+U',          'sftp',         1),
('sftp_download',     'Ctrl+D',         'sftp',         1),
('sftp_rename',       'F2',              'sftp',         1),
('sftp_delete',       'Delete',          'sftp',         1);

-- Default settings
INSERT INTO settings (key, value) VALUES
('default_backend',     'vte'),
('default_color_scheme', 'System'),
('default_font_family',  'Monospace'),
('default_font_size',    '12.0'),
('default_opacity',      '1.0'),
('scrollback_limit',     '10000'),
('confirm_close',       '1'),
('reconnect_on_drop',    '1'),
('tmux_default',        '0'),
('host_key_policy',     'strict');   -- global default; per-session override via sessions.host_key_policy
"#;

/// A `folders` row.
fn row_to_folder(row: &rusqlite::Row) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get("id")?,
        name: row.get("name")?,
        parent_id: row.get("parent_id")?,
        sort_order: row.get("sort_order")?,
        icon: row.get("icon")?,
    })
}

/// A `macros` row.
fn row_to_macro(row: &rusqlite::Row) -> rusqlite::Result<Macro> {
    Ok(Macro {
        id: row.get("id")?,
        name: row.get("name")?,
        steps: steps_from_json(&row.get::<_, String>("steps")?).map_err(from_sql_error)?,
    })
}

/// A `sessions` row.
fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<SessionConfig> {
    Ok(SessionConfig {
        id: row.get("id")?,
        name: row.get("name")?,
        session_type: serde_json::from_str(&row.get::<_, String>("type")?)
            .map_err(from_sql_error)?,
        folder_id: row.get("folder_id")?,
        host: row.get("host")?,
        port: row.get("port")?,
        username: row.get("username")?,
        auth_method: optional_json_column(row, "auth_method")?,
        key_path: row.get("key_path")?,
        host_key_policy: optional_json_column(row, "host_key_policy")?,
        jump_session_id: row.get("jump_session_id")?,
        forward_rules: optional_json_column(row, "forward_rules")?,
        tmux_enabled: row.get("tmux_enabled")?,
        tmux_session_name: row.get("tmux_session_name")?,
        ssh_options: optional_json_column(row, "ssh_options")?,
        rdp_domain: row.get("rdp_domain")?,
        rdp_width: row.get("rdp_width")?,
        rdp_height: row.get("rdp_height")?,
        rdp_color_depth: row.get("rdp_color_depth")?,
        rdp_compression: row.get("rdp_compression")?,
        rdp_options: optional_json_column(row, "rdp_options")?,
        vnc_color_depth: row.get("vnc_color_depth")?,
        vnc_options: optional_json_column(row, "vnc_options")?,
        serial_device: row.get("serial_device")?,
        serial_baud_rate: row.get("serial_baud_rate")?,
        serial_data_bits: row.get("serial_data_bits")?,
        serial_parity: row.get("serial_parity")?,
        serial_stop_bits: row.get("serial_stop_bits")?,
        serial_flow_control: row.get("serial_flow_control")?,
        local_shell: row.get("local_shell")?,
        local_working_dir: row.get("local_working_dir")?,
        preferred_backend: row.get("terminal_backend")?,
        color_scheme: row.get("color_scheme")?,
        font_family: row.get("font_family")?,
        font_size: row.get("font_size")?,
        opacity: row.get("opacity")?,
        icon: row.get("icon")?,
        last_connected: row.get("last_connected")?,
        connect_count: row.get("connect_count")?,
    })
}

#[cfg(test)]
mod tests;
