//! The action registry and the conflict rules (doc 11 §Action registry,
//! §Conflict detection).
//!
//! A chord conflicts when both holders can be active at once: anything
//! against a global action, two actions in the same context, or a terminal
//! action against a session-tree or SFTP one (terminal shortcuts stay live
//! while those panels have focus). The session tree and SFTP never
//! co-fire, which is what lets `Ctrl+D` be both `duplicate_session` and
//! `sftp_download`. Keys are stored canonical, so chords compare by key,
//! not by spelling. Handlers live in the GTK layer, not here.

use std::collections::HashMap;

use thiserror::Error;

use crate::hotkeys::trigger::{
    canonical_ui_key, key_token, key_token_is_bindable, ui_key_is_bindable,
};

/// One rebindable action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    pub display_name: String,
    /// The compiled-in binding, canonical UI format (`"Ctrl+Shift+T"`).
    pub default_key: String,
    /// The effective binding, canonical; empty once an override unbinds it.
    pub current_key: String,
    pub context: ActionContext,
    pub enabled: bool,
    /// `current_key == default_key`.
    pub is_default: bool,
}

impl Action {
    /// Whether the action holds a chord (unbound and disabled ones do not
    /// take part in conflicts).
    pub fn is_bound(&self) -> bool {
        self.enabled && !self.current_key.is_empty()
    }
}

/// Where an action's chord fires (doc 11 §Dispatch). Declared in
/// default-table order, which `Ord` follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionContext {
    /// Anywhere in the main window.
    Global,
    /// The current tab's terminal, unless focus is in a text field.
    Terminal,
    /// Focus inside the session tree.
    SessionTree,
    /// Focus inside the side dock's SFTP panel.
    Sftp,
    /// Passthrough: no actions, keys go to the remote session.
    RdpTab,
    VncTab,
}

impl ActionContext {
    pub const ALL: [ActionContext; 6] = [
        ActionContext::Global,
        ActionContext::Terminal,
        ActionContext::SessionTree,
        ActionContext::Sftp,
        ActionContext::RdpTab,
        ActionContext::VncTab,
    ];

    /// The `hotkeys.context` column value (doc 09).
    pub fn as_str(self) -> &'static str {
        match self {
            ActionContext::Global => "global",
            ActionContext::Terminal => "terminal",
            ActionContext::SessionTree => "session_tree",
            ActionContext::Sftp => "sftp",
            ActionContext::RdpTab => "rdp_tab",
            ActionContext::VncTab => "vnc_tab",
        }
    }

    pub fn from_db_str(text: &str) -> Option<ActionContext> {
        ActionContext::ALL
            .into_iter()
            .find(|ctx| ctx.as_str() == text)
    }

    /// Whether the context fires only while focus is in one widget (the
    /// session tree or the SFTP panel).
    pub fn is_focus_context(self) -> bool {
        matches!(self, ActionContext::SessionTree | ActionContext::Sftp)
    }
}

/// Why a rebind was refused; each variant names the offending input.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RebindError {
    #[error("no action registered with id {id:?}")]
    UnknownAction { id: String },

    /// An unknown modifier, a trailing `'+'`, or a key GDK does not know.
    #[error("{key:?} is not a bindable key sequence: {reason}")]
    UnparsableKey { key: String, reason: String },

    /// The chord is held by an action that can fire at the same time.
    #[error("{key:?} in context {context:?} conflicts with {}", .conflicts.join(", "))]
    Conflict {
        key: String,
        context: ActionContext,
        conflicts: Vec<String>,
    },
}

/// What [`ActionRegistry::rebind_checked`] does with a conflicting chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse and report the holders, so the UI can ask "Override?".
    Reject,
    /// Take the chord and unbind (and disable) the holders.
    Override,
}

#[derive(Clone, Debug, Default)]
pub struct ActionRegistry {
    actions: HashMap<String, Action>,
}

