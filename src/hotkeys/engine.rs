//! [`HotkeyEngine`]: the action registry plus its SQLite persistence
//! (doc 07 §HotkeyEngine, §Persistence), and [`HotkeyEngine::install`],
//! which binds the registry to GTK: Tier-1 application accels and one
//! Tier-2 shortcut controller per widget context. Once installed, every
//! mutation rebuilds only the bindings it changed.
//!
//! Persistence rules: the registry starts from the compiled-in defaults;
//! only `is_default = 0` rows are loaded, onto the *current* binding;
//! unknown action ids are skipped, so a database written by another build
//! still loads.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use gtk4::prelude::*;

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
}

/// How an action's chord reaches GTK (doc 07 §The Two-Tier Mechanism).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// An application accel on the window action: fires whatever has focus.
    Accel,
    /// A shortcut in its context's controller: fires only with focus there.
    Controller,
    /// RDP/VNC: nothing is bound, keys reach the remote session.
    Passthrough,
}

pub fn tier(context: ActionContext) -> Tier {
    match context {
        ActionContext::Global => Tier::Accel,
        context if context.is_widget_context() => Tier::Controller,
        _ => Tier::Passthrough,
    }
}

/// Every action is a window action (`win.<id>`), whichever tier binds it.
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

/// Where a Tier-2 context's controller lives.
///
/// The controller is local-scoped and runs in the capture phase: it sees
/// only key events aimed inside `widget`, and sees them before the focused
/// widget (VTE takes every key it is given). When `widget` holds more than
/// the context — the notebook holds RDP/VNC tabs and SFTP sidebars beside
/// the terminals — `in_context` narrows it; a chord pressed outside the
/// context propagates untouched.
#[derive(Clone)]
pub struct ContextHost {
    context: ActionContext,
    widget: gtk4::Widget,
    in_context: Rc<dyn Fn() -> bool>,
}

impl ContextHost {
    /// `widget` is exactly the context: its subtree having focus is enough.
    pub fn new(context: ActionContext, widget: &impl IsA<gtk4::Widget>) -> Self {
        Self {
            context,
            widget: widget.clone().upcast(),
            in_context: Rc::new(|| true),
        }
    }

    /// Fire only while `in_context` says focus is inside the context.
    pub fn with_focus_check(mut self, in_context: impl Fn() -> bool + 'static) -> Self {
        self.in_context = Rc::new(in_context);
        self
    }
}

impl fmt::Debug for ContextHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextHost")
            .field("context", &self.context)
            .field("widget", &self.widget)
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

/// Applies one action's current state to whatever is installed.
trait Binder: fmt::Debug {
    fn bind(&self, action: &Action);

    /// Detach whatever [`HotkeyEngine::install`] attached.
    fn uninstall(&self) {}
}

/// The GTK side: app accels for Tier 1, one shortcut per Tier-2 action
/// whose trigger is swapped in place on a rebind.
#[derive(Debug)]
struct GtkBinder {
    app: gtk4::Application,
    shortcuts: HashMap<String, gtk4::Shortcut>,
    controllers: Vec<(gtk4::Widget, gtk4::ShortcutController)>,
}

impl GtkBinder {
    fn new(app: &gtk4::Application, registry: &ActionRegistry, hosts: Vec<ContextHost>) -> Self {
        let mut shortcuts = HashMap::new();
        let mut controllers = Vec::new();
        for host in hosts {
            if tier(host.context) != Tier::Controller {
                continue;
            }
            let controller = gtk4::ShortcutController::new();
            controller.set_scope(gtk4::ShortcutScope::Local);
            controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let actions = registry.ordered_actions();
            for action in actions.into_iter().filter(|a| a.context == host.context) {
                let (name, in_context) = (action_name(&action.id), host.in_context.clone());
                let activate = gtk4::CallbackAction::new(move |widget, _| {
                    if in_context() && widget.activate_action(&name, None).is_ok() {
                        gtk4::glib::Propagation::Stop
                    } else {
                        gtk4::glib::Propagation::Proceed
                    }
                });
                let shortcut = gtk4::Shortcut::new(Some(gtk4::NeverTrigger::get()), Some(activate));
                controller.add_shortcut(shortcut.clone());
                shortcuts.insert(action.id.clone(), shortcut);
            }
            host.widget.add_controller(controller.clone());
            controllers.push((host.widget, controller));
        }
        Self {
            app: app.clone(),
            shortcuts,
            controllers,
        }
    }
}

impl Binder for GtkBinder {
    fn uninstall(&self) {
        for (widget, controller) in &self.controllers {
            widget.remove_controller(controller);
        }
    }

    fn bind(&self, action: &Action) {
        match tier(action.context) {
            Tier::Accel => {
                let accel = live_accel(action);
                self.app
                    .set_accels_for_action(&action_name(&action.id), accel.as_deref().as_slice());
            }
            Tier::Controller => {
                let Some(shortcut) = self.shortcuts.get(&action.id) else {
                    return; // no host was given for this context
                };
                match live_accel(action).and(parse_trigger(&action.current_key)) {
                    Some(trigger) => shortcut.set_trigger(Some(trigger)),
                    None => shortcut.set_trigger(Some(gtk4::NeverTrigger::get())),
                }
            }
            Tier::Passthrough => {}
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
    /// An engine over doc 07's default table.
    pub fn new() -> Self {
        Self::with_registry(defaults::default_registry())
    }

    pub fn with_registry(registry: ActionRegistry) -> Self {
        Self {
            registry,
            binder: None,
        }
    }

    /// Bind every action (doc 07 §The Two-Tier Mechanism): globals as
    /// `app` accels on `win.<id>`, widget contexts as shortcuts in one
    /// controller per `hosts` entry. A context without a host stays
    /// unbound. Installing again replaces the previous controllers.
    pub fn install(&mut self, app: &gtk4::Application, hosts: Vec<ContextHost>) {
        self.install_binder(Rc::new(GtkBinder::new(app, &self.registry, hosts)));
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
        self.rebuild(report.applied.iter().map(String::as_str));
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
