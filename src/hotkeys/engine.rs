//! [`HotkeyEngine`]: the action registry plus its SQLite persistence
//! (doc 11 §HotkeyEngine, §Persistence), and [`HotkeyEngine::install`],
//! which binds the registry to the main window: one capture-phase shortcut
//! controller whose shortcuts check their action's context at each key
//! press (doc 11 §Dispatch). Once installed, every mutation swaps only the
//! triggers it changed.
//!
//! Persistence rules: the registry starts from the compiled-in defaults;
//! only `is_default = 0` rows are loaded, onto the *current* binding and
//! through the same conflict check as an override; unknown action ids are
//! skipped, so a database written by another build still loads.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};

use crate::hotkeys::defaults;
use crate::hotkeys::registry::{
    Action, ActionContext, ActionRegistry, ConflictPolicy, RebindError,
};
use crate::hotkeys::transfer;
use crate::hotkeys::trigger::{parse_trigger, ui_key_is_bindable, ui_key_to_accel};
use crate::store::db::SessionStore;

/// What [`HotkeyEngine::load_from_db`] did with the stored rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotkeyLoadReport {
    /// Action ids rebound from the database, in load order.
    pub applied: Vec<String>,
    /// Rows not applied: unknown action ids, or keys GTK could never fire.
    pub skipped: Vec<String>,
    /// Actions unbound because a loaded chord collides with theirs under
    /// today's conflict rules.
    pub displaced: Vec<String>,
}

/// The dispatcher's controller name, so it can be told from GTK's own.
pub const DISPATCHER_NAME: &str = "wayterm-shortcuts";

/// Every action is a window action, `win.<id>`.
pub fn action_name(id: &str) -> String {
    format!("win.{id}")
}

/// The accelerator `action` should hold right now; `None` while it is
/// disabled or unbound, which leaves its chord free.
pub fn live_accel(action: &Action) -> Option<String> {
    if action.is_bound() {
        ui_key_to_accel(&action.current_key)
    } else {
        None
    }
}

/// The order the controller tries a chord's shortcuts in: the narrower
/// context first. Conflict checks keep co-firing pairs apart, so this only
/// decides a layout that slipped past them.
pub fn precedence(context: ActionContext) -> u8 {
    match context {
        ActionContext::SessionTree | ActionContext::Sftp => 0,
        ActionContext::Terminal => 1,
        ActionContext::Global => 2,
        ActionContext::RdpTab | ActionContext::VncTab => 3,
    }
}

/// When one context's shortcuts may fire (doc 11 §Dispatch), checked at
/// each key press. The main window gives one per context it hosts; global
/// shortcuts need none, and a context without one stays unbound.
#[derive(Clone)]
pub struct ContextHost {
    context: ActionContext,
    active: Rc<dyn Fn() -> bool>,
}

impl ContextHost {
    /// Active while `active` says so.
    pub fn new(context: ActionContext, active: impl Fn() -> bool + 'static) -> Self {
        Self {
            context,
            active: Rc::new(active),
        }
    }

    /// Active while focus is inside `widget`, but not where the focus owns
    /// its keys ([`focus_owns_keys`]).
    pub fn focus_within(context: ActionContext, widget: &impl IsA<gtk4::Widget>) -> Self {
        let widget = widget.clone().upcast::<gtk4::Widget>();
        Self::new(context, move || {
            focus_within(&widget) && !focus_owns_keys(&widget)
        })
    }

    /// Whether the context is active right now.
    pub fn is_active(&self) -> bool {
        (self.active)()
    }
}

impl fmt::Debug for ContextHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextHost")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

/// Whether the focus of `widget`'s window is `widget` or inside it.
pub fn focus_within(widget: &impl IsA<gtk4::Widget>) -> bool {
    widget
        .root()
        .and_then(|root| root.focus())
        .is_some_and(|focus| focus == *widget.as_ref() || focus.is_ancestor(widget))
}

