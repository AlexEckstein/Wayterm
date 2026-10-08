//! Action registry and conflict detection (doc 07 §"Action Registry",
//! §"Conflict Detection").
//!
//! The registry is the *data model* the doc describes: an
//! [`ActionRegistry`] of [`Action`]s, each with a compiled-in
//! `default_key`, an effective `current_key`, the [`ActionContext`]
//! that decides which tier binds it, and the `is_default` flag that
//! distinguishes "still the shipped binding" from "user rebound this".
//! Handlers do not live here — Tier-1 actions are `gio` actions and
//! Tier-2 actions are `gtk4::CallbackAction` shortcuts, so the engine
//! (and, later, the GTK layer) binds the data to them.
//!
//! [`Action`] and [`ActionContext`] are the doc's shapes field-for-field
//! and variant-for-variant. Two deliberate, documented widenings:
//!
//! * `ActionContext` is `Copy` (the doc's `conflicting_actions` takes a
//!   context by value and reads it by reference afterwards);
//! * `ActionContext` gains `as_str` / `from_db_str` for the `hotkeys`
//!   table's `context` column vocabulary (doc 06) — the strings are the
//!   seed's own, not new ones.
//!
//! Conflict detection implements the doc's two-tier rules verbatim:
//! two Tier-1 actions, two Tier-2 actions in the same context, or one
//! of each are conflicts (all three cases collapse into
//! [`can_co_fire`]); two Tier-2 actions in *different* contexts are not,
//! which is what makes `Ctrl+D` safe as both `duplicate_session` and
//! `sftp_download`.

use std::collections::HashMap;

use thiserror::Error;

use crate::hotkeys::trigger::ui_key_is_bindable;

/// Every rebindable action and its compiled-in binding.
///
/// Doc 07 §"Action Registry", transcribed field-for-field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub id: String,           // "new_tab"
    pub display_name: String, // "New Tab"
    pub default_key: String,  // "Ctrl+Shift+T" (UI format, parsed at bind time)
    pub current_key: String,  // effective binding (default until rebound)
    pub context: ActionContext,
    pub enabled: bool,
    pub is_default: bool, // true if current binding == default
}

impl Action {
    /// Whether this action currently holds a chord.
    ///
    /// An override clears the loser of a conflict, so an unbound action
    /// is one whose `current_key` is empty. Such an action takes part in
    /// no chord comparisons — see [`conflicting_actions`].
    pub fn is_bound(&self) -> bool {
        self.enabled && !self.current_key.is_empty()
    }

    /// The context's wire string — the `hotkeys.context` column value
    /// (doc 06) and the settings-UI grouping key.
    pub fn context_str(&self) -> &'static str {
        self.context.as_str()
    }
}

/// Which tier binds an action (doc 07 §"The Two-Tier Mechanism").
///
/// Doc 07 §"Action Registry", variant-for-variant. `Global` is Tier 1
/// (application accelerators); the widget contexts are Tier 2
/// (`ShortcutScope::Managed` controllers); `RdpTab` / `VncTab` are
/// passthrough contexts with no controller of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionContext {
    Global,      // Tier 1: application accelerators
    Terminal,    // Tier 2: attached to terminal area
    SessionTree, // Tier 2: attached to session tree
    Sftp,        // Tier 2: attached to SFTP sidebar
    RdpTab,      // passthrough context — no controller of its own
    VncTab,      // passthrough context — no controller of its own
}

impl ActionContext {
    /// All contexts in declaration (i.e. table) order.
    pub const ALL: [ActionContext; 6] = [
        ActionContext::Global,
        ActionContext::Terminal,
        ActionContext::SessionTree,
        ActionContext::Sftp,
        ActionContext::RdpTab,
        ActionContext::VncTab,
    ];

    /// The doc 06 `hotkeys.context` wire string for this context.
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

    /// Parse a `hotkeys.context` column value (doc 06's seed vocabulary,
    /// plus the two passthrough contexts).
    ///
    /// `None` for anything else — the caller skips such a row, exactly
    /// as unknown action ids are skipped at load time (doc 07
    /// §"Persistence").
    pub fn from_db_str(text: &str) -> Option<ActionContext> {
        ActionContext::ALL
            .into_iter()
            .find(|ctx| ctx.as_str() == text)
    }

