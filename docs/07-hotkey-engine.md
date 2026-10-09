# Hotkey Engine

Every app action has a rebindable shortcut, and shortcuts can differ by
context (terminal vs session tree vs SFTP sidebar). `src/hotkeys/` holds
the data model and persistence; the GTK binding layer is still to be
built (§HotkeyEngine).

## The Two-Tier Mechanism

GTK 4's own scoping does the work; nothing toggles controllers on focus.

- **Tier 1 — global:** application actions with accelerators
  (`set_accels_for_action`). They fire whatever has focus.
- **Tier 2 — widget contexts:** one `ShortcutController` per region
  (terminal, session tree, SFTP sidebar) with `ShortcutScope::Managed`, so
  its shortcuts fire only while that widget or a descendant has focus.
- **Passthrough:** RDP and VNC tabs get no controller, so only Tier-1
  accels can intercept keys there; everything else reaches the remote
  session.

Because an application accel fires even when a Tier-2 widget has focus, a
chord may live in only one tier at a time.

## Action Registry

```rust
pub struct Action {
    pub id: String,            // "new_tab"
    pub display_name: String,  // "New Tab"
    pub default_key: String,   // UI format, "Ctrl+Shift+T"
    pub current_key: String,   // effective; empty once an override unbinds it
    pub context: ActionContext,
    pub enabled: bool,
    pub is_default: bool,      // current_key == default_key
}

pub enum ActionContext { Global, Terminal, SessionTree, Sftp, RdpTab, VncTab }
```

The registry is data only: handlers are gio actions (Tier 1) or
`CallbackAction`s (Tier 2). Contexts are stored in `hotkeys.context` as
`global`, `terminal`, `session_tree`, `sftp`, `rdp_tab`, `vnc_tab`.

## Conflict Detection

Two bound actions conflict when they share a chord and can fire together:

1. two globals;
2. two actions in the same Tier-2 context;
3. a global and any Tier-2 action;
4. but **not** two Tier-2 actions in different contexts — they are never
   active at once, which is why `Ctrl+D` can be both `duplicate_session`
   and `sftp_download`.

Disabled and unbound actions hold no chord. A rebind onto a taken chord is
refused (the UI then asks "*Ctrl+Shift+T* is already bound to 'New Tab'.
Override?"); overriding takes the chord and unbinds and disables the loser.

## Default Actions

| Action | Default | Context |
|---|---|---|
| `new_tab`, `close_tab` | Ctrl+Shift+T, Ctrl+Shift+W | global |
| `next_tab`, `prev_tab` | Ctrl+Tab, Ctrl+Shift+Tab | global |
| `goto_tab_1` … `goto_tab_9` | Alt+1 … Alt+9 | global |
| `toggle_sftp`, `toggle_session_tree` | Ctrl+Shift+B, Ctrl+Shift+S | global |
| `toggle_fullscreen` | F11 | global |
| `reconnect`, `disconnect` | Ctrl+Shift+R, Ctrl+Shift+X | global |
| `new_session` | Ctrl+N | global |
| `rename_tab` | Ctrl+Shift+Return | global |
| `split_horizontal`, `split_vertical`, `close_split` | Ctrl+Shift+D, Ctrl+Shift+E, Ctrl+Shift+Q | terminal |
| `search`, `search_scrollback` | Ctrl+Shift+F, Ctrl+Shift+K | terminal |
| `copy`, `paste`, `select_all`, `clear` | Ctrl+Shift+C, Ctrl+Shift+V, Ctrl+Shift+A, Ctrl+Shift+L | terminal |
| `edit_session`, `duplicate_session` | Ctrl+E, Ctrl+D | session_tree |
| `delete_session`, `connect_session` | Delete, Return | session_tree |
| `sftp_upload`, `sftp_download` | Ctrl+U, Ctrl+D | sftp |
| `sftp_rename`, `sftp_delete` | F2, Delete | sftp |

37 actions. The table is conflict-free under the rules above (tests
enforce it); the only repeated chords are `Ctrl+D` and `Delete`, each
across two Tier-2 contexts. The 2026-08 revision fixed three collisions
that must stay fixed: `split_vertical` vs `paste`, `rename_tab` vs
`reconnect`, and `toggle_sftp` vs terminal `search`. The Enter key is
spelled `Return`: GDK has no key named `Enter`, so such a binding could
never fire.

## Trigger Parsing

Bindings are stored in UI format and converted for GTK:
`Ctrl+Shift+T` → `<Control><Shift>t`, `Ctrl+Alt+Delete` →
`<Control><Alt>Delete`, `F11` → `F11`.

- Modifiers: `Ctrl`, `Shift`, `Alt`, `Super`, `Meta`; anything else, or a
  trailing `+`, is invalid. A literal `+` key can only be captured.
- Only single characters are lowercased; key names (`Tab`, `F11`,
  `Delete`) are case-sensitive.
- A binding is accepted only if it parses **and** GDK knows the key name,
  so nothing can be stored that would silently never fire.
- Capture reads `key()` + `modifier_state()` from the key event and renders
  it in UI format; lock and button masks are ignored.

## HotkeyEngine

`HotkeyEngine` wraps the registry with the checked rebind (`Reject` or
`Override`), reset, enable/disable, persistence, and JSON export.

**Not built yet:** `install()` — binding Tier-1 accels and the Tier-2
controllers from the registry, and rebuilding one binding after a rebind.
Until then the only accel installed is `toggle_sftp`'s default, and the
window actions (`copy`, `paste`, `search`, `reconnect`, …) exist but have
no keys.

## Settings UI

A window listing every action (name | key | context) with Capture and
Reset buttons. Capture opens a small modal that records the next chord
(Escape cancels; bare modifiers are ignored). Conflicts raise the
Override? prompt; every change is saved immediately. A `ListBox` stands in
for a `ColumnView`: the same columns, less machinery for 37 rows.

## Persistence

On startup the registry starts from the default table, then the
`is_default = 0` rows from doc 06's `hotkeys` table are applied to the
*current* binding. Seeded `is_default = 1` rows are never loaded. Rows
for unknown action ids, or with keys GTK cannot fire, are skipped and
reported, so a database from another build still loads; an empty key is
kept (it records an override's loser).

Saving writes one `is_default = 0` row per rebound or disabled action
(replacing any earlier row inside a `BEGIN IMMEDIATE` transaction) and
deletes the row of every action back at its default.

Layouts export as JSON (`{"version": 1, "bindings": [...]}`), deviations
only, so an import onto a newer build keeps that build's defaults.
Imported chords override what holds them; unknown actions, changed
contexts, and keys that no longer bind are skipped and reported.
