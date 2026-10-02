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

use std::path::Path;

use rusqlite::Connection;

/// A SQLite-backed store of session folders and saved sessions.
pub struct SessionStore {
    conn: Connection,
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
        Ok(Self { conn })
    }

    /// Open an in-memory store with the same schema and seed data.
    ///
    /// For tests and CI. `journal_mode=WAL` is a no-op on `:memory:`
    /// databases (SQLite reports `memory`); everything else is
    /// identical to a file-backed store.
    pub fn in_memory() -> rusqlite::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        initialize(&mut conn)?;
        Ok(Self { conn })
    }
}

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

#[cfg(test)]
mod tests;
