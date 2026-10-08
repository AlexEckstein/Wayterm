# Hotkey Engine

## Overview

Configurable keyboard shortcuts for all app actions. Users can rebind
any action, add new shortcuts, and set context-specific bindings (e.g.
different keys for terminal vs session tree).

GTK4 provides `GtkShortcutController` and `GtkShortcut` natively — the
modern replacement for GTK3's `GtkAccelGroup`.

> **Revision note (2026-08):** the original design attached four
> controllers (one per context) to the main window, all with
> `ShortcutScope::Global`, and hand-toggled them on focus change. That
> has two real bugs: (1) the "Global" controllers are active
> *simultaneously* — e.g. terminal-focus enabled both the Terminal and
> Global controllers, so the claimed "search vs toggle_sftp both
> Ctrl+Shift+F — no conflict" was wrong: both fired at once; (2) it fights
> the framework instead of using it. This revision uses GTK4's actual
> scoping mechanism. The default table is also de-conflicted (the old
> one had paste/split_vertical and reconnect/rename_tab collisions).

## The Two-Tier Mechanism (how GTK4 actually wants this done)

**Tier 1 — Global actions: `GtkApplication` actions + accelerators.**
Tab management and app-level actions are registered as `gio::SimpleAction`
on the `GtkApplication`/`GtkWindow`, with accels set via
`set_accels_for_action`. These fire regardless of which widget has
focus — exactly what "global" means — and GTK shows them in the menus
automatically.

```rust
// Setup (once, at startup):
let action = gio::SimpleAction::new("new-tab", None);
action.connect_activate(|_, _| { /* app-level handler */ });
app.add_action(&action);
app.set_accels_for_action("app.new-tab", &["<Control><Shift>T"]);
```

**Tier 2 — Context actions: widget-attached `ShortcutController`s.**
Each focusable region (terminal area, SFTP sidebar, session tree) gets
its own controller attached to that widget, with the default scope
`ShortcutScope::Managed`. Managed scope = the controller's shortcuts
fire only when the widget (or a descendant) has focus. No manual
enable/disable, no focus tracking to get right — the widget hierarchy
does the scoping.

```rust
let controller = gtk4::ShortcutController::new();
controller.set_scope(gtk4::ShortcutScope::Managed);  // this is the default
terminal_widget.add_controller(controller);
// every shortcut added here is active ONLY while terminal has focus
```

**RDP/VNC pass-through falls out for free:** remote-desktop tabs aren't
attached to any terminal/sftp/tree controller, so with focus in an RDP
tab only Tier-1 accels can intercept keys — which is precisely the
intended small set (new tab, close tab, next/prev tab, fullscreen).
Everything else reaches the widget's key-press handler, which forwards
to the remote session.

**Conflict rule this implies:** a Tier-1 accel and a Tier-2 shortcut with
the same key chord WILL both fire when the Tier-2 widget has focus
(application accels are handled by a window-level controller that runs
regardless). So a chord may exist in at most ONE tier at a time — the
conflict detector checks accordingly (below).

## Action Registry

```rust
use std::collections::HashMap;

pub struct ActionRegistry {
    actions: HashMap<String, Action>,
}

pub struct Action {
    pub id: String,            // "new_tab"
    pub display_name: String,  // "New Tab"
    pub default_key: String,  // "Ctrl+Shift+T" (UI format, parsed at bind time)
    pub current_key: String,  // effective binding (default until rebound)
    pub context: ActionContext,
    pub enabled: bool,
    pub is_default: bool,      // true if current binding == default
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ActionContext {
    Global,        // Tier 1: application accelerators
    Terminal,      // Tier 2: attached to terminal area
    SessionTree,   // Tier 2: attached to session tree
    Sftp,          // Tier 2: attached to SFTP sidebar
    RdpTab,        // passthrough context — no controller of its own
    VncTab,        // passthrough context — no controller of its own
}

impl ActionRegistry {
    pub fn register(
        &mut self,
        id: &str, display_name: &str, default_key: &str,
        context: ActionContext,
    ) { /* insert with current_key = default_key */ }

    /// Rebind: update current_key, flip is_default, then HotkeyEngine
    /// rebuilds the affected controller / accel.
    pub fn rebind(&mut self, id: &str, new_key: &str) -> Result<(), RebindError> {
        if let Some(action) = self.actions.get_mut(id) {
            action.current_key = new_key.to_string();
            action.is_default = action.current_key == action.default_key;
            Ok(())
        } else { Err(RebindError::UnknownAction) }
    }
}
```

