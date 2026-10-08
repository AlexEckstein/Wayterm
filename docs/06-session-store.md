# Session Store Schema

## Overview

Sessions are persisted in a SQLite database via `rusqlite`. Credentials
are stored separately in the OS keyring (libsecret), never in the
database.

## Database Schema

```sql
-- Session folders for tree organization
CREATE TABLE folders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    parent_id   INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    sort_order  INTEGER DEFAULT 0,
    icon        TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Saved sessions (SSH, RDP, VNC, serial, local shell)
CREATE TABLE sessions (
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
    tmux_session_name TEXT,                   -- bare suffix; SSHTransport prepends 'wayterm-' (doc 04)
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
CREATE TABLE hotkeys (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    action      TEXT NOT NULL,
    key_sequence TEXT NOT NULL,
    context     TEXT DEFAULT 'global',
    enabled     INTEGER DEFAULT 1,
    is_default  INTEGER DEFAULT 1
);

-- Global settings (key-value)
CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT
);

-- Connection history / recent sessions
CREATE TABLE connection_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    connected_at TEXT NOT NULL DEFAULT (datetime('now')),
    duration    INTEGER,
    bytes_sent  INTEGER,
    bytes_received INTEGER
);

-- Indexes
CREATE INDEX idx_sessions_folder ON sessions(folder_id);
CREATE INDEX idx_sessions_type ON sessions(type);
CREATE INDEX idx_sessions_host ON sessions(host);
CREATE INDEX idx_folders_parent ON folders(parent_id);
CREATE INDEX idx_hotkeys_action ON hotkeys(action);
CREATE INDEX idx_history_session ON connection_history(session_id);
CREATE INDEX idx_history_recent ON connection_history(connected_at DESC);
```

## Rust Schema Definition

Using `rusqlite` with prepared statements. The schema is created on
first run via migrations:

```rust
use rusqlite::{Connection, params};
use std::path::Path;

pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(MIGRATION_001)?;
        Ok(SessionStore { conn })
    }
}

const MIGRATION_001: &str = r#"
CREATE TABLE IF NOT EXISTS folders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    parent_id   INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    sort_order  INTEGER DEFAULT 0,
    icon        TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
-- ... (full schema from above)
"#;
```

## Session Config Struct

The database maps to a Rust struct via serde for JSON fields:

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SessionType {
    SSH,
    RDP,
    VNC,
    Serial,
    LocalShell,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuthMethod {
    Password,
    Key,
    Agent,
    KeyboardInteractive,   // see doc 11 — not optional, 2FA/OTP needs it
    None,
}

/// Host-key verification policy (doc 11). None = use global setting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostKeyPolicy {
    Strict,      // unknown → prompt; changed → hard fail (default)
    AcceptNew,   // unknown → auto-accept + record; changed → hard fail
    Off,         // no verification — lab/testing only, warning badge
}

