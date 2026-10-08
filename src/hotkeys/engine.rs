//! The hotkey engine's data half (doc 07 §"HotkeyEngine",
//! §"Persistence").
//!
//! [`HotkeyEngine`] owns the [`ActionRegistry`] and reconciles it with
//! the SQLite `hotkeys` table (doc 06). Everything that would need GTK
//! is deliberately absent: doc 07's `install()` — Tier-1 accels on the
//! `GtkApplication` plus one `ShortcutController` per widget context —
//! belongs to the UI task, along with `HotkeyEngine::app`,
//! `context_controllers`, and `widget_for_context`. The seam is marked
//! below so it is obvious what lands where.
//!
//! Persistence follows doc 07 §"Persistence" exactly:
//!
//! 1. the registry is populated with the compiled-in defaults
//!    ([`crate::hotkeys::default_registry`]);
//! 2. user-customized rows (`WHERE is_default = 0`) are loaded and
//!    applied to the *current* binding — never to `default_key`;
//! 3. rows marked `is_default = 1` are first-run seed data and are
//!    never loaded back over the registry's defaults, so a re-seeded or
//!    hand-edited default can never override the compiled-in table;
//! 4. an unknown action id is skipped, not an error — a config written
//!    by a newer build (or a pruned build) must still load.
//!
//! Writes go the other way: [`HotkeyEngine::save_to_db`] persists one
//! `is_default = 0` row per deviating action and removes the row of
//! every action that is back at its default, which is what makes
//! "Reset to Default" survive a restart.

use crate::hotkeys::defaults;
use crate::hotkeys::registry::{
    can_co_fire, conflicting_actions, Action, ActionContext, ActionRegistry, ConflictPolicy,
    RebindError,
};
use crate::hotkeys::transfer;
use crate::hotkeys::trigger::{ui_key_is_bindable, KeyChord};
use crate::store::db::SessionStore;

/// What [`HotkeyEngine::load_from_db`] did with the stored rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotkeyLoadReport {
    /// Action ids rebound from the database, in load order.
    pub applied: Vec<String>,
    /// Rows deliberately not applied: unknown action ids, and non-empty
    /// key sequences GTK could never fire (doc 07 skips unknown ids;
    /// the key check keeps a corrupt row from silently producing a
    /// dead binding).
    pub skipped: Vec<String>,
}

/// Registry plus persistence. The GTK install seam is the UI task's
/// (doc 07 §"HotkeyEngine").
///
/// ```ignore
/// // Doc 07 §"HotkeyEngine" — lands with the GTK binding layer:
/// //   pub fn install(&self, widgets: &ContextWidgets) { … }
/// //     Global      → app.set_accels_for_action(id, &[accel])   (Tier 1)
/// //     widget ctx  → widget.add_controller(ShortcutController)  (Tier 2)
/// //     Rdp/VncTab  → nothing; keys pass through to the session
/// // and rebind() then rebuilds only the affected accel / controller:
/// //   pub fn rebuild_binding(&mut self, action_id: &str) { … }
/// ```
#[derive(Clone, Debug, Default)]
pub struct HotkeyEngine {
    registry: ActionRegistry,
}

impl HotkeyEngine {
    /// An engine over doc 07's compiled-in default table.
    pub fn new() -> Self {
        Self {
            registry: defaults::default_registry(),
        }
    }

    /// An engine over a caller-built registry (tests, a future plugin
    /// adding its own actions — doc 05).
    pub fn with_registry(registry: ActionRegistry) -> Self {
        Self { registry }
    }

    /// The registry, read-only.
    pub fn registry(&self) -> &ActionRegistry {
        &self.registry
    }

    /// The registry, mutable.
    ///
    /// Escape hatch for the UI task's per-action metadata; rebinds go
    /// through [`Self::rebind`] so `is_default` and the conflict rules
    /// cannot be bypassed.
    pub fn registry_mut(&mut self) -> &mut ActionRegistry {
        &mut self.registry
    }

    /// Every action, in doc 07 table order.
    pub fn actions(&self) -> Vec<&Action> {
        self.registry.ordered_actions()
    }

    /// One action by id.
    pub fn action(&self, id: &str) -> Option<&Action> {
        self.registry.action(id)
    }

    /// Rebind, refusing a chord another live action already holds
    /// (doc 07 §"Conflict Detection").
    ///
    /// Returns the ids displaced by the rebind — always empty under
    /// this policy, which is the one the settings dialog calls when the
    /// user has *not* answered "Override?".
    pub fn rebind(&mut self, action_id: &str, new_key: &str) -> Result<Vec<String>, RebindError> {
        self.rebind_with(action_id, new_key, ConflictPolicy::Reject)
    }

