//! The compiled-in default bindings: doc 07 §Default Actions.
//!
//! The table is de-conflicted (the tests enforce it): the only chords that
//! repeat are `Ctrl+D` and `Delete`, each in two Tier-2 contexts that are
//! never active together. One deliberate deviation from the doc's printed
//! table: the Enter key is spelled `Return`, because GDK has no key named
//! `Enter` and such a binding could never fire.

use crate::hotkeys::registry::{ActionContext, ActionRegistry};

/// One row of the default table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultAction {
    pub id: &'static str,
    pub display_name: &'static str,
    /// UI format (`"Ctrl+Shift+T"`).
    pub default_key: &'static str,
    pub context: ActionContext,
}

const fn a(
    id: &'static str,
    display_name: &'static str,
    default_key: &'static str,
    context: ActionContext,
) -> DefaultAction {
    DefaultAction {
        id,
        display_name,
        default_key,
        context,
    }
}

use ActionContext::{Global as G, SessionTree as S, Sftp as F, Terminal as T};

/// Doc 07's table in its order; `goto_tab_1` … `goto_tab_9` is one printed
/// row, so 37 actions in all.
#[rustfmt::skip]
pub const DEFAULT_ACTIONS: &[DefaultAction] = &[
    a("new_tab", "New Tab", "Ctrl+Shift+T", G),
    a("close_tab", "Close Tab", "Ctrl+Shift+W", G),
    a("next_tab", "Next Tab", "Ctrl+Tab", G),
    a("prev_tab", "Previous Tab", "Ctrl+Shift+Tab", G),
    a("goto_tab_1", "Go to Tab 1", "Alt+1", G),
    a("goto_tab_2", "Go to Tab 2", "Alt+2", G),
    a("goto_tab_3", "Go to Tab 3", "Alt+3", G),
    a("goto_tab_4", "Go to Tab 4", "Alt+4", G),
    a("goto_tab_5", "Go to Tab 5", "Alt+5", G),
    a("goto_tab_6", "Go to Tab 6", "Alt+6", G),
    a("goto_tab_7", "Go to Tab 7", "Alt+7", G),
    a("goto_tab_8", "Go to Tab 8", "Alt+8", G),
    a("goto_tab_9", "Go to Tab 9", "Alt+9", G),
    a("toggle_sftp", "Toggle SFTP Sidebar", "Ctrl+Shift+B", G),
    a("toggle_session_tree", "Toggle Session Tree", "Ctrl+Shift+S", G),
    a("toggle_fullscreen", "Fullscreen", "F11", G),
    a("reconnect", "Reconnect", "Ctrl+Shift+R", G),
    a("disconnect", "Disconnect", "Ctrl+Shift+X", G),
    a("new_session", "New Session Wizard", "Ctrl+N", G),
    a("rename_tab", "Rename Tab", "Ctrl+Shift+Return", G),
    a("split_horizontal", "Split Horizontal", "Ctrl+Shift+D", T),
    a("split_vertical", "Split Vertical", "Ctrl+Shift+E", T),
    a("close_split", "Close Split", "Ctrl+Shift+Q", T),
    a("search", "Search in Terminal", "Ctrl+Shift+F", T),
    a("search_scrollback", "Search Scrollback", "Ctrl+Shift+K", T),
    a("copy", "Copy Selection", "Ctrl+Shift+C", T),
    a("paste", "Paste", "Ctrl+Shift+V", T),
    a("select_all", "Select All", "Ctrl+Shift+A", T),
    a("clear", "Clear Terminal", "Ctrl+Shift+L", T),
    a("edit_session", "Edit Session", "Ctrl+E", S),
    a("duplicate_session", "Duplicate Session", "Ctrl+D", S),
    a("delete_session", "Delete Session", "Delete", S),
    a("connect_session", "Connect Session", "Return", S),
    a("sftp_upload", "Upload", "Ctrl+U", F),
    a("sftp_download", "Download", "Ctrl+D", F),
    a("sftp_rename", "Rename", "F2", F),
    a("sftp_delete", "Delete", "Delete", F),
];

/// A registry holding the default table.
pub fn default_registry() -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    for default in DEFAULT_ACTIONS {
        registry.register(
            default.id,
            default.display_name,
            default.default_key,
            default.context,
        );
    }
    registry
}

#[cfg(test)]
mod tests;