Handlers do not live in the registry (the original sketch stored
`Box<dyn Fn()>` there — that doesn't survive Tier 1, where activation
comes through `gio::Action::activate`). Tier-1 actions are gio actions;
Tier-2 actions are `CallbackAction` shortcuts. The registry is the
*data model*; the engine binds it to both mechanisms.

## HotkeyEngine

```rust
pub struct HotkeyEngine {
    registry: ActionRegistry,
    app: gtk4::Application,                       // Tier 1 target
    context_controllers: HashMap<ActionContext, gtk4::ShortcutController>,
    widget_for_context: HashMap<ActionContext, gtk4::Widget>, // attach targets
}

impl HotkeyEngine {
    pub fn new(registry: ActionRegistry, app: &gtk4::Application) -> Self { /* ... */ }

    /// Bind everything: Tier 1 → accels, Tier 2 → per-widget controllers.
    pub fn install(&self, widgets: &ContextWidgets) {
        for action in self.registry.actions() {
            match action.context {
                ActionContext::Global => self.bind_accel(action),
                ctx if ctx.is_widget_context() => {
                    let controller = self.context_controllers.get(&ctx).unwrap();
                    self.add_shortcut(controller, action);
                }
                ActionContext::RdpTab | ActionContext::VncTab => { /* none */ }
            }
        }
    }

    fn add_shortcut(&self, controller: &gtk4::ShortcutController, action: &Action) {
        let Some(trigger) = parse_trigger(&action.current_key) else { return };
        let action_id = action.id.clone();
        let callback = gtk4::CallbackAction::new(move |_widget, _args| {
            // dispatch to the same place the gio action would
            app_dispatch(&action_id);
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk4::Shortcut::new(
            Some(trigger), Some(callback)));
    }

    /// Rebind = swap binding in registry, then rebuild the one affected
    /// controller (Tier 2) or re-set the accel (Tier 1).
    pub fn rebind(&mut self, action_id: &str, new_key: &str) -> Result<(), RebindError> {
        self.registry.rebind(action_id, new_key)?;
        self.rebuild_binding(action_id);
        Ok(())
    }
}
```

**API notes (gtk4-rs 0.11):** `add_controller` takes the controller by
value (ownership moves into the widget); `CallbackAction::new` expects
`Fn(&Widget, Option<&glib::Variant>) -> glib::Propagation` — the
original sketch's `Option<&Widget>` first parameter was wrong.
`ShortcutController::add_shortcut` appends; to rebuild on rebind, create
a fresh controller rather than trying to remove individual shortcuts
(GTK4 has `remove_shortcut` but wholesale rebuild is simpler and
idempotent).

## Trigger Parsing

The original `gtk_key_to_gtk_accel` lowercased the entire string — that
corrupts key names ("Tab", "F11", "Delete" are case-sensitive tokens in
accelerator strings). Correct parse of our UI format:

