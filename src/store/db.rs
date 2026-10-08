//! SQLite persistence for folders, sessions, hotkeys, and settings
//! (doc 06). No credential material ever passes through this layer.

use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;

use crate::store::model::{Folder, SessionConfig};
use crate::ui::tab_state::{self, TabState};

/// A SQLite-backed store of session folders and saved sessions.
pub struct SessionStore {
    conn: Connection,
    /// `None` for an in-memory store.
    path: Option<PathBuf>,
}

impl SessionStore {
    /// Open (creating if needed) the database at `path`: pragmas, schema,
    /// and the first-run seed. Idempotent on an existing database.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        initialize(&mut conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    /// An in-memory store with the same schema and seed (tests).
    pub fn in_memory() -> rusqlite::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        initialize(&mut conn)?;
        Ok(Self { conn, path: None })
    }

    /// Another connection to the same store: the same file re-opened, or a
    /// snapshot copy of an in-memory store.
    pub fn duplicate(&self) -> rusqlite::Result<Self> {
        let Some(path) = &self.path else {
            let mut conn = Connection::open_in_memory()?;
            rusqlite::backup::Backup::new(&self.conn, &mut conn)?.run_to_completion(
                100,
                std::time::Duration::from_millis(1),
                None,
            )?;
            return Ok(Self { conn, path: None });
        };
        Self::open(path)
    }

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

    pub fn rename_folder(&self, id: i64, new_name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE folders SET name = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![new_name, id],
        )?;
        Ok(())
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

    pub fn delete_session(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Bump `connect_count` and stamp `last_connected`.
    pub fn touch_connected(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_connected = datetime('now'), \
             connect_count = connect_count + 1 WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// User-customized hotkey rows (`is_default = 0`), oldest first. Seeded
    /// `is_default = 1` rows never load over the compiled-in defaults
    /// (doc 07 §Persistence).
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

    /// A boolean settings row (see [`flag_enabled`]). A row that cannot be
    /// read counts as missing, i.e. on.
    pub fn flag(&self, key: &str) -> bool {
        let value = self.get_setting(key).unwrap_or_else(|err| {
            eprintln!("Wayterm: failed to read setting '{key}': {err}");
            None
        });
        flag_enabled(value.as_deref())
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

    /// Persist the open-tab list (doc 10 Phase 4) as one JSON settings row —
    /// it is always read and written whole. `[]` is stored, not deleted, so
    /// "no tabs open" is remembered too. Tab identity only, no credentials.
    pub fn save_open_tabs(&self, tabs: &[TabState]) -> rusqlite::Result<()> {
        let json = tab_state::tabs_to_json(tabs)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
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

fn initialize(conn: &mut Connection) -> rusqlite::Result<()> {
    // `foreign_keys` is per connection; WAL persists in the file.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(MIGRATION_001)?;
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
    terminal_backend, color_scheme, font_family, font_size, opacity, notes";
const SESSION_COLUMN_COUNT: usize = 37;

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// [`SESSION_COLUMNS`]' values for `config`. Enum and struct columns hold
/// their serde JSON (doc 06); `type` included, which is why it is stored
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
        c.notes.clone().into(),
    ])
}

fn json<T: Serialize>(value: Option<&T>) -> rusqlite::Result<Value> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map(Value::from)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn from_sql_error(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn optional_json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row,
    name: &str,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(name)?
        .map(|text| serde_json::from_str(&text).map_err(from_sql_error))
        .transpose()
}

/// Schema v1 — doc 06 §Database Schema verbatim, plus `IF NOT EXISTS`.
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
                                              -- THROUGH the referenced session (doc 11).
                                              -- v1: max depth 1, enforced in UI + connect logic.
    forward_rules   TEXT,                     -- JSON array, doc 11 §Port forwarding:
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

    -- Metadata
    last_connected   TEXT,
    connect_count    INTEGER DEFAULT 0,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    notes            TEXT
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

/// First-run seed — doc 06 §Default Data verbatim. The hotkey rows are
/// informational (`is_default = 1` rows never load); doc 07's table in
/// `hotkeys::defaults` is the source of truth.
const SEED_DEFAULTS: &str = r#"
-- Default hotkeys — MUST stay in sync with doc 07's default-actions table.
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
('default_color_scheme', 'Dark'),
('default_font_family',  'Monospace'),
('default_font_size',    '12.0'),
('default_opacity',      '1.0'),
('scrollback_limit',     '10000'),
('confirm_close',       '1'),
('reconnect_on_drop',    '1'),
('tmux_default',        '0'),
('host_key_policy',     'strict');   -- global default; per-session override via sessions.host_key_policy
"#;

fn row_to_folder(row: &rusqlite::Row) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get("id")?,
        name: row.get("name")?,
        parent_id: row.get("parent_id")?,
        sort_order: row.get("sort_order")?,
        icon: row.get("icon")?,
    })
}

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
        notes: row.get("notes")?,
        last_connected: row.get("last_connected")?,
        connect_count: row.get("connect_count")?,
    })
}

#[cfg(test)]
mod tests;
