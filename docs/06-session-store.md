# Session Store

Sessions, folders, hotkey overrides, and settings live in one SQLite
database (`$XDG_DATA_HOME/wayterm/wayterm.sqlite3`) through `rusqlite`
(`src/store/`). Credentials never do: they live in the OS keyring
(§Credential Storage).

## Database Schema

`SessionStore::open` turns on `journal_mode=WAL` and `foreign_keys`, then
applies this schema (with `IF NOT EXISTS`) and, on a fresh database, the
seed below. There is no migration framework yet: later additions are
idempotent steps run on every open — the `rdp_trust` table (doc 08) and
`sessions.icon`, which `ALTER TABLE` adds to databases created before
it existed.

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
    icon             TEXT,                    -- icon name; NULL = the type's default (§Icons)

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


Deleting a folder deletes its sub-folders (`ON DELETE CASCADE`) and moves
its sessions to the root (`ON DELETE SET NULL`).
Dragging a session onto a folder in the session tree moves it there
(sets `folder_id`); dropping it on a session joins that session's
folder, and dropping it below the rows moves it to the root.

### Wire formats

Enum columns hold serde spellings, locked by `src/store/model.rs` tests:

| Column | Values |
|---|---|
| `type` | `ssh`, `rdp`, `vnc`, `serial`, `local` (stored as a JSON string, quotes included) |
| `auth_method` | `password`, `key`, `agent`, `keyboard-interactive`, `none` |
| `host_key_policy` | `strict`, `accept-new`, `off`; NULL = the global setting |
| `serial_parity` | `none`, `odd`, `even`, `mark`, `space` — `mark`/`space` are stored but not representable by the `serialport` crate; connecting with either fails instead of silently opening with other framing (doc 04) |
| `serial_flow_control` | `none`, `hardware`, `software` |
| `serial_data_bits` / `serial_stop_bits` | 5–8 / 1–2 |
| `rdp_color_depth` | 16, 24, 32 (NULL → 32) |

`ssh_options`, `rdp_options`, `vnc_options`, and `forward_rules` are JSON
objects (`SshOptions`, `RdpOptions`, `VncOptions`, `[ForwardRule]`).
`SessionConfig` mirrors one row and never holds a secret; it deserializes
with defaults, so JSON written before a field existed still loads. Its
`icon` is serialized only when set, so a session without one keeps the
exact earlier wire format.

### Icons

`folders.icon` and `sessions.icon` hold an icon name drawn through the
GTK icon theme (`src/ui/icons.rs`); NULL means the default. A name the
theme cannot find (a deleted custom icon, another machine) draws the
default, never a broken image.

Wayterm ships its own set of 38 symbolic icons (`assets/tree-icons/`,
compiled into the binary as a GResource by `build.rs`, which needs
`glib-compile-resources` from the GLib dev tools), so the defaults and
the picker look the same whatever icon theme is installed. They cover
infrastructure (server, server rack, database, NAS, disk, UPS, power,
CPU, single-board computer), network (switch, router, firewall, Wi-Fi,
VPN, web, cloud, backup), compute (VM, container, cluster, terminal,
code, gear, monitoring, key), devices (desktop, remote desktop, laptop,
serial, printer, camera, media, mail, home, light bulb, sensor), plus
folder and star. Names are `wayterm-<icon>-symbolic`. The defaults:
`folder` for folders, and per session type `server` (SSH), `desktop`
(RDP), `remote-desktop` (VNC), `serial` (serial), `terminal` (local).
Built-in SVGs use only filled `rect`/`circle`/`path` shapes — the only
ones GTK recolors — which a test enforces.

Custom icons are SVG only. "Import SVG…" copies the file into
`$XDG_DATA_HOME/wayterm/icons/`, which is on the icon theme's search
path, and stores the name `wayterm-<file stem>` (lowercased, other
characters folded to `-`; no file paths in the database; a name a
built-in icon has gets a counter). A
`-symbolic` SVG is recolored to the theme like the built-in icons.
Files over 1 MiB, without an `<svg` element, or that GTK cannot load
are refused; re-importing the same file reuses its name, and a
different file with a taken name gets a counter, so an existing icon
never changes underneath its users.

The icon is edited in the session dialog and, for folders, in the
session tree's **Edit Folder** dialog (name + icon). Both offer a grid
of the built-in icons and, under its own heading, every imported SVG; a
free-text icon name (any icon of the installed theme); Import SVG…; and
a reset to the default.

## Default Data