    /// Whether the context gets a Tier-2 `ShortcutController`.
    ///
    /// The doc's `install()` matches `ctx if ctx.is_widget_context()`
    /// for the Tier-2 arm and binds nothing at all for
    /// `RdpTab` / `VncTab`, whose keys pass through to the remote
    /// session.
    pub fn is_widget_context(self) -> bool {
        matches!(
            self,
            ActionContext::Terminal | ActionContext::SessionTree | ActionContext::Sftp
        )
    }

    /// Rank in the doc 07 default table's order — used to render the
    /// registry deterministically (settings list, JSON export).
    pub fn rank(self) -> usize {
        match self {
            ActionContext::Global => 0,
            ActionContext::Terminal => 1,
            ActionContext::SessionTree => 2,
            ActionContext::Sftp => 3,
            ActionContext::RdpTab => 4,
            ActionContext::VncTab => 5,
        }
    }
}

/// What a rebind can fail with.
///
/// Doc 07 names only `UnknownAction`; the settings UI needs the other
/// two to render "Override?" and to refuse a binding GTK could never
/// fire. Every variant names the offending input so a UI can report it.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RebindError {
    /// No action is registered under this id (doc 07's
    /// `RebindError::UnknownAction`).
    #[error("no action registered with id {id:?}")]
    UnknownAction { id: String },

    /// The binding is not one GTK can parse or fire: an unknown
    /// modifier, a trailing `'+'`, or a key name GDK does not know.
    #[error("{key:?} is not a bindable key sequence: {reason}")]
    UnparsableKey { key: String, reason: String },

    /// The chord is already held by an action that can fire at the same
    /// time (doc 07 §"Conflict Detection").
    #[error("{key:?} in context {context:?} conflicts with {}", .conflicts.join(", "))]
    Conflict {
        key: String,
        context: ActionContext,
        conflicts: Vec<String>,
    },
}

/// How [`ActionRegistry::rebind_checked`] treats a conflicting chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse the rebind and report the holders (doc 07's UI flow:
    /// "… is already bound to 'New Tab'. Override?" — the user then
    /// chooses).
    Reject,
    /// Take the chord and unbind the losers, as doc 07 specifies for
    /// the override branch: "overriding clears (unbinds) the loser".
    /// Returns the displaced action ids.
    Override,
}

/// The set of rebindable actions (doc 07 §"Action Registry").
///
/// Registration order is preserved only through [`ordered_actions`];
/// the map itself is unordered, as the doc's `HashMap` is.
#[derive(Clone, Debug, Default)]
pub struct ActionRegistry {
    actions: HashMap<String, Action>,
}

impl ActionRegistry {
    /// An empty registry. The shipped bindings live in
    /// [`crate::hotkeys::defaults`] — use
    /// [`default_registry`](crate::hotkeys::default_registry) to get one
    /// populated.
    pub fn new() -> Self {
        Self {
            actions: HashMap::new(),
        }
    }

    /// Register an action with its compiled-in binding
    /// (doc 07 §"Action Registry": insert with `current_key =
    /// default_key`).
    ///
    /// Registering an id twice replaces the entry, which is what makes
    /// registering the default table twice (tests, a rebuilt settings
    /// page) idempotent rather than accumulating stale state.
    pub fn register(
        &mut self,
        id: &str,
        display_name: &str,
        default_key: &str,
        context: ActionContext,
    ) {
        self.actions.insert(
            id.to_string(),
            Action {
                id: id.to_string(),
                display_name: display_name.to_string(),
                default_key: default_key.to_string(),
                current_key: default_key.to_string(),
                context,
                enabled: true,
                is_default: true,
            },
        );
    }