/// Whether the focus of `widget`'s window is somewhere that owns its
/// keys: a text field (an entry, the search popover, a text view) or a
/// popover (the session tree's menu answers `Return` itself). Context
/// shortcuts stand aside there, so they never steal typing.
pub fn focus_owns_keys(widget: &impl IsA<gtk4::Widget>) -> bool {
    let mut current = widget.root().and_then(|root| root.focus());
    while let Some(widget) = current {
        if widget.is::<gtk4::Editable>()
            || widget.is::<gtk4::TextView>()
            || widget.is::<gtk4::Popover>()
        {
            return true;
        }
        current = widget.parent();
    }
    false
}

/// Activate the window action `id` if it is registered and enabled;
/// `false` — let the key through — otherwise.
fn fire(window: &gtk4::Widget, id: &str) -> bool {
    let Some(actions) = window.dynamic_cast_ref::<gio::ActionGroup>() else {
        return false;
    };
    if !actions.has_action(id) || !actions.is_action_enabled(id) {
        return false;
    }
    actions.activate_action(id, None);
    true
}

/// Applies one action's current state to whatever is installed.
trait Binder: fmt::Debug {
    fn bind(&self, action: &Action);

    /// Detach whatever [`HotkeyEngine::install`] attached.
    fn uninstall(&self) {}
}

/// The GTK side: one capture-phase controller on the window, one shortcut
/// per action of a hosted context, its trigger swapped in place on a
/// rebind.
#[derive(Debug)]
struct GtkBinder {
    window: gtk4::Widget,
    controller: gtk4::ShortcutController,
    shortcuts: HashMap<String, gtk4::Shortcut>,
}