    /// Rebind under an explicit conflict policy; returns the ids the
    /// rebind displaced.
    ///
    /// [`ConflictPolicy::Override`] implements doc 07's UI contract:
    /// "overriding clears (unbinds) the loser" — the losers lose their
    /// chord and are disabled, and this engine's later
    /// [`Self::save_to_db`] persists that state.
    pub fn rebind_with(
        &mut self,
        action_id: &str,
        new_key: &str,
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, RebindError> {
        self.registry.rebind_checked(action_id, new_key, policy)
    }

    /// Restore an action's compiled-in binding (doc 07 §"Settings UI":
    /// per-row "Reset to Default").
    pub fn reset_to_default(&mut self, action_id: &str) -> Result<(), RebindError> {
        self.registry.reset_to_default(action_id)
    }

    /// Enable or disable an action.
    pub fn set_enabled(&mut self, action_id: &str, enabled: bool) -> Result<(), RebindError> {
        self.registry.set_enabled(action_id, enabled)
    }

    /// Actions that would conflict with `action_id` taking `new_key`.
    pub fn conflicts_for(&self, action_id: &str, new_key: &str) -> Vec<&Action> {
        self.registry.conflicts_for(action_id, new_key)
    }

    /// Actions holding `new_key` in a context that can fire alongside
    /// `context` (doc 07 §"Conflict Detection").
    pub fn conflicting_actions(&self, new_key: &str, context: ActionContext) -> Vec<&Action> {
        conflicting_actions(&self.registry, new_key, context)
    }

    /// Whether two contexts can hold a shortcut at the same time
    /// (doc 07 rules 1–4 collapsed into one predicate).
    pub fn can_co_fire(&self, a: ActionContext, b: ActionContext) -> bool {
        can_co_fire(&a, &b)
    }

    /// Load user-customized bindings from the `hotkeys` table
    /// (doc 07 §"Persistence").
    ///
    /// Only `is_default = 0` rows are read: the seeded
    /// `is_default = 1` rows are first-run data and never load back over
    /// the compiled-in defaults. Each row is applied to the action's
    /// *current* binding. Rows for unknown action ids are skipped, and
    /// so are non-empty key sequences GTK could not fire — an empty key
    /// sequence is not one of those: it is the marker an
    /// [`ConflictPolicy::Override`] leaves on the loser of a chord, and
    /// it must round-trip.
    pub fn load_from_db(
        &mut self,
        store: &SessionStore,
    ) -> Result<HotkeyLoadReport, rusqlite::Error> {
        let mut report = HotkeyLoadReport::default();
        for (action_id, key, enabled) in store.load_custom_hotkeys()? {
            if !key.is_empty() && !ui_key_is_bindable(&key) {
                report.skipped.push(action_id);
                continue;
            }
            match self
                .registry
                .apply_loaded_binding(&action_id, &key, enabled)
            {
                Ok(()) => report.applied.push(action_id),
                // Doc 07: unknown ids are skipped, not fatal.
                Err(RebindError::UnknownAction { .. }) => report.skipped.push(action_id),
                Err(_) => report.skipped.push(action_id),
            }
        }
        Ok(report)
    }

    /// Persist the current deviations from doc 07's table.
    ///
    /// One `is_default = 0` row per action that is rebound or disabled;
    /// the row of every action back at its default is deleted, so the
    /// compiled-in binding applies again on the next start. Returns the
    /// number of rows written.
    ///
    /// This is the write half of doc 06's `hotkeys` semantics ("user
    /// rebinding writes `is_default = 0` rows which override at load
    /// time"); the seeded `is_default = 1` rows are never touched, so
    /// the table keeps doc 06's row set intact.
    pub fn save_to_db(&self, store: &SessionStore) -> Result<usize, rusqlite::Error> {
        let mut written = 0;
        for action in self.registry.ordered_actions() {
            if action.is_default && action.enabled {
                store.delete_custom_hotkey(&action.id)?;
                continue;
            }
            store.save_custom_hotkey(
                &action.id,
                &action.current_key,
                action.context.as_str(),
                action.enabled,
            )?;
            written += 1;
        }
        Ok(written)
    }

    /// Serialize the deviating bindings as JSON (doc 07 §"Persistence",
    /// last paragraph: hotkey configs export independently of sessions).
    pub fn export_json(&self) -> serde_json::Result<String> {
        transfer::export_json(&self.registry)
    }

    /// Apply a JSON export, skipping entries whose action id this build
    /// does not know — the same skip-unknown semantics as
    /// [`Self::load_from_db`].
    pub fn import_json(
        &mut self,
        json: &str,
    ) -> Result<transfer::HotkeyImportReport, serde_json::Error> {
        transfer::import_json(&mut self.registry, json)
    }

    /// The GTK trigger for a captured key chord
    /// (`gdk::EventKey::key()` + `modifier_state()`), for the settings
    /// UI's "Capture Key" button.
    ///
    /// Requires GTK to be initialized; see [`KeyChord::to_trigger`].
    pub fn trigger_from_chord(chord: KeyChord) -> Option<gtk4::ShortcutTrigger> {
        chord.to_trigger()
    }
}

#[cfg(test)]
mod tests;