    /// Rebind: update `current_key`, flip `is_default`.
    ///
    /// Doc 07 §"Action Registry", transcribed verbatim: the doc's own
    /// comment notes that `HotkeyEngine` rebuilds the affected
    /// controller / accel afterwards. The GTK rebuild lives in the UI
    /// task; [`crate::hotkeys::HotkeyEngine::rebind`] is the caller that
    /// also validates and checks for conflicts.
    pub fn rebind(&mut self, id: &str, new_key: &str) -> Result<(), RebindError> {
        if let Some(action) = self.actions.get_mut(id) {
            action.current_key = new_key.to_string();
            action.is_default = action.current_key == action.default_key;
            Ok(())
        } else {
            Err(RebindError::UnknownAction { id: id.to_string() })
        }
    }

    /// Rebind with the doc's settings-UI checks: the binding must be
    /// parseable and GTK-fireable, and it must not collide with an
    /// action that can fire at the same time.
    ///
    /// Nothing is mutated unless every check passes. Returns the ids
    /// this rebind displaced — empty under [`ConflictPolicy::Reject`]
    /// and when there was no conflict, and only meaningful under
    /// [`ConflictPolicy::Override`].
    pub fn rebind_checked(
        &mut self,
        id: &str,
        new_key: &str,
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, RebindError> {
        let Some(action) = self.actions.get(id) else {
            return Err(RebindError::UnknownAction { id: id.to_string() });
        };
        let context = action.context;
        if !ui_key_is_bindable(new_key) {
            return Err(RebindError::UnparsableKey {
                key: new_key.to_string(),
                reason: unparsable_reason(new_key),
            });
        }
        // Conflicts with *other* actions only: rebinding an action to
        // the chord it already holds is not a conflict.
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
        if policy == ConflictPolicy::Override {
            for other in &conflicts {
                if let Some(action) = self.actions.get_mut(other) {
                    action.current_key.clear();
                    action.enabled = false;
                    // An empty binding is not the compiled-in default.
                    action.is_default = false;
                }
            }
        }
        self.rebind(id, new_key)?;
        // A rebound action is enabled again: the user just gave it a
        // chord, whether that chord is the default one or not.
        if let Some(action) = self.actions.get_mut(id) {
            action.enabled = true;
        }
        Ok(conflicts)
    }

    /// Restore `default_key`, `enabled`, and `is_default` — doc 07
    /// §"Settings UI": per-row "Reset to Default".
    pub fn reset_to_default(&mut self, id: &str) -> Result<(), RebindError> {
        let Some(action) = self.actions.get(id) else {
            return Err(RebindError::UnknownAction { id: id.to_string() });
        };
        let default_key = action.default_key.clone();
        let action = self.actions.get_mut(id).expect("just looked it up");
        action.current_key = default_key;
        action.enabled = true;
        action.is_default = true;
        Ok(())
    }

    /// Enable or disable an action without touching its binding.
    ///
    /// A disabled action holds no chord: [`Action::is_bound`] goes
    /// false and it stops conflicting with anything
    /// ([`conflicting_actions`]).
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), RebindError> {
        let Some(action) = self.actions.get_mut(id) else {
            return Err(RebindError::UnknownAction { id: id.to_string() });
        };
        action.enabled = enabled;
        Ok(())
    }

    /// Apply a loaded binding verbatim (doc 07 §"Persistence" step 2).
    ///
    /// The doc's load path mutates the *current* binding without
    /// rebuilding anything, and skips rows it cannot apply; this is that
    /// mutation, factored out so `load_from_db` can tell "applied" from
    /// "skipped" without duplicating the flip.
    pub fn apply_loaded_binding(
        &mut self,
        id: &str,
        key: &str,
        enabled: bool,
    ) -> Result<(), RebindError> {
        if !self.actions.contains_key(id) {
            return Err(RebindError::UnknownAction { id: id.to_string() });
        }
        self.rebind(id, key)?;
        if let Some(action) = self.actions.get_mut(id) {
            action.enabled = enabled;
        }
        Ok(())
    }

