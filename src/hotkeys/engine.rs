//! [`HotkeyEngine`]: the action registry plus its SQLite persistence
//! (doc 07 §HotkeyEngine, §Persistence). Binding actions to GTK accels and
//! shortcut controllers is the UI's job.
//!
//! Persistence rules: the registry starts from the compiled-in defaults;
//! only `is_default = 0` rows are loaded, onto the *current* binding;
//! unknown action ids are skipped, so a database written by another build
//! still loads.

use crate::hotkeys::defaults;
use crate::hotkeys::registry::{Action, ActionRegistry, ConflictPolicy, RebindError};
use crate::hotkeys::transfer;
use crate::hotkeys::trigger::ui_key_is_bindable;
use crate::store::db::SessionStore;

/// What [`HotkeyEngine::load_from_db`] did with the stored rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotkeyLoadReport {
    /// Action ids rebound from the database, in load order.
    pub applied: Vec<String>,
    /// Rows not applied: unknown action ids, or keys GTK could never fire.
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HotkeyEngine {
    registry: ActionRegistry,
}

impl HotkeyEngine {
    /// An engine over doc 07's default table.
    pub fn new() -> Self {
        Self::with_registry(defaults::default_registry())
    }

    pub fn with_registry(registry: ActionRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &ActionRegistry {
        &self.registry
    }

    pub fn action(&self, id: &str) -> Option<&Action> {
        self.registry.action(id)
    }

    /// Rebind, refusing a chord a co-firing action already holds.
    pub fn rebind(&mut self, action_id: &str, new_key: &str) -> Result<Vec<String>, RebindError> {
        self.rebind_with(action_id, new_key, ConflictPolicy::Reject)
    }

    /// Rebind under `policy`; returns the ids an override displaced.
    pub fn rebind_with(
        &mut self,
        action_id: &str,
        new_key: &str,
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, RebindError> {
        self.registry.rebind_checked(action_id, new_key, policy)
    }

    pub fn reset_to_default(&mut self, action_id: &str) -> Result<(), RebindError> {
        self.registry.reset_to_default(action_id)
    }

    pub fn set_enabled(&mut self, action_id: &str, enabled: bool) -> Result<(), RebindError> {
        self.registry.set_enabled(action_id, enabled)
    }

    /// Actions that would conflict with `action_id` taking `new_key`.
    pub fn conflicts_for(&self, action_id: &str, new_key: &str) -> Vec<&Action> {
        self.registry.conflicts_for(action_id, new_key)
    }

    /// Apply the user's `is_default = 0` rows. An empty key is kept: it is
    /// how an override records the action it unbound.
    pub fn load_from_db(
        &mut self,
        store: &SessionStore,
    ) -> Result<HotkeyLoadReport, rusqlite::Error> {
        let mut report = HotkeyLoadReport::default();
        for (action_id, key, enabled) in store.load_custom_hotkeys()? {
            let usable = key.is_empty() || ui_key_is_bindable(&key);
            if usable
                && self
                    .registry
                    .apply_loaded_binding(&action_id, &key, enabled)
                    .is_ok()
            {
                report.applied.push(action_id);
            } else {
                report.skipped.push(action_id);
            }
        }
        Ok(report)
    }

    /// Persist one row per deviating action and delete the rows of actions
    /// back at their default. Returns the number of rows written.
    pub fn save_to_db(&self, store: &SessionStore) -> Result<usize, rusqlite::Error> {
        let mut written = 0;
        for action in self.registry.ordered_actions() {
            if action.is_default && action.enabled {
                store.delete_custom_hotkey(&action.id)?;
            } else {
                store.save_custom_hotkey(
                    &action.id,
                    &action.current_key,
                    action.context.as_str(),
                    action.enabled,
                )?;
                written += 1;
            }
        }
        Ok(written)
    }

    /// The deviating bindings as a shareable JSON layout.
    pub fn export_json(&self) -> serde_json::Result<String> {
        transfer::export_json(&self.registry)
    }

    /// Apply a JSON layout, skipping actions this build does not know.
    pub fn import_json(
        &mut self,
        json: &str,
    ) -> Result<transfer::HotkeyImportReport, serde_json::Error> {
        transfer::import_json(&mut self.registry, json)
    }
}

#[cfg(test)]
mod tests;