impl ActionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) an action at its compiled-in binding.
    pub fn register(
        &mut self,
        id: &str,
        display_name: &str,
        default_key: &str,
        context: ActionContext,
    ) {
        let default_key = stored_key(default_key);
        self.actions.insert(
            id.to_string(),
            Action {
                id: id.to_string(),
                display_name: display_name.to_string(),
                current_key: default_key.clone(),
                default_key,
                context,
                enabled: true,
                is_default: true,
            },
        );
    }

    /// Set `current_key` unchecked (canonicalized when it parses), keeping
    /// `is_default` in step.
    pub fn rebind(&mut self, id: &str, new_key: &str) -> Result<(), RebindError> {
        let action = self.get_mut(id)?;
        action.current_key = stored_key(new_key);
        action.is_default = action.current_key == action.default_key;
        Ok(())
    }

    /// Rebind with the settings UI's checks: the key must be one GTK can
    /// fire, and conflicts are handled per `policy`. Nothing changes unless
    /// every check passes. Returns the ids an override displaced.
    pub fn rebind_checked(
        &mut self,
        id: &str,
        new_key: &str,
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, RebindError> {
        let context = self.get_mut(id)?.context;
        if !ui_key_is_bindable(new_key) {
            return Err(RebindError::UnparsableKey {
                key: new_key.to_string(),
                reason: unparsable_reason(new_key),
            });
        }
        let mut conflicts: Vec<String> = self
            .conflicts_for(id, new_key)
            .into_iter()
            .map(|a| a.id.clone())
            .collect();
        conflicts.sort();
        if !conflicts.is_empty() && policy == ConflictPolicy::Reject {
            return Err(RebindError::Conflict {
                key: new_key.to_string(),
                context,
                conflicts,
            });
        }
        for loser in &conflicts {
            let loser = self.get_mut(loser)?;
            loser.current_key.clear();
            loser.enabled = false;
            loser.is_default = false;
        }
        self.rebind(id, new_key)?;
        self.get_mut(id)?.enabled = true;
        Ok(conflicts)
    }

    /// Per-row "Reset to Default" (doc 11 §Settings page).
    pub fn reset_to_default(&mut self, id: &str) -> Result<(), RebindError> {
        let action = self.get_mut(id)?;
        action.current_key = action.default_key.clone();
        action.enabled = true;
        action.is_default = true;
        Ok(())
    }

    /// Enable or disable without touching the binding.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), RebindError> {
        self.get_mut(id)?.enabled = enabled;
        Ok(())
    }

    /// Apply a stored binding as-is (doc 11 §Persistence).
    pub fn apply_loaded_binding(
        &mut self,
        id: &str,
        key: &str,
        enabled: bool,
    ) -> Result<(), RebindError> {
        self.rebind(id, key)?;
        self.get_mut(id)?.enabled = enabled;
        Ok(())
    }

    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions.get(id)
    }

    /// Mutable access for metadata; rebind through [`Self::rebind`] so
    /// `is_default` cannot go stale.
    pub fn action_mut(&mut self, id: &str) -> Option<&mut Action> {
        self.actions.get_mut(id)
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Action, RebindError> {
        self.actions
            .get_mut(id)
            .ok_or_else(|| RebindError::UnknownAction { id: id.to_string() })
    }

    pub fn contains(&self, id: &str) -> bool {
        self.actions.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Every action, unordered.
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        self.actions.values()
    }

    /// Every action in default-table order (context, then id).
    pub fn ordered_actions(&self) -> Vec<&Action> {
        let mut actions: Vec<&Action> = self.actions.values().collect();
        actions.sort_by(|a, b| (a.context, &a.id).cmp(&(b.context, &b.id)));
        actions
    }

    /// Actions rebound or disabled — the rows persisted as `is_default = 0`.
    pub fn custom_actions(&self) -> Vec<&Action> {
        self.ordered_actions()
            .into_iter()
            .filter(|a| !a.is_default || !a.enabled)
            .collect()
    }

    /// Other actions that would conflict with `id` taking `new_key`.
    pub fn conflicts_for(&self, id: &str, new_key: &str) -> Vec<&Action> {
        let Some(context) = self.context_of(id) else {
            return Vec::new();
        };
        conflicting_actions(self, new_key, context)
            .into_iter()
            .filter(|a| a.id != id)
            .collect()
    }

    pub fn current_key(&self, id: &str) -> Option<&str> {
        self.actions.get(id).map(|a| a.current_key.as_str())
    }

    pub fn context_of(&self, id: &str) -> Option<ActionContext> {
        self.actions.get(id).map(|a| a.context)
    }
}

/// Can shortcuts in these two contexts be active at the same time? Doc 11
/// rules 1–4 are this predicate; rule 5 is its negation.
pub fn can_co_fire(a: &ActionContext, b: &ActionContext) -> bool {
    use ActionContext::{Global, Terminal};
    let terminal_and_panel =
        |x: &ActionContext, y: &ActionContext| *x == Terminal && y.is_focus_context();
    *a == Global || *b == Global || a == b || terminal_and_panel(a, b) || terminal_and_panel(b, a)
}

/// Bound actions holding `new_key`'s chord, however it is spelled, in a
/// context that co-fires with `context` (doc 11 §Conflict detection).
pub fn conflicting_actions<'a>(
    registry: &'a ActionRegistry,
    new_key: &str,
    context: ActionContext,
) -> Vec<&'a Action> {
    let new_key = stored_key(new_key);
    registry
        .actions()
        .filter(|a| a.is_bound() && a.current_key == new_key && can_co_fire(&a.context, &context))
        .collect()
}

/// `key` as the registry stores it: canonical when it parses, as given
/// otherwise (an unchecked rebind can store anything; it never fires).
fn stored_key(key: &str) -> String {
    canonical_ui_key(key).unwrap_or_else(|| key.to_string())
}

fn unparsable_reason(key: &str) -> String {
    let token = key_token(key);
    if token.is_empty() {
        "empty key".to_string()
    } else if !key_token_is_bindable(token) {
        format!("unknown key name {token:?}")
    } else {
        format!("unknown modifier in {key:?}")
    }
}

#[cfg(test)]
mod tests;