Seeded once, inside a transaction, when the `settings` table is empty
(`hotkeys` has no `UNIQUE (action, context)`, so re-running the seed would
duplicate rows; that constraint is the first schema-v2 candidate).

| Setting | Default | Notes |
|---|---|---|
| `default_backend` | `vte` | |
| `default_color_scheme` | `Dark` | |
| `default_font_family` / `default_font_size` | `Monospace` / `12.0` | |
| `default_opacity` | `1.0` | |
| `scrollback_limit` | `10000` | |
| `confirm_close` | `1` | doc 04 §Lifecycle & Cancellation |
| `reconnect_on_drop` | `1` | auto-reconnect vs prompt |
| `tmux_default` | `0` | |
| `host_key_policy` | `strict` | doc 11 |
| `minimize_to_tray` | (not seeded) | missing = on |
| `sftp_on_connect` | (not seeded) | missing = on; off defers the SFTP sidebar (doc 09) |
| `open_tabs` | (written at runtime) | JSON tab list (doc 10 Phase 4) |

Boolean settings are on unless explicitly `0`/`false`/`no`/`off`; a
missing or unreadable row is on, so a typo never disables a safety prompt.

The Settings window's General page switches `confirm_close`,
`reconnect_on_drop`, `sftp_on_connect`, and `minimize_to_tray`. Each flip
writes `1`/`0` at once and applies live: the first two count from the
next tab close or connection drop, open tabs included; `sftp_on_connect`
governs tabs opened or connected afterwards; the tray setting is read at
each window close. Its other page, Keyboard Shortcuts, is doc 07's.

The seed also inserts 28 `hotkeys` rows with `is_default = 1`. They are
informational: only `is_default = 0` rows (user overrides) are ever loaded,
and doc 07's table is the source of truth. The seed predates two fixes
there, so it spells the Enter key `Enter` (GDK's name is `Return`) and
lacks `goto_tab_1` … `goto_tab_9`; a test pins exactly those differences.

## Credential Storage

Secrets go in the OS keyring under libsecret schema
`com.alex-eckstein.wayterm.Session`, with attributes `session_id`,
`session_name`, and `secret_type` (`password`, `key_passphrase`,
`vnc_password`, `rdp_password`). `auth_method` only records which method to
use. Export/import never includes secrets. The application id
`com.alex-eckstein.wayterm` is baked into keyring schemas and XDG paths —
changing it orphans stored credentials. (The keyring source is not wired
yet; lookups fall through to the prompt — doc 11 §CredentialManager.)

## Import and export (Phase 9)

Code: `src/transfer/` (engines), `src/ui/import_dialog.rs` (windows).

- **Import**, in priority order: `~/.ssh/config`, then Remmina `.remmina`
  files, MobaXterm `.ini`/`.mxtsessions` exports, Asbru-CM YAML — plus
  Wayterm's own JSON export. `~/.ssh/config` goes through Wayterm's own
  read-only block scanner, not `russh-config` (it cannot enumerate `Host`
  blocks or follow `Include`): one session per exact `Host` alias,
  wildcard patterns become template sessions, negated patterns and
  `Match` blocks are skipped, `Include` is followed where it appears
  (depth-capped, cycle-safe), key files are never read.
- **Export** is JSON: `{"version": 1, "folders": [...]}` with folders
  nested (each holding its `children` and `sessions`); a top-level
  `sessions` array appears only when sessions live outside any folder.
  A jump host is named, not numbered. Import accepts `version` ≤ 1 and
  refuses newer documents. Secrets are never exported (`SessionConfig`
  holds none); an export → import roundtrip is tested.
- **Importers are pure readers**: source text in, folders + sessions +
  report out — no store access. `apply_import` is the single store
  writer. Parsed folder and jump-host references are positions in the
  parse result, remapped by `apply_import` to the ids the store assigns
  (one jump hop only, doc 11).
- **Nothing is dropped silently.** Remmina/MobaXterm/Asbru secret fields
  are skipped with a warning; unmappable entries (unsupported protocols,
  missing hosts) are warnings and count as skipped; the import report
  lists them all.
- **Icons** (§Icons) travel as names in Wayterm's JSON export, folders
  and sessions alike; the SVG files do not. On a machine without the
  same custom SVG, the item draws its default icon.
- **UI:** when the store is empty on first start, Wayterm offers the
  import once (recorded in the `import_offered` setting). Import and
  Export are on the session-tree menu and are window actions
  (`win.import-sessions` / `win.export-sessions`).