/// One port-forward rule (doc 11 §Port forwarding).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForwardRule {
    pub kind: ForwardKind,     // Local (v1); Remote/Dynamic in v2
    pub bind_addr: String,    // "127.0.0.1"
    pub bind_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionConfig {
    pub id: i64,
    pub name: String,
    pub session_type: SessionType,
    pub folder_id: Option<i64>,

    // Common
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth_method: Option<AuthMethod>,

    // SSH
    pub key_path: Option<String>,
    pub host_key_policy: Option<HostKeyPolicy>,   // None = global default
    pub jump_session_id: Option<i64>,            // ProxyJump hop (doc 11), depth ≤ 1 in v1
    pub forward_rules: Option<Vec<ForwardRule>>, // local forwards (doc 11)
    pub tmux_enabled: bool,
    pub tmux_session_name: Option<String>,
    pub ssh_options: Option<SshOptions>,

    // RDP
    pub rdp_domain: Option<String>,
    pub rdp_width: Option<u32>,
    pub rdp_height: Option<u32>,
    pub rdp_color_depth: Option<u32>,
    pub rdp_compression: bool,
    pub rdp_options: Option<RdpOptions>,

    // VNC
    pub vnc_color_depth: Option<u32>,
    pub vnc_options: Option<VncOptions>,

    // Serial
    pub serial_device: Option<String>,
    pub serial_baud_rate: Option<u32>,
    pub serial_data_bits: Option<u8>,
    pub serial_parity: Option<String>,
    pub serial_stop_bits: Option<u8>,
    pub serial_flow_control: Option<String>,

    // Local
    pub local_shell: Option<String>,
    pub local_working_dir: Option<String>,

    // Terminal
    pub preferred_backend: Option<String>,
    pub color_scheme: Option<String>,
    pub font_family: Option<String>,
    pub font_size: Option<f64>,
    pub opacity: Option<f64>,

    // Metadata
    pub notes: Option<String>,
    pub last_connected: Option<String>,
    pub connect_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshOptions {
    pub compression: bool,
    pub keepalive_interval: Option<u32>,
    pub keepalive_count: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RdpOptions {
    pub audio: bool,
    pub clipboard: bool,
    pub fonts: bool,
    pub printer: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VncOptions {
    pub quality: u8,       // 0-9
    pub scaling: bool,
    pub view_only: bool,
}
```

## Database Operations

```rust
impl SessionStore {
    pub fn insert_session(&self, config: &SessionConfig) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO sessions (name, type, folder_id, host, port, username, auth_method,
                key_path, tmux_enabled, tmux_session_name, ssh_options,
                rdp_domain, rdp_width, rdp_height, rdp_color_depth, rdp_compression, rdp_options,
                vnc_color_depth, vnc_options,
                serial_device, serial_baud_rate, serial_data_bits, serial_parity,
                serial_stop_bits, serial_flow_control,
                local_shell, local_working_dir,
                terminal_backend, color_scheme, font_family, font_size, opacity, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27,
                ?28, ?29, ?30, ?31, ?32, ?33)",
            params![
                config.name,
                serde_json::to_string(&config.session_type)?,
                config.folder_id,
                config.host,
                config.port,
                config.username,
                config.auth_method.as_ref().map(|a| serde_json::to_string(a).unwrap()),
                config.key_path,
                config.tmux_enabled as i32,
                config.tmux_session_name,
                config.ssh_options.as_ref().map(|o| serde_json::to_string(o).unwrap()),
                config.rdp_domain,
                config.rdp_width,
                config.rdp_height,
                config.rdp_color_depth,
                config.rdp_compression as i32,
                config.rdp_options.as_ref().map(|o| serde_json::to_string(o).unwrap()),
                config.vnc_color_depth,
                config.vnc_options.as_ref().map(|o| serde_json::to_string(o).unwrap()),
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

    pub fn get_session(&self, id: i64) -> Result<SessionConfig, rusqlite::Error> {
        // SELECT * FROM sessions WHERE id = ?1
        // Map row to SessionConfig
        todo!()
    }

    pub fn list_sessions(&self, folder_id: Option<i64>) -> Result<Vec<SessionConfig>, rusqlite::Error> {
        // SELECT * FROM sessions WHERE folder_id = ?1 ORDER BY sort_order
        todo!()
    }

    pub fn update_session(&self, config: &SessionConfig) -> Result<(), rusqlite::Error> {
        // UPDATE sessions SET ... WHERE id = ?1
        todo!()
    }

    pub fn delete_session(&self, id: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    // Folders
    pub fn insert_folder(&self, name: &str, parent_id: Option<i64>) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO folders (name, parent_id) VALUES (?1, ?2)",
            params![name, parent_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_folders(&self, parent_id: Option<i64>) -> Result<Vec<Folder>, rusqlite::Error> {
        // ...
        todo!()
    }

    pub fn delete_folder(&self, id: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
        Ok(())
    }
}
```

## Credential Storage

Credentials are NEVER stored in the SQLite database. They go in the OS
keyring via libsecret, using libsecret's attribute-based lookup (not a
single concatenated key string — attributes are indexed and matchable):

```
schema: com.alex-eckstein.wayterm.Session
attributes per secret:
    session_id    → sessions.id
    session_name  → sessions.name (human-readable in the keyring UI)
    secret_type   → 'password' | 'key_passphrase' | 'vnc_password' | 'rdp_password'
```

The `sessions.auth_method` field records *which* method to use, but the
secret material is in the keyring. If a user exports/imports sessions,
credentials are not included — they must re-authenticate on first connect.
Full flow (keyring → prompt → store-if-checked, challenge-response
dialog, zeroization) is specified in
[11-ssh-auth-security.md](11-ssh-auth-security.md).

> **App id note:** the application id is `com.alex-eckstein.wayterm` —
> decided at design phase, before any code, exactly as required: it's baked
> into keyring schemas, XDG directories, and gsettings schemas, and
> changing it later orphans stored credentials. Binaries/packages use the
> plain name `wayterm`.

## Default Data

```sql
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
```

> **Known gap (verified Phase 1, Task 3):** `hotkeys` has **no UNIQUE
> constraint** — not on `action`, not on `(action, context)`. Re-running
> the seed verbatim would therefore insert duplicate rows. The code
> compensates rather than changing the schema: the seed runs inside a
> transaction (so a half-applied seed never commits) guarded by a
> `SELECT EXISTS(SELECT 1 FROM settings)` probe, so it applies exactly
> once per database. A `UNIQUE (action, context)` index is the obvious
> **schema-v2 candidate** — it is what makes the seed idempotent at the
> database level instead of by convention.

## Import/Export

Sessions can be exported to JSON for sharing or backup. Credentials are
not included — only connection parameters.

```json
{
  "version": 1,
  "exported_at": "2026-08-27T12:00:00Z",
  "folders": [
    { "name": "Production", "sessions": [
      {
        "name": "prod-db",
        "type": "ssh",
        "host": "db.prod.lan",
        "port": 22,
        "username": "admin",
        "auth_method": "key",
        "key_path": "~/.ssh/id_ed25519"
      }
    ]}
  ]
}
```

Rust implementation via serde:

```rust
pub fn export_sessions(store: &SessionStore, path: &Path) -> Result<(), Box<dyn Error>> {
    let folders = store.list_folders(None)?;
    let export = ExportFile {
        version: 1,
        exported_at: chrono::Utc::now().to_rfc3339(),
        folders: folders,
    };
    let json = serde_json::to_string_pretty(&export)?;
    fs::write(path, json)?;
    Ok(())
}
```

## Migration from Other Tools

Priority order — `~/.ssh/config` first, because it's the format every
Linux user already has (see doc 10 Phase 9):

1. **`~/.ssh/config`** — `russh-config` crate parses it (Host blocks,
   HostName, User, Port, IdentityFile, ProxyJump, dynamic entries).
   Import creates one saved session per Host block (skipping wildcard
   `Host *` blocks, or offering them as templates). This is the single
   cheapest adoption driver — a user's first run should offer it.
2. Remmina (`.remmina` files, GKeyFile format — one file per session)
3. MobaXterm (`.mxtpro`/registry export — format is XML/INI-ish, undocumented; reverse-engineer)
4. Asbru (PAC YAML)
5. Termius (if export available)

Each importer reads the foreign format, maps to `SessionConfig`, and
inserts into the DB. All importers are read-only against their source —
importing never writes back to `~/.ssh/config`.