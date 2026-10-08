//! The compiled-in default bindings (doc 07 §"Default Actions").
//!
//! [`DEFAULT_ACTIONS`] is doc 07's de-conflicted default table,
//! transcribed row-for-row — action id, display name, UI-format key,
//! context — and registered into every fresh
//! [`ActionRegistry`](crate::hotkeys::ActionRegistry) so
//! [`HotkeyEngine::load_from_db`](crate::hotkeys::HotkeyEngine::load_from_db)
//! has something to rebind against.
//!
//! The de-confliction is the table's whole point (doc 07 §"Default
//! Actions", "Changes from the previous revision, and why"), so it is
//! locked by tests here rather than by convention: `defaults_are_parsed`
//! walks every default key through the doc's parser (no silently dead
//! binding), `defaults_are_conflict_free` runs the doc's four conflict
//! rules over the whole table, and `doc06_seed_rows_match_doc07_table`
//! pins the table against doc 06's seed SQL.
//!
//! ## Two divergences from the docs, both reported to the orchestrator
//!
//! 1. **The Enter key is spelled `Return`.** Doc 07's table binds
//!    `rename_tab` to `Ctrl+Shift+Enter` and `connect_session` to
//!    `Enter`, but `Enter` is not a GDK key name:
//!    `gdk_keyval_from_name("Enter")` returns
//!    [`VoidSymbol`](gdk4::Key::VoidSymbol), so GTK rejects
//!    `"<Control>Enter"` and *both bindings would silently never fire*.
//!    GTK's canonical name for that key is `Return` — which is also
//!    what the doc's own mandated capture path produces, since
//!    `gdk::Key::name()` for the Enter key is `Return`. The table
//!    therefore uses `Ctrl+Shift+Return` / `Return`, and
//!    `enter_spelling_is_the_only_deviation` asserts both halves of
//!    that finding. Doc 07 needs the fix, not this code.
//!
//! 2. **`goto_tab_1` … `goto_tab_9` are here but not in doc 06's seed.**
//!    Doc 07's table has the row; doc 06 §"Default Data" — which says
//!    its hotkey rows "MUST stay in sync with doc 07's default-actions
//!    table" — seeds 28 rows and omits those nine. Per the task
//!    contract the code follows doc 07's table; doc 06's seed needs
//!    nine rows added. [`seed_rows`] generates what doc 06 *should*
//!    seed; `doc06_seed_rows_match_doc07_table` compares it against the
//!    verbatim seed in `db.rs` and asserts the seed's rows are a
//!    subset with identical keys, which fails loudly if a key ever
//!    drifts.
//!
//! The Enter-key spelling also means [`seed_rows`] differs from doc 06's
//! current seed in two rows (`rename_tab`, `connect_session`) for the
//! reason above.

use crate::hotkeys::registry::{ActionContext, ActionRegistry};

/// One row of doc 07 §"Default Actions".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultAction {
    pub id: &'static str,
    pub display_name: &'static str,
    /// UI format (`"Ctrl+Shift+T"`), parsed at bind time (doc 07).
    pub default_key: &'static str,
    pub context: ActionContext,
}

