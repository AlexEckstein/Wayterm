# Sessions

Saved sessions and their folders: the SQLite store they live in, the
session tree that shows them, the session dialog that edits them, and
their icons. Credentials never live here (doc 05); the settings rows in the
same database are doc 13 §Settings rows.

Code: `src/store/` (`db.rs` the store, `model.rs` the row types),
`src/ui/panels/session_tree.rs`, `src/ui/dialogs/session.rs` and
`session/model.rs`, `src/ui/dialogs/folder.rs`, `src/ui/icons.rs`.

## Database

One SQLite database, `$XDG_DATA_HOME/wayterm/wayterm.sqlite3` (default
`~/.local/share/wayterm/`), through `rusqlite`. It holds folders,
sessions, hotkey overrides (doc 11 §Persistence), settings (doc 13),
macros (doc 12), RDP certificate trust (doc 08 §Certificates) and the
open-tab list (doc 03 §Restoring tabs). The UI shares one `SessionStore`
on the main thread; the RDP certificate check opens a second handle on
the blocking pool, which WAL makes safe.

`SessionStore::open` turns on `journal_mode=WAL` and `foreign_keys`, then
runs every schema step and, on a fresh database, the seed (doc 13
§Settings rows). There is no migration framework: each step is idempotent
and runs on every open.

| Step | Change |
|---|---|
| 1 | the base schema below (`IF NOT EXISTS`) |
| 2 | `rdp_trust (host, port, fingerprint, first_seen)`, primary key (host, port) |
| 3 | `sessions.icon`, added where missing |
| 4 | drop the retired `sessions.notes` column where present |
| 5 | `macros (id, name, steps, created_at, updated_at)` (doc 12 §Storage) |

## Schema

```sql
CREATE TABLE folders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    parent_id   INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    sort_order  INTEGER DEFAULT 0,
    icon        TEXT,                     -- icon name; NULL = the folder icon
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE sessions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id       INTEGER REFERENCES folders(id) ON DELETE SET NULL,
    name            TEXT NOT NULL,
    type            TEXT NOT NULL,        -- ssh, rdp, vnc, serial, local
    sort_order      INTEGER DEFAULT 0,

    -- Common connection fields
    host            TEXT,
    port            INTEGER,              -- NULL = the type's default
    username        TEXT,
    auth_method     TEXT,                 -- NULL = password
    host_key_policy TEXT,                 -- NULL = the global setting

    -- SSH
    key_path        TEXT,
    tmux_enabled    INTEGER DEFAULT 0,
    tmux_session_name TEXT,               -- bare suffix; the transport prepends 'wayterm-'
    ssh_options     TEXT,                 -- JSON SshOptions
    jump_session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
    forward_rules   TEXT,                 -- JSON [ForwardRule]

    -- RDP
    rdp_domain      TEXT,
    rdp_width       INTEGER,              -- NULL = 1024
    rdp_height      INTEGER,              -- NULL = 768
    rdp_color_depth INTEGER,              -- NULL = 32
    rdp_compression INTEGER DEFAULT 1,
    rdp_options     TEXT,                 -- JSON RdpOptions

    -- VNC
    vnc_color_depth INTEGER,
    vnc_options     TEXT,                 -- JSON VncOptions

    -- Serial
    serial_device    TEXT,
    serial_baud_rate INTEGER,
    serial_data_bits INTEGER DEFAULT 8,
    serial_parity    TEXT DEFAULT 'none',
    serial_stop_bits INTEGER DEFAULT 1,
    serial_flow_control TEXT DEFAULT 'none',

    -- Local shell
    local_shell      TEXT,
    local_working_dir TEXT,

    -- Terminal appearance (per-session override; NULL = the global row)
    terminal_backend TEXT,
    color_scheme     TEXT,
    font_family      TEXT,
    font_size        REAL,
    opacity          REAL,
    icon             TEXT,                -- NULL = the type's default icon

    -- Metadata
    last_connected   TEXT,
    connect_count    INTEGER DEFAULT 0,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE hotkeys (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    action       TEXT NOT NULL,
    key_sequence TEXT NOT NULL,
    context      TEXT DEFAULT 'global',
    enabled      INTEGER DEFAULT 1,
    is_default   INTEGER DEFAULT 1
);

CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE connection_history (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id     INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    connected_at   TEXT NOT NULL DEFAULT (datetime('now')),
    duration       INTEGER,
    bytes_sent     INTEGER,
    bytes_received INTEGER
);

CREATE INDEX idx_sessions_folder ON sessions(folder_id);
CREATE INDEX idx_sessions_type   ON sessions(type);
CREATE INDEX idx_sessions_host   ON sessions(host);
CREATE INDEX idx_folders_parent  ON folders(parent_id);
CREATE INDEX idx_hotkeys_action  ON hotkeys(action);
CREATE INDEX idx_history_session ON connection_history(session_id);
CREATE INDEX idx_history_recent  ON connection_history(connected_at DESC);
```