```rust
/// "Ctrl+Shift+T" → "<Control><Shift>t"
/// "Ctrl+Alt+Delete" → "<Control><Alt>Delete"
/// "Alt+1" → "<Alt>1"          "F11" → "F11"
fn parse_trigger(key: &str) -> Option<gtk4::ShortcutTrigger> {
    let accel = ui_key_to_accel(key)?;
    gtk4::ShortcutTrigger::parse_string(&accel)
    // (binding name in gtk4-rs 0.11 — verify exact form at Phase 4 kickoff)
}

fn ui_key_to_accel(key: &str) -> Option<String> {
    const MODS: &[(&str, &str)] = &[
        ("Ctrl", "<Control>"), ("Shift", "<Shift>"),
        ("Alt", "<Alt>"), ("Super", "<Super>"), ("Meta", "<Meta>"),
    ];
    let tokens: Vec<&str> = key.split('+').collect();
    let key_name = tokens.last()?.to_string();
    if key_name.is_empty() { return None; }          // trailing '+'
    let mut out = String::new();
    for t in &tokens[..tokens.len() - 1] {
        let m = MODS.iter().find(|(ui, _)| *ui == *t)?; // unknown modifier → None
        out.push_str(m.1);
    }
    // Key names keep their case; single letters normalize to lowercase:
    let key_name = if key_name.chars().count() == 1 {
        key_name.to_lowercase()
    } else { key_name };
    out.push_str(&key_name);
    Some(out)
}
```

For key capture in the settings UI, don't parse strings at all: read
the `gdk::EventKey` directly (`event.key()` + `event.modifier_state()`)
and build the trigger from `gtk4::Trigger::from(`key + modifiers`)` —
string round-trips are only for persistence.

## Conflict Detection

With the two-tier design, "conflict" means: the same chord bound twice
within the set of shortcuts that can be simultaneously active. That is:

1. Two Tier-1 (global) actions with the same chord — always conflict.
2. Two Tier-2 actions in the same context — conflict.
3. A Tier-1 and a Tier-2 action with the same chord — conflict (both
   fire when the Tier-2 widget is focused).
4. Two Tier-2 actions in *different* contexts — not a conflict (never
   simultaneously active; this is what makes Ctrl+C safe in SFTP while
   Ctrl+Shift+C is copy in terminal).

```rust
pub fn conflicting_actions(
    registry: &ActionRegistry,
    new_key: &str,
    context: ActionContext,
) -> Vec<&Action> {
    registry.actions().filter(|a| {
        a.current_key == new_key && can_co_fire(&a.context, &context)
    }).collect()
}

fn can_co_fire(a: &ActionContext, b: &ActionContext) -> bool {
    matches!((a, b),
        (Global, _) | (_, Global) | (x, y) if x == y)
}
```

UI shows: "Ctrl+Shift+T is already bound to 'New Tab'. Override?" —
overriding clears (unbinds) the loser.

## Default Actions (de-conflicted)

| Action ID | Display Name | Default Key | Context |
|---|---|---|---|
| `new_tab` | New Tab | Ctrl+Shift+T | global |
| `close_tab` | Close Tab | Ctrl+Shift+W | global |
| `next_tab` | Next Tab | Ctrl+Tab | global |
| `prev_tab` | Previous Tab | Ctrl+Shift+Tab | global |
| `goto_tab_1` … `goto_tab_9` | Go to Tab N | Alt+1 … Alt+9 | global |
| `toggle_sftp` | Toggle SFTP Sidebar | Ctrl+Shift+B | global |
| `toggle_session_tree` | Toggle Session Tree | Ctrl+Shift+S | global |
| `toggle_fullscreen` | Fullscreen | F11 | global |
| `reconnect` | Reconnect | Ctrl+Shift+R | global |
| `disconnect` | Disconnect | Ctrl+Shift+X | global |
| `new_session` | New Session Wizard | Ctrl+N | global |
| `rename_tab` | Rename Tab | Ctrl+Shift+Enter | global |
| `split_horizontal` | Split Horizontal | Ctrl+Shift+D | terminal |
| `split_vertical` | Split Vertical | Ctrl+Shift+E | terminal |
| `close_split` | Close Split | Ctrl+Shift+Q | terminal |
| `search` | Search in Terminal | Ctrl+Shift+F | terminal |
| `search_scrollback` | Search Scrollback | Ctrl+Shift+K | terminal |
| `copy` | Copy Selection | Ctrl+Shift+C | terminal |
| `paste` | Paste | Ctrl+Shift+V | terminal |
| `select_all` | Select All | Ctrl+Shift+A | terminal |
| `clear` | Clear Terminal | Ctrl+Shift+L | terminal |
| `edit_session` | Edit Session | Ctrl+E | session_tree |
| `duplicate_session` | Duplicate Session | Ctrl+D | session_tree |
| `delete_session` | Delete Session | Delete | session_tree |
| `connect_session` | Connect Session | Enter | session_tree |
| `sftp_upload` | Upload | Ctrl+U | sftp |
| `sftp_download` | Download | Ctrl+D | sftp |
| `sftp_rename` | Rename | F2 | sftp |
| `sftp_delete` | Delete | Delete | sftp |

