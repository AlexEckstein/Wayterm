//! Hotkey config export/import as JSON (doc 07 §"Persistence", last
//! paragraph: "hotkey configs export to JSON independently of sessions,
//! for sharing keyboard layouts between machines").
//!
//! Only *deviations* from doc 07's compiled-in table are exported: an
//! action still at its default needs no row, and leaving them out means
//! an import onto a newer build keeps that build's defaults instead of
//! pinning the old ones. The `is_default` flag travels with every
//! entry anyway, so the document is self-describing and an entry that
//! later *does* name the default key round-trips to "no deviation".
//!
//! Import applies the doc's load semantics to the file it is given:
//! an entry for an action this build does not know is skipped and
//! reported, never an error — a shared layout may have been written by a
//! build with more actions.
//!
//! No credential material is involved: bindings are chord strings, not
//! connection parameters (doc 06 §"Credential Storage").

use serde::{Deserialize, Serialize};

use crate::hotkeys::registry::{ActionContext, ActionRegistry, ConflictPolicy};

/// Schema version of the export document.
pub const HOTKEY_EXPORT_VERSION: u32 = 1;

/// A hotkey export document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyExport {
    pub version: u32,
    /// Actions that deviate from the compiled-in defaults, in doc 07
    /// table order.
    pub bindings: Vec<ExportedBinding>,
}

/// One exported binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedBinding {
    /// Action id (doc 07 §"Action Registry": `"new_tab"`).
    pub action: String,
    /// Human-readable name at export time, for review before import.
    pub display_name: String,
    /// UI-format key (`"Ctrl+Shift+T"`), or `""` for an action an
    /// override unbound.
    pub key: String,
    /// The action's context, as the `hotkeys.context` wire string.
    pub context: String,
    pub enabled: bool,
    pub is_default: bool,
}

/// What [`import_json`] did with an export document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotkeyImportReport {
    /// Action ids whose binding the import changed.
    pub applied: Vec<String>,
    /// Action ids this build does not know — skipped (doc 07's
    /// skip-unknown rule).
    pub skipped: Vec<String>,
    /// Actions the import unbound because it overrode their chord.
    pub displaced: Vec<String>,
}

/// Serialize the registry's deviating bindings.
///
/// ```text
/// {"version":1,"bindings":[{"action":"copy","display_name":"Copy Selection",
///  "key":"Ctrl+Shift+C","context":"terminal","enabled":true,"is_default":false}]}
/// ```
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

/// Apply an export document to `registry`.
///
/// The document is authoritative: an incoming chord overrides whatever
/// holds it now (doc 07's "Override?" answered once for the whole
/// file), and the losers are unbound. Entries for unknown action ids are
/// skipped and reported. An entry whose key no longer binds (a default
/// that changed under it) is skipped too, rather than stored dead.
///
/// A `version` other than [`HOTKEY_EXPORT_VERSION`] is still applied:
/// unknown *fields* are ignored by serde and unknown *action ids* are
/// skipped, so a forward-compatible document degrades to a partial
/// import instead of failing outright.
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
            // The document moves the action to a context this build
            // gives a different tier; rebinding across tiers would
            // create conflicts the file never resolved.
            report.skipped.push(binding.action);
            continue;
        }
        if binding.key.is_empty() {
            // An unbound action: apply the unbind directly, since there
            // is no chord to check for conflicts.
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
