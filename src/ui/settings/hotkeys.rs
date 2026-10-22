//! The Keyboard Shortcuts settings page (doc 11 §Settings page): every action
//! with its binding, a Capture button that records the next chord, and Reset.
//!
//! [`SettingsView`] is the display-free logic (rows, capture formatting,
//! conflicts, rebind-then-save); the GTK functions only render it. A
//! captured chord that is taken raises "… is already bound to 'X'.
//! Override?"; overriding unbinds the loser. Every change is saved at once.
//! A `ListBox` stands in for doc 11's `ColumnView`: same columns, far less
//! machinery for a fixed 37-row table.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use thiserror::Error;

use crate::hotkeys::{Action, ActionContext, ConflictPolicy, HotkeyEngine, KeyChord, RebindError};
use crate::store::SessionStore;
use crate::ui::widgets::{confirm, set_margins};

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// Why a rebind or reset through the UI failed.
#[derive(Debug, Error)]
pub enum SettingsError {
    /// The registry refused the change.
    #[error(transparent)]
    Rebind(#[from] RebindError),
    /// Saving failed.
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// One list row: name | key | context, plus reset state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsRow {
    /// Action id.
    pub id: String,
    /// Action name.
    pub display_name: String,
    /// Current chord; empty when unbound.
    pub current_key: String,
    /// Default chord.
    pub default_key: String,
    /// Where it fires.
    pub context: ActionContext,
    /// The context's display name.
    pub context_label: String,
    /// Whether the action is at its default.
    pub is_default: bool,
    /// Whether the action is enabled.
    pub enabled: bool,
}

impl SettingsRow {
    /// The row for `action`.
    pub fn from_action(action: &Action) -> Self {
        Self {
            id: action.id.clone(),
            display_name: action.display_name.clone(),
            current_key: action.current_key.clone(),
            default_key: action.default_key.clone(),
            context: action.context,
            context_label: context_label(action.context).to_string(),
            is_default: action.is_default,
            enabled: action.enabled,
        }
    }
}

/// The context column's text.
pub fn context_label(context: ActionContext) -> &'static str {
    match context {
        ActionContext::Global => "Global",
        ActionContext::Terminal => "Terminal",
        ActionContext::SessionTree => "Session Tree",
        ActionContext::Sftp => "SFTP",
        ActionContext::RdpTab => "RDP Tab",
        ActionContext::VncTab => "VNC Tab",
    }
}

/// The settings logic; every function takes what it works on.
pub struct SettingsView;

impl SettingsView {
    /// Every action as a row, in table order.
    pub fn rows(engine: &HotkeyEngine) -> Vec<SettingsRow> {
        Self::rows_from_registry(engine.registry())
    }

    /// Every action's row, in table order.
    pub fn rows_from_registry(registry: &crate::hotkeys::ActionRegistry) -> Vec<SettingsRow> {
        registry
            .ordered_actions()
            .into_iter()
            .map(SettingsRow::from_action)
            .collect()
    }

    /// A captured chord in UI format; `None` if GDK cannot name the key.
    pub fn capture_label(chord: KeyChord) -> Option<String> {
        chord.to_ui_key()
    }

    /// Escape cancels capture.
    pub fn is_cancel_key(key: gdk::Key) -> bool {
        key == gdk::Key::Escape
    }

    /// A bare modifier or lock key: capture keeps waiting for the chord.
    pub fn is_modifier_only(key: gdk::Key) -> bool {
        matches!(
            key,
            gdk::Key::Shift_L
                | gdk::Key::Shift_R
                | gdk::Key::Control_L
                | gdk::Key::Control_R
                | gdk::Key::Alt_L
                | gdk::Key::Alt_R
                | gdk::Key::Super_L
                | gdk::Key::Super_R
                | gdk::Key::Meta_L
                | gdk::Key::Meta_R
                | gdk::Key::Hyper_L
                | gdk::Key::Hyper_R
                | gdk::Key::Caps_Lock
                | gdk::Key::Num_Lock
        )
    }

