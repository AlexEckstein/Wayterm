# Hotkey Engine

Every app action has a rebindable shortcut, and shortcuts can differ by
context (terminal vs session tree vs SFTP sidebar). `src/hotkeys/` holds
the data model, persistence, and the GTK dispatcher (§HotkeyEngine); the
UI registers each action's handler (§Handlers).

## Dispatch

One `ShortcutController` on the main window holds one shortcut per bound
action. It runs in the capture phase, so it sees every key before the
focused widget does (VTE takes every key it is given). Each shortcut
checks its action's context when the key is pressed, and only then
activates the window action `win.<id>`:

| Context | Fires when |
|---|---|
| `global` | always, while the main window has focus |
| `terminal` | the current tab has a live terminal, and focus is not in a text field or popover |
| `session_tree` | focus is inside the session tree, but not in a text field or popover |
| `sftp` | focus is inside the side dock's SFTP panel, but not in a text field or popover |
| `rdp_tab`, `vnc_tab` | never: these contexts hold no actions |

A text field is anything editable (entries, the terminal search popover,
text views); a popover owns its keys (the session tree's context menu
answers `Return` itself). So terminal shortcuts keep working after a
click on a tab, the side dock or a header button, and stand aside only
while the user types somewhere else. RDP and VNC tabs have no terminal,
so only global chords are taken from them; every other key reaches the
remote session.

A key whose shortcut's context is inactive, or whose window action is
missing or disabled (`toggle_sftp` on a tab without a sidebar), passes
through untouched.

## Action Registry

```rust
pub struct Action {
    pub id: String,            // "new_tab"
    pub display_name: String,  // "New Tab"
    pub default_key: String,   // canonical UI format, "Ctrl+Shift+T"
    pub current_key: String,   // canonical; empty once an override unbinds it
    pub context: ActionContext,
    pub enabled: bool,
    pub is_default: bool,      // current_key == default_key
}

pub enum ActionContext { Global, Terminal, SessionTree, Sftp, RdpTab, VncTab }
```

The registry is data only: handlers are window actions (§Handlers).
Contexts are stored in `hotkeys.context` as `global`, `terminal`,
`session_tree`, `sftp`, `rdp_tab`, `vnc_tab`.

## Conflict Detection

Two bound actions conflict when they share a chord and can fire together:

1. two globals;
2. two actions in the same context;
3. a global and any context action;
4. a terminal action and a session-tree or SFTP action: terminal
   shortcuts stay live while those panels have focus;
5. but **not** a session-tree action and an SFTP action: focus is in one
   panel at most, which is why `Ctrl+D` can be both `duplicate_session`
   and `sftp_download`.

Disabled and unbound actions hold no chord. Chords compare by key, not
by spelling (§Trigger Parsing): a captured `Ctrl+Shift+t` collides with
`new_tab`'s `Ctrl+Shift+T`. A rebind onto a taken chord is refused (the
UI then asks "*Ctrl+Shift+T* is already bound to 'New Tab'. Override?");
overriding takes the chord and unbinds and disables the loser.

Should a co-firing pair share a chord anyway, the dispatcher tries the
narrower context first: session tree and SFTP, then terminal, then
global.

## Default Actions

| Action | Default | Context |
|---|---|---|
| `new_tab`, `close_tab` | Ctrl+Shift+T, Ctrl+Shift+W | global |
| `next_tab`, `prev_tab` | Ctrl+Tab, Ctrl+Shift+Tab | global |
| `goto_tab_1` … `goto_tab_9` | Alt+1 … Alt+9 | global |
| `toggle_sftp`, `toggle_session_tree` | Ctrl+Shift+B, Ctrl+Shift+S | global |
| `toggle_fullscreen` | F11 | global |
| `reconnect`, `disconnect` | Ctrl+Shift+R, Ctrl+Shift+X | global |
| `new_session` | Ctrl+Shift+N | global |
| `rename_tab` | Ctrl+Shift+Return | global |
| `search`, `search_scrollback` | Ctrl+Shift+F, Ctrl+Shift+K | terminal |
| `copy`, `paste`, `select_all`, `clear` | Ctrl+Shift+C, Ctrl+Shift+V, Ctrl+Shift+A, Ctrl+Shift+L | terminal |
| `edit_session`, `duplicate_session` | Ctrl+E, Ctrl+D | session_tree |
| `delete_session`, `connect_session` | Delete, Return | session_tree |
| `sftp_upload`, `sftp_download` | Ctrl+U, Ctrl+D | sftp |
| `sftp_rename`, `sftp_delete` | F2, Delete | sftp |

34 actions. The table is conflict-free under the rules above (tests
enforce it); the only repeated chords are `Ctrl+D` and `Delete`, each
shared by the session tree and SFTP. The Enter key is spelled `Return`:
GDK has no key named `Enter`, so such a binding could never fire.

Global and terminal chords are taken before the terminal sees them, so
none of their defaults is a bare `Ctrl+<letter>`: those are the
terminal's control characters (tests enforce it). `Alt+1` … `Alt+9` are
taken from the terminal too (readline's digit argument, irssi's window
keys), as in GNOME Terminal.

