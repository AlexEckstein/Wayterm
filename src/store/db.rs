//! SQLite persistence for saved sessions (doc 06).
//!
//! [`SessionStore::open`] creates the database file if needed, applies
//! the schema (§"Database Schema") and default seed data (§"Default
//! Data") — both transcribed verbatim — and arms the WAL and
//! foreign-keys pragmas.
//!
//! No credential material ever passes through this layer (doc 06
//! §"Credential Storage"): the database stores connection parameters
//! only; secrets live in the OS keyring (libsecret, doc 11).

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::store::model::{Folder, SessionConfig};

/// A SQLite-backed store of session folders and saved sessions.
pub struct SessionStore {
    conn: Connection,
    /// `Some(path)` for a file-backed store, `None` for an in-memory store.
    /// Duplicating a file-backed store re-opens the same file so that UI
    /// handlers and the tree refresh see the same persisted data.
    path: Option<PathBuf>,
}

impl SessionStore {
    /// Open the store at `path`, creating the file (parent directory
    /// must exist), the schema, and the default seed data on first
    /// open.
    ///
    /// Calling `open` again on an existing database succeeds: schema
    /// creation is `IF NOT EXISTS` and the seed runs only when the
    /// defaults are not already present.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        initialize(&mut conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    /// Open an in-memory store with the same schema and seed data.
    ///
    /// For tests and CI. `journal_mode=WAL` is a no-op on `:memory:`
    /// databases (SQLite reports `memory`); everything else is
    /// identical to a file-backed store.
    pub fn in_memory() -> rusqlite::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        initialize(&mut conn)?;
        Ok(Self { conn, path: None })
    }

    /// Return another connection to the same store.
    ///
    /// For a file-backed store this re-opens the same database file (with
    /// WAL + foreign keys enabled), so mutations made on one connection are
    /// visible to the other. For an in-memory store this copies the current
    /// state into a new `:memory:` database, which is the best isolation the
    /// SQLite backup API allows for ephemeral stores.
    ///
    /// The UI uses this to give GTK reference-counted closures their own
    /// connection while keeping the store single-threaded (the underlying
    /// `rusqlite::Connection` is neither `Send` nor `Sync`).
    pub fn duplicate(&self) -> rusqlite::Result<Self> {
        match &self.path {
            Some(path) => Self::open(path),
            None => {
                let mut new_conn = Connection::open_in_memory()?;
                {
                    let backup = rusqlite::backup::Backup::new(&self.conn, &mut new_conn)?;
                    backup.run_to_completion(100, std::time::Duration::from_millis(1), None)?;
                }
                Ok(Self {
                    conn: new_conn,
                    path: None,
                })
            }
        }
    }

    /// Insert a folder and return its generated row id.
    ///
    /// `parent_id` is `None` for a root folder. The new row starts
    /// with `sort_order = 0` and `updated_at` from the default
    /// trigger-equivalent (doc 06 §"Database Schema").
    pub fn insert_folder(&self, name: &str, parent_id: Option<i64>) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO folders (name, parent_id) VALUES (?1, ?2)",
            params![name, parent_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List the direct children of a folder.
    ///
    /// `parent_id = None` lists root folders. Results are ordered by
    /// `sort_order` ascending, then `name` ascending — the contract
    /// the session-tree UI relies on (doc 06 §"Database Operations").
    pub fn list_folders(&self, parent_id: Option<i64>) -> rusqlite::Result<Vec<Folder>> {
        let sql = match parent_id {
            Some(_) => {
                "SELECT id, name, parent_id, sort_order, icon \
                 FROM folders WHERE parent_id = ?1 \
                 ORDER BY sort_order ASC, name ASC"
            }
            None => {
                "SELECT id, name, parent_id, sort_order, icon \
                 FROM folders WHERE parent_id IS NULL \
                 ORDER BY sort_order ASC, name ASC"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match parent_id {
            Some(pid) => stmt.query_map(params![pid], row_to_folder)?,
            None => stmt.query_map([], row_to_folder)?,
        };
        rows.collect()
    }

    /// Rename an existing folder.
    ///
    /// Re-running with the same name is a no-op update; updating a
    /// non-existent id silently affects zero rows, matching the simple
    /// contract used in doc 06 §"Database Operations".
    pub fn rename_folder(&self, id: i64, new_name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE folders SET name = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![new_name, id],
        )?;
        Ok(())
    }

    /// Delete a folder, cascading to sub-folders and orphaning any
    /// sessions that lived inside it.
    ///
    /// The cascade comes from the schema itself:
    /// `folders.parent_id ON DELETE CASCADE` recursively removes
    /// descendant folders, while `sessions.folder_id ON DELETE SET
    /// NULL` preserves the sessions at the root (doc 06 §"Database
    /// Schema").
    pub fn delete_folder(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM folders WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Insert a saved session and return its generated row id.
    ///
    /// JSON columns (`ssh_options`, `rdp_options`, `vnc_options`,
    /// `forward_rules`) are serialized via serde. `session_type` and
    /// `auth_method` store their serde wire strings (doc 06
    /// §"Session Config Struct"). No credential material is stored.
    pub fn insert_session(&self, config: &SessionConfig) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (
                name, type, folder_id, sort_order,
                host, port, username, auth_method, host_key_policy,
                key_path, tmux_enabled, tmux_session_name, ssh_options,
                jump_session_id, forward_rules,
                rdp_domain, rdp_width, rdp_height, rdp_color_depth, rdp_compression, rdp_options,
                vnc_color_depth, vnc_options,
                serial_device, serial_baud_rate, serial_data_bits, serial_parity,
                serial_stop_bits, serial_flow_control,
                local_shell, local_working_dir,
                terminal_backend, color_scheme, font_family, font_size, opacity,
                notes
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21,
                ?22, ?23,
                ?24, ?25, ?26, ?27,
                ?28, ?29,
                ?30, ?31,
                ?32, ?33, ?34, ?35, ?36,
                ?37
            )",
            params![
                config.name,
                serde_json::to_string(&config.session_type)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                config.folder_id,
                0,
                config.host,
                config.port,
                config.username,
                config
                    .auth_method
                    .as_ref()
                    .map(|a| serde_json::to_string(a).unwrap()),
                config
                    .host_key_policy
                    .as_ref()
                    .map(|p| serde_json::to_string(p).unwrap()),
                config.key_path,
                config.tmux_enabled as i32,
                config.tmux_session_name,
                config
                    .ssh_options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap()),
                config.jump_session_id,
                config
                    .forward_rules
                    .as_ref()
                    .map(|r| serde_json::to_string(r).unwrap()),
                config.rdp_domain,
                config.rdp_width,
                config.rdp_height,
                config.rdp_color_depth,
                config.rdp_compression as i32,
                config
                    .rdp_options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap()),
                config.vnc_color_depth,
                config
                    .vnc_options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap()),
                config.serial_device,
                config.serial_baud_rate,
                config.serial_data_bits,
                config.serial_parity,
                config.serial_stop_bits,
                config.serial_flow_control,
                config.local_shell,
                config.local_working_dir,
                config.preferred_backend,
                config.color_scheme,
                config.font_family,
                config.font_size,
                config.opacity,
                config.notes,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read one saved session by id.
    ///
    /// Returns `QueryReturnedNoRows` if the id does not exist.
    pub fn get_session(&self, id: i64) -> rusqlite::Result<SessionConfig> {
        self.conn.query_row(
            "SELECT * FROM sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
    }

    /// List direct children of a folder, ordered by `sort_order` then `name`.
    ///
    /// `folder_id = None` lists root sessions (rows where `folder_id IS NULL`).
    pub fn list_sessions(&self, folder_id: Option<i64>) -> rusqlite::Result<Vec<SessionConfig>> {
        let sql = match folder_id {
            Some(_) => {
                "SELECT * FROM sessions WHERE folder_id = ?1 ORDER BY sort_order ASC, name ASC"
            }
            None => {
                "SELECT * FROM sessions WHERE folder_id IS NULL ORDER BY sort_order ASC, name ASC"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match folder_id {
            Some(fid) => stmt.query_map(params![fid], row_to_session)?,
            None => stmt.query_map([], row_to_session)?,
        };
        rows.collect()
    }

    /// Persist changes to an existing session.
    ///
    /// The row is matched by `config.id`. `updated_at` is refreshed to
    /// `datetime('now')`; `connect_count` and `last_connected` are left
    /// untouched (use [`touch_connected`] to bump them).
    pub fn update_session(&self, config: &SessionConfig) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET
                name = ?1, type = ?2, folder_id = ?3, sort_order = ?4,
                host = ?5, port = ?6, username = ?7, auth_method = ?8, host_key_policy = ?9,
                key_path = ?10, tmux_enabled = ?11, tmux_session_name = ?12, ssh_options = ?13,
                jump_session_id = ?14, forward_rules = ?15,
                rdp_domain = ?16, rdp_width = ?17, rdp_height = ?18, rdp_color_depth = ?19,
                rdp_compression = ?20, rdp_options = ?21,
                vnc_color_depth = ?22, vnc_options = ?23,
                serial_device = ?24, serial_baud_rate = ?25, serial_data_bits = ?26,
                serial_parity = ?27, serial_stop_bits = ?28, serial_flow_control = ?29,
                local_shell = ?30, local_working_dir = ?31,
                terminal_backend = ?32, color_scheme = ?33, font_family = ?34, font_size = ?35,
                opacity = ?36,
                notes = ?37,
                updated_at = datetime('now')
             WHERE id = ?38",
            params![
                config.name,
                serde_json::to_string(&config.session_type)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                config.folder_id,
                0,
                config.host,
                config.port,
                config.username,
                config
                    .auth_method
                    .as_ref()
                    .map(|a| serde_json::to_string(a).unwrap()),
                config
                    .host_key_policy
                    .as_ref()
                    .map(|p| serde_json::to_string(p).unwrap()),
                config.key_path,
                config.tmux_enabled as i32,
                config.tmux_session_name,
                config
                    .ssh_options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap()),
                config.jump_session_id,
                config
                    .forward_rules
                    .as_ref()
                    .map(|r| serde_json::to_string(r).unwrap()),
                config.rdp_domain,
                config.rdp_width,
                config.rdp_height,
                config.rdp_color_depth,
                config.rdp_compression as i32,
                config
                    .rdp_options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap()),
                config.vnc_color_depth,
                config
                    .vnc_options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap()),
                config.serial_device,
                config.serial_baud_rate,
                config.serial_data_bits,
                config.serial_parity,
                config.serial_stop_bits,
                config.serial_flow_control,
                config.local_shell,
                config.local_working_dir,
                config.preferred_backend,
                config.color_scheme,
                config.font_family,
                config.font_size,
                config.opacity,
                config.notes,
                config.id,
            ],
        )?;
        Ok(())
    }

    /// Delete a saved session by id.
    pub fn delete_session(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Mark a session as just-connected.
    ///
    /// Bumps `connect_count` and sets `last_connected = datetime('now')`.
    pub fn touch_connected(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_connected = datetime('now'), \
             connect_count = connect_count + 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Read the user-customized hotkey rows: `WHERE is_default = 0`,
    /// oldest first (doc 07 §"Persistence", step 2).
    ///
    /// Rows seeded with `is_default = 1` are first-run data and are
    /// never returned — doc 07 is explicit that they "are never loaded
    /// back over the registry's compiled-in defaults", and filtering
    /// here is what guarantees that even if a seed row is edited by
    /// hand. `context` is not returned: doc 07 applies a row by action
    /// id, and the context is a property of the registered action.
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

    /// Write one user-customized hotkey row (`is_default = 0`).
    ///
    /// Doc 06 §"Default Data": "user rebinding writes is_default = 0
    /// rows which override at load time". Rebinding the same action
    /// twice replaces the row rather than appending a second one, which
    /// is what the missing `UNIQUE (action, context)` constraint cannot
    /// enforce on its own: the delete and the insert share one
    /// `BEGIN IMMEDIATE` transaction, so the write lock is taken before
    /// the read of the row set and two writers cannot interleave into
    /// duplicates. `UNIQUE (action, context)` remains doc 06's
    /// schema-v2 candidate; `db.rs` still has a single
    /// `CREATE TABLE IF NOT EXISTS` migration and no version table, so
    /// adding a second one is a migration-system decision, not a hotkey
    /// one.
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
            params![action],
        )?;
        tx.execute(
            "INSERT INTO hotkeys (action, key_sequence, context, enabled, is_default) \
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![action, key_sequence, context, enabled as i32],
        )?;
        tx.commit()
    }

    /// Drop a user-customized hotkey row, returning the number of rows
    /// removed.
    ///
    /// Called when an action is back at its compiled-in binding: with no
    /// `is_default = 0` row, [`Self::load_custom_hotkeys`] has nothing to
    /// say about it and the compiled-in default applies again. Seeded
    /// `is_default = 1` rows are never in scope for this statement.
    pub fn delete_custom_hotkey(&self, action: &str) -> rusqlite::Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM hotkeys WHERE action = ?1 AND is_default = 0",
            params![action],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Read one row from the key-value `settings` table (doc 06).
    ///
    /// `Ok(None)` means the key is absent, which is distinct from an
    /// `Err` (the database is unreadable) — callers that need a default
    /// supply it themselves, so a missing row never silently becomes an
    /// empty string.
    /// Read a global setting from the `settings` key-value table.
    ///
    /// Returns `Ok(None)` when the row is absent — callers apply the
    /// documented default (e.g. `tray::minimize_to_tray_enabled` treats a
    /// missing `minimize_to_tray` row as ON). Added for P4-5; previously
    /// only the db tests had a private equivalent.
    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension as _;
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
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

    /// Upsert a global setting into the `settings` key-value table.
    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
    /// Persist the list of open tabs so the next launch can restore it
    /// (doc 10 Phase 4: "Remember open tabs on exit, restore on
    /// launch").
    ///
    /// # Why a `settings` row and not a table
    ///
    /// The open-tab list is one value that is always read whole and
    /// written whole, and doc 06 already ships `settings(key TEXT
    /// PRIMARY KEY, value TEXT)` for exactly that shape. A dedicated
    /// `open_tabs` table would need its own CREATE in
    /// [`MIGRATION_001`], its own idempotency story for databases that
    /// are already on disk, and indexes — all to hold what a single
    /// key-value row holds natively. The row is therefore written with
    /// an upsert so repeated saves replace it instead of colliding on
    /// the primary key.
    ///
    /// An empty list is stored as `[]`, not deleted: "no tabs open" is
    /// a state worth remembering, and keeping the row means a fresh
    /// store and a deliberately-emptied one are distinguishable.
    ///
    /// Only tab identity is stored — no credentials (doc 06
    /// §"Credential Storage").
    pub fn save_open_tabs(&self, tabs: &[crate::ui::tab_state::TabState]) -> rusqlite::Result<()> {
        let json = crate::ui::tab_state::tabs_to_json(tabs)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![crate::ui::tab_state::OPEN_TABS_SETTING_KEY, json],
        )?;
        Ok(())
    }

    /// Read the persisted open-tab list.
    ///
    /// Returns an empty list when nothing was ever saved — a fresh
    /// database has no `open_tabs` row, which is the same thing as "no
    /// tabs to restore". Malformed JSON surfaces as a
    /// [`rusqlite::Error::FromSqlConversionFailure`] rather than an
    /// empty list: silently restoring nothing would look identical to a
    /// successful launch with no saved tabs, and the caller can log it
    /// and carry on.
    pub fn load_open_tabs(&self) -> rusqlite::Result<Vec<crate::ui::tab_state::TabState>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![crate::ui::tab_state::OPEN_TABS_SETTING_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(Vec::new());
        };
        crate::ui::tab_state::tabs_from_json(&json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
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

/// One `hotkeys` row with `is_default = 0`, as
/// `(action, key_sequence, enabled)`.
///
/// A plain tuple in the table's column order: the hotkey row carries no
/// JSON and no typed model (doc 06 §"Database Schema"), and
/// [`SessionStore::load_custom_hotkeys`] is its only reader.
pub type CustomHotkeyRow = (String, String, bool);

/// Shared setup for both constructors: pragmas, schema, seed.
fn initialize(conn: &mut Connection) -> rusqlite::Result<()> {
    // Doc 06 §"Rust Schema Definition": pragmas first, then the
    // migration. `foreign_keys` is per-connection and re-armed on
    // every open; `journal_mode=WAL` persists in the database file
    // and is idempotent.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(MIGRATION_001)?;
    seed_defaults(conn)
}

/// Insert doc 06's default hotkeys and settings exactly once.
///
/// The seed runs inside a transaction so a half-applied seed can
/// never commit; the `settings` probe below then reliably detects
/// "already seeded" on re-open. The probe — not `IF NOT EXISTS` —
/// is what keeps `open` idempotent for the seed: `hotkeys` has no
/// UNIQUE constraint on `action`, so re-running the seed verbatim
/// would insert duplicate rows.
fn seed_defaults(conn: &mut Connection) -> rusqlite::Result<()> {
    let seeded: i64 = conn.query_row("SELECT EXISTS(SELECT 1 FROM settings)", [], |row| {
        row.get(0)
    })?;
    if seeded == 0 {
        let tx = conn.transaction()?;
        tx.execute_batch(SEED_DEFAULTS)?;
        tx.commit()?;
    }
    Ok(())
}

/// Initial schema — doc 06 §"Database Schema", copied verbatim.
///
/// The only edit: `IF NOT EXISTS` on every CREATE (as doc 06's own
/// Rust sketch shows) so re-opening an existing database succeeds.
/// Includes the 2026-08 revision columns `host_key_policy`,
/// `jump_session_id`, and `forward_rules`.
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

/// Default rows — doc 06 §"Default Data", copied verbatim.
///
/// 28 hotkeys and 10 settings, inserted only into a fresh database by
/// [`seed_defaults`]. The hotkeys MUST stay in sync with doc 07's
/// default-actions table.
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
        session_type: serde_json::from_str(row.get::<_, String>("type")?.as_str()).map_err(
            |e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            },
        )?,
        folder_id: row.get("folder_id")?,
        host: row.get("host")?,
        port: row.get("port")?,
        username: row.get("username")?,
        auth_method: optional_json_column(row, "auth_method")?,
        key_path: row.get("key_path")?,
        host_key_policy: optional_json_column(row, "host_key_policy")?,
        jump_session_id: row.get("jump_session_id")?,
        forward_rules: optional_json_column(row, "forward_rules")?,
        tmux_enabled: row.get::<_, i32>("tmux_enabled")? != 0,
        tmux_session_name: row.get("tmux_session_name")?,
        ssh_options: optional_json_column(row, "ssh_options")?,
        rdp_domain: row.get("rdp_domain")?,
        rdp_width: row.get("rdp_width")?,
        rdp_height: row.get("rdp_height")?,
        rdp_color_depth: row.get("rdp_color_depth")?,
        rdp_compression: row.get::<_, i32>("rdp_compression")? != 0,
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
        connect_count: row.get::<_, i32>("connect_count")? as u32,
    })
}

fn optional_json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row,
    name: &str,
) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = row.get(name)?;
    match text {
        Some(s) => {
            let value = serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