Deleting a folder deletes its sub-folders (`ON DELETE CASCADE`) and moves
their sessions to the root (`ON DELETE SET NULL`). Deleting a jump host
leaves the sessions that used it without one.

## Wire formats

Enum columns hold serde spellings, locked by `src/store/model.rs` tests;
changing one is a schema break.

| Column | Values |
|---|---|
| `type` | `ssh`, `rdp`, `vnc`, `serial`, `local` (stored as a JSON string, quotes included) |
| `auth_method` | `password`, `key`, `agent`, `keyboard-interactive`, `none` |
| `host_key_policy` | `strict`, `accept-new`, `off`; NULL = the global setting |
| `serial_parity` | `none`, `odd`, `even`, `mark`, `space` (`mark`/`space` cannot connect, doc 06 §Limitations) |
| `serial_flow_control` | `none`, `hardware`, `software` |
| `serial_data_bits` / `serial_stop_bits` | 5–8 / 1–2 |
| `rdp_color_depth` | 16, 24, 32 |

JSON columns:

| Column | Shape |
|---|---|
| `ssh_options` | `SshOptions { compression, keepalive_interval, keepalive_count, ssh_w_fallback }` |
| `forward_rules` | `[{kind: "local"\|"remote"\|"dynamic", bind_addr, bind_port, remote_host, remote_port, enabled}]` |
| `rdp_options` | `RdpOptions { audio, clipboard, fonts, printer }` |
| `vnc_options` | `VncOptions { quality (0–9), scaling, view_only }` |

`SessionConfig` mirrors one row and never holds a secret. It deserializes
with defaults, so JSON written before a field existed still loads (an old
`ssh_options` loads `ssh_w_fallback` as `false`); its `icon` is serialized
only when set, so a session without one keeps the earlier wire format.

`SessionType` answers each per-protocol question in one place (default
port, SFTP support, default icon, …) with exhaustive matches, so a new
type must answer every one before it compiles. Default ports: SSH 22,
RDP 3389, VNC 5900; serial and local have none.

## Session tree

The side dock's Sessions panel (`src/ui/panels/session_tree.rs`): every folder
and saved session, depth first — at each level folders first, then
sessions — each with its icon.

- **Long names** end in "…" when the panel is too narrow, and show
  whole in their tooltip.
- **Context menu** (right-click): New Folder, Edit Folder, Delete Folder
  on folders; New Session, Edit Session, Clone Session, Delete Session on
  sessions. Deletes ask first.
- **Activate** (double-click, Return): a session opens in a new tab; a
  folder expands or collapses.
- **Drag and drop:** dragging a session or folder onto a folder moves it
  there (a folder brings its contents); dropping on a session joins that
  session's folder; dropping below the rows moves to the root. A folder
  never moves into itself or one of its sub-folders
  (`SessionStore::move_folder` refuses). Drags carry their own GType, so
  only drags from this tree are accepted.
- **Clone** inserts a copy named "`name` (copy)" in the same folder,
  without the id or connection history; keyring secrets stay with the
  original.
- **Delete** also removes the session's keyring entries (doc 05 §Saving
  and deleting).
- The session-tree shortcuts (`edit_session`, `duplicate_session`,
  `delete_session`, `connect_session`) act on the selected row (doc 11
  §Handlers).