The 2026-08 revision fixed two collisions that must stay fixed:
`rename_tab` vs `reconnect`, and `toggle_sftp` vs terminal `search`. The
2026-10 revision moved `new_session` off `Ctrl+N` (the shell's
next-history key, vim's completion) and dropped `split_horizontal`,
`split_vertical` and `close_split`: there are no split panes, and the
actions return with them.

## Handlers

Every action is a window action, `win.<id>`, registered before the
engine is installed; `install` lists any action without one (logged at
startup), and a widget test builds the main window and checks that every
action in the table has one. What each does:

- `new_tab`: the host picker (§New Tab).
- `close_tab`: closes the current tab as its close button does, asking
  first per `confirm_close` (doc 04 §Lifecycle). Home is never closed.
- `next_tab`, `prev_tab`: the next or previous tab, wrapping round;
  `goto_tab_N`: the Nth tab, if there is one. Home is not counted.
- `rename_tab`: the current tab's rename prompt, as double-clicking its
  name opens.
- `toggle_sftp`: the side dock's SFTP panel, or back to the panel it came
  from (doc 09).
- `toggle_session_tree`: hides the dock's panel while it shows Sessions;
  otherwise shows the panel on Sessions and moves focus into the tree.
  Picking any dock tab brings a hidden panel back; while it is hidden,
  `sftp_on_connect` (doc 09) leaves it hidden.
- `toggle_fullscreen`: the window's fullscreen state.
- `reconnect`, `disconnect`: the current tab's connection (doc 04).
- `new_session`: the session dialog for a new session (doc 06).
- terminal actions: on the current tab's terminal; the two searches
  open its search popover.
- session-tree actions act on the selected row, like its context menu:
  `edit_session` edits a session or folder, `duplicate_session` is Clone
  Session, `delete_session` deletes a session or folder once confirmed,
  `connect_session` opens a session or opens and closes a folder. Each is
  enabled only while a row it applies to is selected, so with none their
  keys reach the list (`Return` activates its focused row).
- SFTP actions press the current sidebar's toolbar button, so they follow
  its enable rules (doc 09).

## New Tab

`new_tab` opens a modal host picker. "Local shell" comes first and is
selected; every saved session follows in the session tree's order, under
its folders, all expanded, with the tree's icons. Up and Down move
between hosts (folders are headings, never selected), Return or a
double-click opens the host in a new tab, Escape cancels. Folders with no
session below them are left out.

"Local shell" is an unsaved local session: the login shell (`$SHELL`) in
the home directory. Its tab is saved under session id 0, which SQLite
never assigns, and restores as a local shell (doc 10 Phase 4).

The tab bar's "+" (doc 01 §Tab bar) skips the picker: it opens a local
shell in a new tab straight away.

## Trigger Parsing

Bindings are stored in canonical UI format and converted for GTK:
`Ctrl+Shift+T` → `<Control><Shift>t`, `Ctrl+Alt+Delete` →
`<Control><Alt>Delete`, `F11` → `F11`.

- Modifiers: `Ctrl`, `Shift`, `Alt`, `Super`, `Meta`; anything else, or a
  trailing `+`, is invalid. A literal `+` key can only be captured.
- Canonical form: modifiers in that order, a letter upper-case, and
  `ISO_Left_Tab` (what Shift+Tab sends) as `Tab`. Every key is
  canonicalized as it is registered, rebound, loaded or imported, so two
  spellings of one chord are one chord, and capturing a default chord
  leaves its action at the default.
- Converting for GTK, only single characters are lowercased; key names
  (`Tab`, `F11`, `Delete`) are case-sensitive.
- A binding is accepted only if it parses **and** GDK knows the key name,
  so nothing can be stored that would silently never fire.
- Capture reads `key()` + `modifier_state()` from the key event and renders
  it canonically; lock and button masks are ignored. A shifted symbol is
  kept as GTK reports it (`Ctrl+Shift+exclam` on a US layout): that is the
  key GTK matches a Shift chord against.

## HotkeyEngine

`HotkeyEngine` wraps the registry with the checked rebind (`Reject` or
`Override`), reset, enable/disable, persistence, and JSON export.

`install(window, hosts)` adds the dispatcher (§Dispatch) to the main
window. Each `ContextHost` says when one context is active, checked at
each key press; the main window hosts `terminal` (a live terminal, focus
not in a text field), and `session_tree` and `sftp` on their panels'
widgets. Global needs no host; a context without one stays unbound.
Disabled and unbound actions hold a never-trigger. Installing again
replaces the controller.

Once installed, `rebind`/`rebind_with`, `reset_to_default`, and
`set_enabled` swap only the triggers they changed (an override also its
losers), in place. `load_from_db` rebuilds the rows it applied and the
actions they displaced, and `import_json` re-syncs every binding. The
settings UI goes through these methods, so a change takes effect live.

## Settings UI

The Keyboard Shortcuts page of the Settings window (beside General, doc 06
§Default Data) lists every action (name | key | context) with Capture and
Reset buttons. Capture opens a small modal that records the next chord
(Escape cancels; bare modifiers are ignored). Conflicts raise the
Override? prompt; every change is saved immediately. A `ListBox` stands in
for a `ColumnView`: the same columns, less machinery for 34 rows.

## Persistence

On startup the registry starts from the default table, then the
`is_default = 0` rows from doc 06's `hotkeys` table are applied to the
*current* binding, oldest first. Seeded `is_default = 1` rows are never
loaded. A bound row goes through the same check as an override, so a
stored chord that collides under today's rules takes the chord and
unbinds the holder (logged at startup; the next save records it). A
disabled row holds no chord and applies as stored. Rows for unknown
action ids (the dropped split actions among them), or with keys GTK
cannot fire, are skipped and reported, so a database from another build
still loads; an empty key is kept (it records an override's loser). Keys
load canonical, so a row that spells a default chord differently is that
default again.

Saving writes one `is_default = 0` row per rebound or disabled action
(replacing any earlier row inside a `BEGIN IMMEDIATE` transaction) and
deletes the row of every action back at its default.

Layouts export as JSON (`{"version": 1, "bindings": [...]}`), deviations
only, so an import onto a newer build keeps that build's defaults.
Imported chords override what holds them; unknown actions, changed
contexts, and keys that no longer bind are skipped and reported.