/// Doc 07 §"Default Actions" — the de-conflicted default table.
///
/// Twenty-nine table rows; the `goto_tab_1` … `goto_tab_9` row expands
/// to nine actions, so 37 in all. Order is the doc's table order:
/// global first, then terminal, session tree, sftp.
///
/// The `Ctrl+D` / `Delete` chords that appear twice are legal on
/// purpose: they are Tier-2 actions in *different* contexts (session
/// tree and sftp), which doc 07 rule 4 says can never fire together —
/// and that pairing is the doc's own worked example.
pub const DEFAULT_ACTIONS: &[DefaultAction] = &[
    DefaultAction {
        id: "new_tab",
        display_name: "New Tab",
        default_key: "Ctrl+Shift+T",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "close_tab",
        display_name: "Close Tab",
        default_key: "Ctrl+Shift+W",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "next_tab",
        display_name: "Next Tab",
        default_key: "Ctrl+Tab",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "prev_tab",
        display_name: "Previous Tab",
        default_key: "Ctrl+Shift+Tab",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_1",
        display_name: "Go to Tab 1",
        default_key: "Alt+1",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_2",
        display_name: "Go to Tab 2",
        default_key: "Alt+2",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_3",
        display_name: "Go to Tab 3",
        default_key: "Alt+3",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_4",
        display_name: "Go to Tab 4",
        default_key: "Alt+4",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_5",
        display_name: "Go to Tab 5",
        default_key: "Alt+5",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_6",
        display_name: "Go to Tab 6",
        default_key: "Alt+6",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_7",
        display_name: "Go to Tab 7",
        default_key: "Alt+7",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_8",
        display_name: "Go to Tab 8",
        default_key: "Alt+8",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "goto_tab_9",
        display_name: "Go to Tab 9",
        default_key: "Alt+9",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "toggle_sftp",
        display_name: "Toggle SFTP Sidebar",
        default_key: "Ctrl+Shift+B",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "toggle_session_tree",
        display_name: "Toggle Session Tree",
        default_key: "Ctrl+Shift+S",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "toggle_fullscreen",
        display_name: "Fullscreen",
        default_key: "F11",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "reconnect",
        display_name: "Reconnect",
        default_key: "Ctrl+Shift+R",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "disconnect",
        display_name: "Disconnect",
        default_key: "Ctrl+Shift+X",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "new_session",
        display_name: "New Session Wizard",
        default_key: "Ctrl+N",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "rename_tab",
        display_name: "Rename Tab",
        // Doc 07 says `Ctrl+Shift+Enter`; `Enter` is not a GDK key
        // name, so that binding would never fire. See the module note.
        default_key: "Ctrl+Shift+Return",
        context: ActionContext::Global,
    },
    DefaultAction {
        id: "split_horizontal",
        display_name: "Split Horizontal",
        default_key: "Ctrl+Shift+D",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "split_vertical",
        display_name: "Split Vertical",
        default_key: "Ctrl+Shift+E",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "close_split",
        display_name: "Close Split",
        default_key: "Ctrl+Shift+Q",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "search",
        display_name: "Search in Terminal",
        default_key: "Ctrl+Shift+F",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "search_scrollback",
        display_name: "Search Scrollback",
        default_key: "Ctrl+Shift+K",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "copy",
        display_name: "Copy Selection",
        default_key: "Ctrl+Shift+C",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "paste",
        display_name: "Paste",
        default_key: "Ctrl+Shift+V",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "select_all",
        display_name: "Select All",
        default_key: "Ctrl+Shift+A",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "clear",
        display_name: "Clear Terminal",
        default_key: "Ctrl+Shift+L",
        context: ActionContext::Terminal,
    },
    DefaultAction {
        id: "edit_session",
        display_name: "Edit Session",
        default_key: "Ctrl+E",
        context: ActionContext::SessionTree,
    },
    DefaultAction {
        id: "duplicate_session",
        display_name: "Duplicate Session",
        default_key: "Ctrl+D",
        context: ActionContext::SessionTree,
    },
    DefaultAction {
        id: "delete_session",
        display_name: "Delete Session",
        default_key: "Delete",
        context: ActionContext::SessionTree,
    },
    DefaultAction {
        id: "connect_session",
        display_name: "Connect Session",
        // Doc 07 says `Enter`; see the module note on `Return`.
        default_key: "Return",
        context: ActionContext::SessionTree,
    },
    DefaultAction {
        id: "sftp_upload",
        display_name: "Upload",
        default_key: "Ctrl+U",
        context: ActionContext::Sftp,
    },
    DefaultAction {
        id: "sftp_download",
        display_name: "Download",
        default_key: "Ctrl+D",
        context: ActionContext::Sftp,
    },
    DefaultAction {
        id: "sftp_rename",
        display_name: "Rename",
        default_key: "F2",
        context: ActionContext::Sftp,
    },
    DefaultAction {
        id: "sftp_delete",
        display_name: "Delete",
        default_key: "Delete",
        context: ActionContext::Sftp,
    },
];

/// Number of rows in doc 07's printed table (the `goto_tab_1` …
/// `goto_tab_9` row counts once).
pub const DEFAULT_TABLE_ROWS: usize = 29;

/// Register every row of [`DEFAULT_ACTIONS`] into `registry`.
pub fn register_defaults(registry: &mut ActionRegistry) {
    for default in DEFAULT_ACTIONS {
        registry.register(
            default.id,
            default.display_name,
            default.default_key,
            default.context,
        );
    }
}

/// A registry holding doc 07's default table — what
/// [`HotkeyEngine::new`](crate::hotkeys::HotkeyEngine::new) starts from.
pub fn default_registry() -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    register_defaults(&mut registry);
    registry
}

/// One `hotkeys` row as doc 06 §"Default Data" seeds it: an
/// `is_default = 1` row for a compiled-in default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedRow {
    pub action: &'static str,
    pub key_sequence: &'static str,
    pub context: &'static str,
    pub is_default: bool,
}

/// The `is_default = 1` rows doc 06 should seed: doc 07's table,
/// rendered in the `hotkeys` table's column order and vocabulary.
///
/// Generated from [`DEFAULT_ACTIONS`] (one source of truth) rather than
/// written out again, so a table edit cannot leave the seed behind. The
/// seed SQL currently in `db.rs` is doc 06's verbatim transcription and
/// stays untouched — see the module note for the two rows where they
/// legitimately differ.
pub fn seed_rows() -> impl Iterator<Item = SeedRow> + 'static {
    DEFAULT_ACTIONS.iter().map(|default| SeedRow {
        action: default.id,
        key_sequence: default.default_key,
        context: default.context.as_str(),
        is_default: true,
    })
}

#[cfg(test)]
mod tests;