    /// The action registered under `id`, if any.
    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions.get(id)
    }

    /// Mutable access to one action. Reserved for the UI task, which
    /// sets per-action metadata; rebinds go through [`Self::rebind`] so
    /// `is_default` cannot go stale.
    pub fn action_mut(&mut self, id: &str) -> Option<&mut Action> {
        self.actions.get_mut(id)
    }

    /// Whether `id` is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.actions.contains_key(id)
    }

    /// Number of registered actions.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Whether the registry holds no actions.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Every action, in unspecified order (the doc's `install()` walks
    /// this).
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        self.actions.values()
    }

    /// Every action in doc 07 default-table order (context rank, then
    /// id) — the settings list's order, and the order JSON export uses.
    pub fn ordered_actions(&self) -> Vec<&Action> {
        let mut actions: Vec<&Action> = self.actions.values().collect();
        actions.sort_by(|a, b| {
            a.context
                .rank()
                .cmp(&b.context.rank())
                .then_with(|| a.id.cmp(&b.id))
        });
        actions
    }

    /// Actions deviating from their compiled-in state: rebound *or*
    /// disabled. These are exactly the rows doc 06 persists as
    /// `is_default = 0`.
    pub fn custom_actions(&self) -> Vec<&Action> {
        self.ordered_actions()
            .into_iter()
            .filter(|a| !a.is_default || !a.enabled)
            .collect()
    }

    /// Actions that would conflict with binding `new_key` to `id`,
    /// excluding `id` itself. See [`conflicting_actions`] for the rules.
    pub fn conflicts_for(&self, id: &str, new_key: &str) -> Vec<&Action> {
        let Some(context) = self.actions.get(id).map(|a| a.context) else {
            return Vec::new();
        };
        conflicting_actions(self, new_key, context)
            .into_iter()
            .filter(|a| a.id != id)
            .collect()
    }

    /// The effective binding of `id` as a UI-format string, for the
    /// settings UI's "key sequence" column.
    pub fn current_key(&self, id: &str) -> Option<&str> {
        self.actions.get(id).map(|a| a.current_key.as_str())
    }

    /// Context of `id`.
    pub fn context_of(&self, id: &str) -> Option<ActionContext> {
        self.actions.get(id).map(|a| a.context)
    }
}

/// Can two contexts' shortcuts be active at the same time?
///
/// Doc 07 §"Conflict Detection": a Tier-1 (global) accel and a Tier-2
/// shortcut with the same chord both fire when the Tier-2 widget has
/// focus, and two Tier-2 shortcuts fire together only within one
/// context. So: either side being [`ActionContext::Global`] co-fires,
/// and otherwise only equal contexts do.
///
/// The doc spells this as a single `matches!` with a trailing
/// `x == y` guard; guard bindings must be bound by every alternative
/// reaching the guard, so the same predicate is written here as the
/// disjunction it is. Doc rules 1, 2 and 3 are this function; rule 4
/// (different Tier-2 contexts never co-fire) is its negation.
pub fn can_co_fire(a: &ActionContext, b: &ActionContext) -> bool {
    matches!(
        (a, b),
        (ActionContext::Global, _) | (_, ActionContext::Global)
    ) || a == b
}

/// Actions that already hold `new_key` in a context that can fire
/// alongside `context` (doc 07 §"Conflict Detection").
///
/// Unbound actions (cleared by an override, or disabled) are skipped —
/// they hold no chord, so they cannot conflict. The caller rebinding an
/// action must exclude that action itself; [`ActionRegistry::conflicts_for`]
/// does.
pub fn conflicting_actions<'a>(
    registry: &'a ActionRegistry,
    new_key: &str,
    context: ActionContext,
) -> Vec<&'a Action> {
    registry
        .actions()
        .filter(|a| a.is_bound() && a.current_key == new_key && can_co_fire(&a.context, &context))
        .collect()
}

/// Why a binding cannot be used, for [`RebindError::UnparsableKey`].
fn unparsable_reason(key: &str) -> String {
    let token = crate::hotkeys::trigger::key_token(key);
    if token.is_empty() {
        "empty key".to_string()
    } else if !crate::hotkeys::trigger::key_token_is_bindable(token) {
        format!("unknown key name {token:?}")
    } else {
        format!("unknown modifier in {key:?}")
    }
}

#[cfg(test)]
mod tests;
