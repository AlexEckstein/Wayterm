//! Hotkey layouts as JSON, for sharing between machines (doc 07
//! §Persistence). Only deviations from the defaults are exported, so an
//! import onto a newer build keeps that build's defaults; entries for
//! unknown actions are skipped and reported, never an error.

use serde::{Deserialize, Serialize};

use crate::hotkeys::registry::{ActionContext, ActionRegistry, ConflictPolicy};

/// Export document schema version.
pub const HOTKEY_EXPORT_VERSION: u32 = 1;

/// A hotkey export document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyExport {
    pub version: u32,
    /// Deviating actions, in table order.
    pub bindings: Vec<ExportedBinding>,
}

/// One exported binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedBinding {
    pub action: String,
    /// For review before import.
    pub display_name: String,
    /// UI format; `""` for an action an override unbound.
    pub key: String,
    /// The `hotkeys.context` wire string.
    pub context: String,
    pub enabled: bool,
    pub is_default: bool,
}

/// What [`import_json`] did with an export document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotkeyImportReport {
    pub applied: Vec<String>,
    /// Unknown actions, wrong contexts, or keys that no longer bind.
    pub skipped: Vec<String>,
    /// Actions unbound because an imported chord overrode theirs.
    pub displaced: Vec<String>,
}

/// Serialize the registry's deviating bindings.
pub fn export_json(registry: &ActionRegistry) -> serde_json::Result<String> {
    let bindings = registry
        .custom_actions()
        .into_iter()
        .map(|action| ExportedBinding {
            action: action.id.clone(),
            display_name: action.display_name.clone(),
            key: action.current_key.clone(),
            context: action.context.as_str().to_string(),
            enabled: action.enabled,
            is_default: action.is_default,
        })
        .collect();
    serde_json::to_string_pretty(&HotkeyExport {
        version: HOTKEY_EXPORT_VERSION,
        bindings,
    })
}

/// Apply an export document. Its chords override whatever holds them now
/// (the losers are unbound). Any `version` is accepted: unknown fields and
/// actions degrade to a partial import.
pub fn import_json(
    registry: &mut ActionRegistry,
    json: &str,
) -> Result<HotkeyImportReport, serde_json::Error> {
    let export: HotkeyExport = serde_json::from_str(json)?;
    let mut report = HotkeyImportReport::default();
    for binding in export.bindings {
        if !registry.contains(&binding.action) {
            report.skipped.push(binding.action);
            continue;
        }
        let Some(context) = ActionContext::from_db_str(&binding.context) else {
            report.skipped.push(binding.action);
            continue;
        };
        if Some(context) != registry.context_of(&binding.action) {
            // A different tier here: its conflicts were never resolved.
            report.skipped.push(binding.action);
            continue;
        }
        if binding.key.is_empty() {
            // An unbind has no chord to check.
            if let Some(action) = registry.action_mut(&binding.action) {
                action.current_key.clear();
                action.enabled = binding.enabled;
                action.is_default = false;
            }
            report.applied.push(binding.action);
            continue;
        }
        match registry.rebind_checked(&binding.action, &binding.key, ConflictPolicy::Override) {
            Ok(displaced) => {
                report.displaced.extend(displaced);
                if !binding.enabled {
                    let _ = registry.set_enabled(&binding.action, false);
                }
                report.applied.push(binding.action);
            }
            Err(_) => report.skipped.push(binding.action),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