impl GtkBinder {
    fn new(
        window: &gtk4::ApplicationWindow,
        registry: &ActionRegistry,
        hosts: Vec<ContextHost>,
    ) -> Self {
        let mut active: HashMap<ActionContext, Rc<dyn Fn() -> bool>> = hosts
            .into_iter()
            .map(|host| (host.context, host.active))
            .collect();
        active.insert(ActionContext::Global, Rc::new(|| true));

        // Local scope on the window: every key aimed anywhere inside it.
        let controller = gtk4::ShortcutController::new();
        controller.set_name(Some(DISPATCHER_NAME));
        controller.set_scope(gtk4::ShortcutScope::Local);
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let mut actions = registry.ordered_actions();
        actions.sort_by_key(|action| precedence(action.context));
        let mut shortcuts = HashMap::new();
        for action in actions {
            let Some(active) = active.get(&action.context).cloned() else {
                continue; // no host was given for this context
            };
            let id = action.id.clone();
            let activate = gtk4::CallbackAction::new(move |window, _| {
                if active() && fire(window, &id) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            let shortcut = gtk4::Shortcut::new(Some(gtk4::NeverTrigger::get()), Some(activate));
            controller.add_shortcut(shortcut.clone());
            shortcuts.insert(action.id.clone(), shortcut);
        }
        window.add_controller(controller.clone());
        Self {
            window: window.clone().upcast(),
            controller,
            shortcuts,
        }
    }
}

impl Binder for GtkBinder {
    fn uninstall(&self) {
        self.window.remove_controller(&self.controller);
    }

    fn bind(&self, action: &Action) {
        let Some(shortcut) = self.shortcuts.get(&action.id) else {
            return;
        };
        match live_accel(action).and(parse_trigger(&action.current_key)) {
            Some(trigger) => shortcut.set_trigger(Some(trigger)),
            None => shortcut.set_trigger(Some(gtk4::NeverTrigger::get())),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HotkeyEngine {
    registry: ActionRegistry,
    /// Set by [`Self::install`]; every mutation pushes through it.
    binder: Option<Rc<dyn Binder>>,
}

impl HotkeyEngine {
    /// An engine over doc 11's default table.
    pub fn new() -> Self {
        Self::with_registry(defaults::default_registry())
    }

    pub fn with_registry(registry: ActionRegistry) -> Self {
        Self {
            registry,
            binder: None,
        }
    }

    /// Bind every action to `window` (doc 11 §Dispatch): one shortcut per
    /// action whose context is global or in `hosts`, each firing the window
    /// action `win.<id>`. Installing again replaces the previous
    /// controller. Returns the ids with no window action registered yet —
    /// their chords would pass through untouched.
    pub fn install(
        &mut self,
        window: &gtk4::ApplicationWindow,
        hosts: Vec<ContextHost>,
    ) -> Vec<String> {
        self.install_binder(Rc::new(GtkBinder::new(window, &self.registry, hosts)));
        self.registry
            .ordered_actions()
            .into_iter()
            .filter(|action| !window.has_action(&action.id))
            .map(|action| action.id.clone())
            .collect()
    }

    fn install_binder(&mut self, binder: Rc<dyn Binder>) {
        if let Some(previous) = self.binder.take() {
            previous.uninstall();
        }
        for action in self.registry.actions() {
            binder.bind(action);
        }
        self.binder = Some(binder);
    }

    /// Push the current state of `ids` to the installed bindings.
    fn rebuild<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        let Some(binder) = &self.binder else { return };
        for id in ids {
            if let Some(action) = self.registry.action(id) {
                binder.bind(action);
            }
        }
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
        let displaced = self.registry.rebind_checked(action_id, new_key, policy)?;
        self.rebuild(std::iter::once(action_id).chain(displaced.iter().map(String::as_str)));
        Ok(displaced)
    }

    pub fn reset_to_default(&mut self, action_id: &str) -> Result<(), RebindError> {
        self.registry.reset_to_default(action_id)?;
        self.rebuild([action_id]);
        Ok(())
    }

    pub fn set_enabled(&mut self, action_id: &str, enabled: bool) -> Result<(), RebindError> {
        self.registry.set_enabled(action_id, enabled)?;
        self.rebuild([action_id]);
        Ok(())
    }

    /// Actions that would conflict with `action_id` taking `new_key`.
    pub fn conflicts_for(&self, action_id: &str, new_key: &str) -> Vec<&Action> {
        self.registry.conflicts_for(action_id, new_key)
    }

    /// Apply the user's `is_default = 0` rows, oldest first. A bound row
    /// goes through the override check, so a chord that collides under
    /// today's conflict rules takes the chord and unbinds the holder. A
    /// disabled row holds no chord and applies as is; an empty key is kept
    /// too: it is how an override records the action it unbound.
    pub fn load_from_db(
        &mut self,
        store: &SessionStore,
    ) -> Result<HotkeyLoadReport, rusqlite::Error> {
        let mut report = HotkeyLoadReport::default();
        for (action_id, key, enabled) in store.load_custom_hotkeys()? {
            let applied = if key.is_empty() || !enabled {
                (key.is_empty() || ui_key_is_bindable(&key))
                    && self
                        .registry
                        .apply_loaded_binding(&action_id, &key, enabled)
                        .is_ok()
            } else {
                match self
                    .registry
                    .rebind_checked(&action_id, &key, ConflictPolicy::Override)
                {
                    Ok(displaced) => {
                        report.displaced.extend(displaced);
                        true
                    }
                    Err(_) => false,
                }
            };
            if applied {
                report.applied.push(action_id);
            } else {
                report.skipped.push(action_id);
            }
        }
        self.rebuild(
            report
                .applied
                .iter()
                .chain(&report.displaced)
                .map(String::as_str),
        );
        // A displaced action may be one a later row rebound again.
        report.displaced.retain(|id| {
            self.registry
                .action(id)
                .is_some_and(|action| !action.is_bound())
        });
        report.displaced.sort();
        report.displaced.dedup();
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
        let report = transfer::import_json(&mut self.registry, json)?;
        // An imported chord may displace actions the report does not name.
        let ids: Vec<String> = self.registry.actions().map(|a| a.id.clone()).collect();
        self.rebuild(ids.iter().map(String::as_str));
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