    /// The actions an override of `new_key` would unbind.
    pub fn conflicts(engine: &HotkeyEngine, action_id: &str, new_key: &str) -> Vec<SettingsRow> {
        engine
            .conflicts_for(action_id, new_key)
            .into_iter()
            .map(SettingsRow::from_action)
            .collect()
    }

    /// "Ctrl+Shift+T is already bound to 'New Tab'. Override?"
    pub fn override_prompt(new_key: &str, losers: &[SettingsRow]) -> String {
        if losers.is_empty() {
            return format!("{new_key} is not bound to any other action.");
        }
        let names: Vec<String> = losers
            .iter()
            .map(|loser| format!("'{}'", loser.display_name))
            .collect();
        format!(
            "{new_key} is already bound to {}. Override?",
            names.join(", ")
        )
    }

    /// Rebind under `policy` and save; returns the displaced ids. Nothing
    /// is saved if the rebind fails.
    pub fn apply_rebind(
        engine: &mut HotkeyEngine,
        store: &SessionStore,
        action_id: &str,
        new_key: &str,
        policy: ConflictPolicy,
    ) -> Result<Vec<String>, SettingsError> {
        let displaced = engine.rebind_with(action_id, new_key, policy)?;
        engine.save_to_db(store)?;
        Ok(displaced)
    }

    /// Reset to the compiled-in binding and save.
    pub fn reset_action(
        engine: &mut HotkeyEngine,
        store: &SessionStore,
        action_id: &str,
    ) -> Result<(), SettingsError> {
        engine.reset_to_default(action_id)?;
        engine.save_to_db(store)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// What every settings callback needs.
struct SettingsState {
    /// The live engine every change goes through.
    engine: Rc<RefCell<HotkeyEngine>>,
    /// Where changes are saved.
    store: Rc<SessionStore>,
}

/// The page widget: a hint above the scrolling action list.
pub fn page(engine: Rc<RefCell<HotkeyEngine>>, store: Rc<SessionStore>) -> gtk4::Widget {
    let state = Rc::new(SettingsState { engine, store });

    let list_box = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    refresh_list(&list_box, &state);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list_box)
        .vexpand(true)
        .hexpand(true)
        .build();
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

    let hint = gtk4::Label::new(Some(
        "Choose \"Capture\" and press the new shortcut. Esc cancels. \
         Rebinding a shortcut already in use asks before overriding it.",
    ));
    hint.set_wrap(true);
    set_margins(&hint, 6);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&hint);
    root.append(&scrolled);
    root.upcast()
}

/// Rebuild the rows from the engine.
fn refresh_list(list_box: &gtk4::ListBox, state: &Rc<SettingsState>) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    for row in SettingsView::rows(&state.engine.borrow()) {
        list_box.append(&build_row(row, list_box, state));
    }
}

/// name | key | context | Capture | Reset.
fn build_row(
    row: SettingsRow,
    list_box: &gtk4::ListBox,
    state: &Rc<SettingsState>,
) -> gtk4::ListBoxRow {
    let name = gtk4::Label::new(Some(&row.display_name));
    name.set_xalign(0.0);
    name.set_hexpand(true);

    let key_text = if row.current_key.is_empty() {
        "(unbound)".to_string()
    } else {
        row.current_key.clone()
    };
    let key = gtk4::Label::new(Some(&key_text));
    key.set_xalign(0.0);
    key.set_width_chars(18);

    let context = gtk4::Label::new(Some(&row.context_label));
    context.set_xalign(0.0);
    context.set_width_chars(12);

    let capture_button = gtk4::Button::with_label("Capture");
    {
        let action_id = row.id.clone();
        let display_name = row.display_name.clone();
        let list_box = list_box.clone();
        let state = Rc::clone(state);
        capture_button.connect_clicked(move |_| {
            open_capture_dialog(&action_id, &display_name, &list_box, &state);
        });
    }

    let reset_button = gtk4::Button::with_label("Reset");
    reset_button.set_sensitive(!row.is_default);
    reset_button.set_tooltip_text(Some("Reset to default"));
    {
        let action_id = row.id.clone();
        let list_box = list_box.clone();
        let state = Rc::clone(state);
        reset_button.connect_clicked(move |_| {
            if let Err(err) =
                SettingsView::reset_action(&mut state.engine.borrow_mut(), &state.store, &action_id)
            {
                eprintln!("Wayterm: failed to reset hotkey {action_id}: {err}");
                return;
            }
            refresh_list(&list_box, &state);
        });
    }

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    hbox.set_margin_top(4);
    hbox.set_margin_bottom(4);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);
    hbox.append(&name);
    hbox.append(&key);
    hbox.append(&context);
    hbox.append(&capture_button);
    hbox.append(&reset_button);

    gtk4::ListBoxRow::builder().child(&hbox).build()
}