Changes from the previous revision, and why:

- `split_vertical` Ctrl+Shift+V → **Ctrl+Shift+E** — was a direct
  collision with `paste` in the same context.
- `rename_tab` Ctrl+Shift+R → **Ctrl+Shift+Enter** — was a direct
  collision with `reconnect` in the same (global) tier. (F2 was also
  considered but F2 is `sftp_rename` — and a global F2 would co-fire
  with it.)
- `toggle_sftp` Ctrl+Shift+F → **Ctrl+Shift+B** — with the old
  all-Global-scope controllers this silently co-fired with terminal
  `search` whenever a terminal had focus; under the two-tier design it
  would be a Tier-1/Tier-2 co-fire — still wrong, so the chord moves.
- `sftp_*` actions added explicitly (the old table hand-waved
  "copy/paste in sftp context"); sftp gets file-manager-style keys.

Context-safety of the table: Ctrl+D is `duplicate_session` in the tree
and `sftp_download` in the sidebar — different widgets, never
simultaneously active, allowed by rule 4 above.

## Context Switching

Mostly obsolete — Managed scope does the work. What remains:

- `determine_context(focused_widget)` walks up the widget hierarchy
  (VteTerminal → Terminal; session TreeView → SessionTree; SFTP
  TreeView → Sftp; VNC drawing area → VncTab; reparented RDP container
  → RdpTab). Used only for: the settings UI's "active context"
  highlight, and the status indicator.
- RDP/VNC tabs: nothing to enable or disable — by construction only
  Tier-1 accels reach them. Keys not consumed by accels propagate to
  the widget's key-press handler → forwarded to the remote session.

## Settings UI

```rust
pub fn create_hotkey_settings_page(registry: &ActionRegistry) -> gtk4::Widget {
    // ListView (GtkListView + GtkColumnView in 0.11) with columns:
    // action name | key sequence | context | (reset-to-default button)
    // - "Capture Key" button grabs the next key press (via a
    //   GtkEventControllerKey on the dialog, not string parsing)
    // - conflicts resolved through conflicting_actions() above
    // - per-row "Reset to Default" (restores default_key, is_default=1)
}
```

(GtkSingleSelection wraps the selection model around the ListStore —
the original sketch conflated model and view types; use
`gtk4::SingleSelection` with a `gtk4::ListModel`, or the
`ColumnView` API.)

## Persistence

Hotkeys are stored in the SQLite `hotkeys` table (doc 06, schema
unchanged). On startup:

1. `ActionRegistry` registers all actions with default bindings
2. Load user-customized bindings from DB (`WHERE is_default = 0`),
   apply via `HotkeyEngine::rebind` — which rebuilds controllers/accels
3. Rows marked `is_default = 1` are the seed data shipped on first run;
   they are never loaded back over the registry's compiled-in defaults

```rust
impl HotkeyEngine {
    pub fn load_from_db(&mut self, store: &SessionStore) -> Result<(), rusqlite::Error> {
        let bindings = store.load_custom_hotkeys()?; // (action, key) pairs
        for (action_id, key) in bindings {
            let _ = self.rebind(&action_id, &key); // unknown ids: skip, don't crash
        }
        Ok(())
    }
}
```

(Unlike the earlier sketch this rebinds via the engine — updating the
*current* binding — rather than mutating `default_key` metadata.)

Export/Import: hotkey configs export to JSON independently of sessions,
for sharing keyboard layouts between machines.