## Session dialog

`src/ui/dialogs/session.rs` creates or edits one `SessionConfig`.
`config_from_fields` (`session/model.rs`) holds all of the mapping
and is display-free; the GTK part only gathers `DialogFields`.

- Common fields: name, type, folder, host, port (0 = the type's default),
  username, icon.
- Choosing a type reveals that type's group; only the selected group's
  values are stored:
  - **SSH:** auth method; password and **Save password to keyring** (for
    the password method, doc 05 §Saving and deleting); jump host (only
    SSH sessions that do not themselves jump; depth > 1 and self-jump are
    refused); forward rules (the Tunnels dialog's editor, doc 04
    §Tunnels dialog); tmux toggle and name (doc 04 §Tmux session
    persistence).
  - **Serial:** device, baud (starts at 115200), data bits, parity, stop
    bits, flow control.
  - **RDP:** domain, resolution (`WIDTHxHEIGHT`, empty = 1024×768), color
    depth. Compression has no control; new RDP rows default it on.
  - **VNC:** a note that its password lives in the keyring.
  - **Local:** shell and working directory.
- OK validates and saves; errors (an unparsable forward rule, a bad
  resolution, a jump chain) show inline and keep the dialog open.

## Folders

Folders nest without a depth limit. New Folder asks for a name; **Edit
Folder** (`src/ui/dialogs/folder.rs`) edits its name and icon.

## Icons

`folders.icon` and `sessions.icon` hold an icon name drawn through the GTK
icon theme (`src/ui/icons.rs`); NULL means the default. A name the theme
cannot find (a deleted custom icon, another machine) draws the default,
never a broken image.

**Built-in set.** Wayterm ships 38 symbolic icons (`assets/tree-icons/`,
compiled into the binary as a GResource by `build.rs`), so the defaults
and the picker look the same whatever icon theme is installed. They cover
infrastructure (server, server rack, database, NAS, disk, UPS, power, CPU,
single-board computer), network (switch, router, firewall, Wi-Fi, VPN,
web, cloud, backup), compute (VM, container, cluster, terminal, code,
gear, monitoring, key), devices (desktop, remote desktop, laptop, serial,
printer, camera, media, mail, home, light bulb, sensor), plus folder and
star. Names are `wayterm-<icon>-symbolic`. Defaults: `folder` for folders,
and per type `server` (SSH), `desktop` (RDP), `remote-desktop` (VNC),
`serial` (serial), `terminal` (local). Two more, `macro` and `sftp`, are
the side dock's tab icons: bundled the same way but not offered in the
picker. Built-in SVGs use only filled `rect`/`circle`/`path` shapes — the
only ones GTK recolors — which a test enforces.

**Custom icons** are SVG only. "Import SVG…" copies the file into
`$XDG_DATA_HOME/wayterm/icons/`, which is on the icon theme's search path,
and stores the name `wayterm-<file stem>` (lowercased, other characters
folded to `-`; no file paths in the database; a name a bundled icon has
gets a counter). A `-symbolic` SVG is recolored to the theme like the
built-in icons. Files over 1 MiB, without an `<svg` element, or that GTK
cannot load are refused. Re-importing the same file reuses its name, and a
different file with a taken name gets a counter, so an existing icon never
changes underneath its users.

**Picker.** The session dialog and Edit Folder both offer a grid of the
built-in icons and, under its own heading, every imported SVG; a free-text
icon name (any icon of the installed theme) with a preview; Import SVG…;
and a reset to the default.

## Limitations

- `last_connected`, `connect_count` and the `connection_history` table
  exist but nothing writes them (`SessionStore::touch_connected` is never
  called).
- `sort_order` is never edited: siblings sort by name.
- The appearance columns (`terminal_backend`, `font_family`, `font_size`,
  `opacity`) are stored but not applied (doc 02 §Limitations), and the
  dialog edits none of them, nor `color_scheme`, `ssh_options`,
  `host_key_policy`, `vnc_options` or `vnc_color_depth`; imports and JSON
  restores can set them.
- `hotkeys` has no `UNIQUE (action, context)`, which is why the seed runs
  only once.