/// The modal "press the new shortcut" window.
fn open_capture_dialog(
    action_id: &str,
    display_name: &str,
    list_box: &gtk4::ListBox,
    state: &Rc<SettingsState>,
) {
    let dialog = gtk4::Window::builder()
        .title("Capture Key")
        .default_width(360)
        .default_height(120)
        .modal(true)
        .build();
    if let Some(parent) = list_box
        .root()
        .and_then(|root| root.downcast::<gtk4::Window>().ok())
    {
        dialog.set_transient_for(Some(&parent));
    }

    let prompt = gtk4::Label::new(Some(&format!(
        "Press the new shortcut for '{display_name}'.\nEsc cancels."
    )));
    set_margins(&prompt, 18);
    dialog.set_child(Some(&prompt));

    let key_controller = gtk4::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let dialog = dialog.clone();
        let action_id = action_id.to_string();
        let list_box = list_box.clone();
        let state = Rc::clone(state);
        key_controller.connect_key_pressed(move |_controller, key, _keycode, modifier_state| {
            if SettingsView::is_cancel_key(key) {
                dialog.close();
                return glib::Propagation::Stop;
            }
            if SettingsView::is_modifier_only(key) {
                // Still waiting for the non-modifier half of the chord.
                return glib::Propagation::Stop;
            }
            let chord = KeyChord::new(key, modifier_state);
            let Some(new_key) = SettingsView::capture_label(chord) else {
                eprintln!("Wayterm: captured a key GDK cannot name; ignoring");
                return glib::Propagation::Stop;
            };
            dialog.close();
            handle_captured_key(&action_id, &new_key, &list_box, &state);
            glib::Propagation::Stop
        });
    }
    dialog.add_controller(key_controller);
    dialog.present();
}

/// Rebind to a captured chord, asking first if it is taken.
fn handle_captured_key(
    action_id: &str,
    new_key: &str,
    list_box: &gtk4::ListBox,
    state: &Rc<SettingsState>,
) {
    let losers = SettingsView::conflicts(&state.engine.borrow(), action_id, new_key);
    if losers.is_empty() {
        if let Err(err) = SettingsView::apply_rebind(
            &mut state.engine.borrow_mut(),
            &state.store,
            action_id,
            new_key,
            ConflictPolicy::Reject,
        ) {
            eprintln!("Wayterm: failed to rebind hotkey {action_id} to {new_key}: {err}");
            return;
        }
        refresh_list(list_box, state);
        return;
    }

    let (action_id, new_key) = (action_id.to_string(), new_key.to_string());
    let (list_box, state) = (list_box.clone(), Rc::clone(state));
    let parent = list_box
        .root()
        .and_then(|root| root.downcast::<gtk4::Window>().ok());
    confirm(
        parent.as_ref(),
        &SettingsView::override_prompt(&new_key, &losers),
        "The other action will be left without a shortcut.",
        "Override",
        move || match SettingsView::apply_rebind(
            &mut state.engine.borrow_mut(),
            &state.store,
            &action_id,
            &new_key,
            ConflictPolicy::Override,
        ) {
            Ok(_) => refresh_list(&list_box, &state),
            Err(err) => {
                eprintln!("Wayterm: failed to override hotkey {action_id} to {new_key}: {err}")
            }
        },
    );
}
