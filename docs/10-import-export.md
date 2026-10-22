# Import and Export

Sessions come in from other tools and go out as Wayterm's own JSON.
Secrets never travel in either direction.

Code: `src/transfer/` (one reader per source, `json.rs` for export, and
`apply_import`), `src/ui/settings/backup.rs` (the Settings page).

## Backup / Restore page

Import and export live on the Settings window's **Backup / Restore** page
(doc 13). It lists one row per source, each with a file picker, in this
order:

1. OpenSSH config (`~/.ssh/config`)
2. Remmina (`.remmina`)
3. MobaXterm (`.ini` / `.mxtsessions`)
4. Asbru-CM (YAML)
5. Wayterm JSON

An import asks for confirmation, then reports what it did in the page's
status line — imported, skipped, templates, and the first three warnings
(the rest become "…") — and refreshes the session tree. **Export** writes
the JSON document to a chosen file.

When the store is empty on first start, Wayterm opens this page once; the
`import_offered` settings row records that it was shown, whatever the user
did next, so it never nags.

## How an import works

- **Readers are pure.** Each source's parser takes text and returns
  folders, sessions and a report — no store access, no network, no exec,
  never a key file. Only `~/.ssh/config`'s `Include` chain is read from
  disk.
- **One writer.** `apply_import` writes a parse result to the store:
  folders parent-first, then sessions. Parsed sessions refer to their
  folder and jump host by position in the parse result; `apply_import`
  remaps both to the ids the store assigns, inserting jump targets first
  (one hop, doc 04 §Jump hosts).
- **Nothing is dropped silently.** Secret fields in a source are skipped
  with a warning; unmappable entries (unsupported protocols, missing
  hosts) are warnings and count as skipped.
- **No secrets.** `SessionConfig` holds none, so no import can write one
  and no export can contain one.
- **Files:** only regular files are read (a symlink to one is fine); a
  FIFO is opened non-blocking and refused, never waited on.

### ssh config

`~/.ssh/config` goes through Wayterm's own read-only block scanner, not
`russh-config`, which cannot enumerate `Host` blocks or follow `Include`:

- each exact alias on a `Host` line is one session; wildcard patterns
  become template sessions; negated patterns never do;
- `Match` blocks and options outside any `Host` are skipped and counted
  in the report;
- like OpenSSH, the first value obtained for an alias wins;
- `Include` is followed where it appears, relative to `~/.ssh`, `~/`
  expanded, at most 5 levels deep and cycle-safe; non-regular files are
  skipped with a warning;
- mapped: `HostName`, `Port`, `User`, `IdentityFile` (the key path;
  `none` ignored), `IdentitiesOnly yes` (the key method),
  `ServerAliveInterval` (the stored keepalive interval) and `ProxyJump`
  (the jump host, resolved to another imported alias). Key files are never
  read.

### Remmina

One `.remmina` file is one session. `group=` (`/`-nested) is the folder;
`server=` splits as `[user@]host[:port]`. Password and passphrase lines are
never copied: each is a "secret skipped" warning.

### MobaXterm

A `.ini` / `.mxtsessions` export. Every `[Bookmarks…]` section is one
folder, its `SubRep=` path (`\`-separated) the nesting; each other line is
a bookmark, `name=#type#field%field%…#…`. Fields 1–3 of an SSH, RDP or VNC
bookmark are host, port and user; nothing else is mapped. `[Passwords]` and
`[Credentials]` are never read.

### Asbru-CM

A YAML export. Entries live under `environments` (or at the top level),
keyed by uuid; `_is_group` marks a folder and `parent` names its parent's
uuid (`__PAC__ROOT__` is the root). Any field naming a password or
passphrase is never copied.

## JSON export

```json
{"version": 1,
 "folders": [{"name": "prod", "icon": "wayterm-server-rack-symbolic",
              "children": [ … ], "sessions": [ … ]}],
 "sessions": [ … ]}
```

- Folders nest, each holding its `children` and `sessions`; a top-level
  `sessions` array appears only when sessions live outside any folder.
- Sessions are full `SessionConfig` rows without ids; a jump host is named
  by `jump_session`, not numbered.
- Icons travel as names, folders and sessions alike; custom SVG files do
  not, so on a machine without the same SVG the item draws its default
  icon (doc 09 §Icons).
- Import accepts `version` ≤ 1 and refuses newer documents. An export →
  import roundtrip is tested.

The JSON import is also how fields the session dialog does not edit
(`ssh_options`, `host_key_policy`, `color_scheme`, …, doc 09 §Limitations)
get set.

## Limitations

- Importing twice creates duplicates: an import always adds, never
  merges.
- MobaXterm and Remmina field semantics are checked against hand-written
  fixtures only, not real exports.
- Macros (doc 12) and hotkey layouts (doc 11 §Persistence, exported
  separately) are not part of the session export.
