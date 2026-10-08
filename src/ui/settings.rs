//! Hotkey settings window — rebinding, capture, conflicts, reset
//! (doc 07 §"Settings UI", §"Conflict Detection", §"Persistence").
//!
//! Split, following `auth_dialog` / `session_dialog`:
//!
//! * a **display-free data layer** ([`SettingsRow`], [`SettingsView`],
//!   [`SettingsError`]) which the integration tests in
//!   `tests/settings_ui.rs` exercise without a display;
//! * a **thin GTK layer** ([`create_hotkey_settings_window`],
//!   [`show_hotkey_settings`]) that renders the rows and drives the
//!   capture / override / reset flows through the data layer.
//!
//! ## ListView-vs-ListBox choice
//!
//! Doc 07 sketches the list as `GtkListView` / `GtkColumnView`. That
//! pairing needs a `ListStore` + `SingleSelection` + per-column factory
//! assembly for what is a fixed 37-row table with two buttons per row.
//! A [`gtk4::ListBox`] with one row widget per action renders the same
//! four columns (display name | current key | context | reset button,
//! plus the doc's "Capture Key" button) with far less machinery, stays
//! keyboard-navigable, and keeps every behaviour the doc mandates. The
//! column content is identical — only the container widget differs.
//!
//! ## Capture path
//!
//! The "Capture Key" button opens a small modal window carrying a
//! [`gtk4::EventControllerKey`]. The next key press is read as doc 07
//! mandates — `key()` + `modifier_state()` — into a [`KeyChord`], which
//! renders the UI-format string both the list and the database store
//! ([`KeyChord::to_ui_key`]); strings are never re-parsed on this path.
//! `Escape` cancels capture, and a bare modifier press is ignored so the
//! dialog keeps waiting for the real chord. While the modal capture
//! dialog has focus no other widget can receive the key press, so the
//! captured chord cannot fire any action on its way in.
//!
//! ## Conflict + persistence rules (doc 07 verbatim behaviour)
//!
//! * [`SettingsView::conflicts`] wraps `HotkeyEngine::conflicts_for`;
//!   a non-empty loser set raises an "… is already bound to 'X'.
//!   Override?" confirm built by [`SettingsView::override_prompt`].
//! * Confirm rebinds with [`ConflictPolicy::Override`], which unbinds
//!   (clears) the loser; decline changes nothing.
//! * Reset rebinds `current_key` back to `default_key`, flipping
//!   `is_default` — never by mutating `default_key`.
//! * Every successful rebind or reset is saved through
//!   `HotkeyEngine::save_to_db`, so bindings survive a restart; loading
//!   happens once at startup in `app::build_ui` via `load_from_db`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use thiserror::Error;

use crate::hotkeys::{Action, ActionContext, ConflictPolicy, HotkeyEngine, KeyChord, RebindError};
use crate::store::SessionStore;

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// What a rebind-or-reset through the settings UI can fail with.
#[derive(Debug, Error)]
pub enum SettingsError {
    /// The engine refused the binding (unknown action, unparsable key,
    /// or a conflict under the `Reject` policy).
    #[error(transparent)]
    Rebind(#[from] RebindError),
    /// The rebind applied but persisting it to SQLite failed.
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// One row of the settings list: the list-columns projection of an
/// [`Action`] (doc 07 §"Settings UI": action name | key sequence |
/// context | reset-to-default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsRow {
    /// Registry id (`"new_tab"`).
    pub id: String,
    /// Human-readable name (`"New Tab"`).
    pub display_name: String,
    /// Effective binding in UI format (`"Ctrl+Shift+T"`).
    pub current_key: String,
    /// Compiled-in binding in UI format.
    pub default_key: String,
    /// Which tier binds the action.
    pub context: ActionContext,
    /// Rendered context for the list's context column.
    pub context_label: String,
    /// Whether `current_key` still equals the compiled-in binding.
    pub is_default: bool,
    /// Whether the action currently holds a chord.
    pub enabled: bool,
}

impl SettingsRow {
    /// Project one registry action into its list row.
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

/// Rendered context for the settings list's context column.
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

/// The settings list's logic: row mapping, capture formatting, conflict
/// wiring, and the rebind/reset-then-save flows.
///
/// Unit struct — every method takes the engine (or chord) it works on,
/// so the GTK layer stays a thin renderer over these.
pub struct SettingsView;

impl SettingsView {
    /// Every registered action as a list row, in doc 07 table order.
    pub fn rows(engine: &HotkeyEngine) -> Vec<SettingsRow> {
        Self::rows_from_registry(engine.registry())
    }

    /// Every action of a registry as a list row, in doc 07 table order.
    pub fn rows_from_registry(registry: &crate::hotkeys::ActionRegistry) -> Vec<SettingsRow> {
        registry
            .ordered_actions()
            .into_iter()
            .map(SettingsRow::from_action)
            .collect()
    }

    /// Render a captured chord as the UI-format string the list and the
    /// database store (`KeyChord::to_ui_key` — the doc's capture path).
    ///
    /// `None` when GDK has no canonical name for the key.
    pub fn capture_label(chord: KeyChord) -> Option<String> {
        chord.to_ui_key()
    }

    /// Whether a captured key press cancels capture (`Escape`).
    ///
    /// Pure keyval comparison — no display needed.
    pub fn is_cancel_key(key: gdk4::Key) -> bool {
        key == gdk4::Key::Escape
    }

    /// Whether a captured key press is a bare modifier (or lock key)
    /// rather than a real chord: the capture dialog ignores these and
    /// keeps waiting for the chord.
    ///
    /// Pure keyval comparison — no display needed.
    pub fn is_modifier_only(key: gdk4::Key) -> bool {
        matches!(
            key,
            gdk4::Key::Shift_L
                | gdk4::Key::Shift_R
                | gdk4::Key::Control_L
                | gdk4::Key::Control_R
                | gdk4::Key::Alt_L
                | gdk4::Key::Alt_R
                | gdk4::Key::Super_L
                | gdk4::Key::Super_R
                | gdk4::Key::Meta_L
                | gdk4::Key::Meta_R
                | gdk4::Key::Hyper_L
                | gdk4::Key::Hyper_R
                | gdk4::Key::Caps_Lock
                | gdk4::Key::Num_Lock
        )
    }

    /// The actions (other than `action_id` itself) that already hold
    /// `new_key` in a context that can fire alongside it — the losers an
    /// "Override?" confirm would clear (doc 07 §"Conflict Detection").
    pub fn conflicts(engine: &HotkeyEngine, action_id: &str, new_key: &str) -> Vec<SettingsRow> {
        engine
            .conflicts_for(action_id, new_key)
            .into_iter()
            .map(SettingsRow::from_action)
            .collect()
    }

    /// The override confirm's message (doc 07: "`Ctrl+Shift+T` is already
    /// bound to 'New Tab'. Override?").
    ///
    /// Multiple losers are named comma-separated; with no losers there is
    /// nothing to confirm and the message reports the chord is free.
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

    /// Rebind `action_id` to `new_key` under `policy`, then persist the
    /// surviving deviations via the engine's save path so the binding
    /// survives a restart.
    ///
    /// Returns the displaced action ids (meaningful under
    /// [`ConflictPolicy::Override`]). Nothing is persisted unless the
    /// rebind itself succeeded.
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

    /// Restore `action_id`'s compiled-in binding (per-row "Reset to
    /// Default": `current_key` back to `default_key`, `is_default` true)
    /// and persist, so the reset survives a restart.
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

/// Shared settings state handed to every GTK closure on the main thread.
struct SettingsState {
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
}

/// Build the dedicated hotkey settings window (doc 07 §"Settings UI").
///
/// A standalone [`gtk4::Window`] — not a notebook page — listing every
/// registered action as display name | current key | context | Capture +
/// Reset buttons. The caller presents the returned window, e.g. via
/// [`show_hotkey_settings`].
pub fn create_hotkey_settings_window(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
) -> gtk4::Window {
    let state = Rc::new(SettingsState { engine, store });

    let window = gtk4::Window::builder()
        .title("Hotkey Settings")
        .default_width(640)
        .default_height(560)
        .modal(false)
        .build();

    let header = gtk4::HeaderBar::new();
    header.set_title_widget(Some(&gtk4::Label::new(Some("Hotkey Settings"))));
    let close_button = gtk4::Button::with_label("Close");
    {
        let window = window.clone();
        close_button.connect_clicked(move |_| window.close());
    }
    header.pack_end(&close_button);
    window.set_titlebar(Some(&header));

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
    hint.set_margin_top(6);
    hint.set_margin_bottom(6);
    hint.set_margin_start(12);
    hint.set_margin_end(12);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&hint);
    root.append(&scrolled);
    window.set_child(Some(&root));

    window
}

/// Build, parent, and present the hotkey settings window.
pub fn show_hotkey_settings(
    engine: Rc<RefCell<HotkeyEngine>>,
    store: Rc<SessionStore>,
    parent: Option<&gtk4::Window>,
) {
    let window = create_hotkey_settings_window(engine, store);
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }
    window.present();
}

/// Rebuild every list row from the engine's current registry state.
fn refresh_list(list_box: &gtk4::ListBox, state: &Rc<SettingsState>) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    for row in SettingsView::rows(&state.engine.borrow()) {
        list_box.append(&build_row(row, list_box, state));
    }
}

/// One action's row: display name | current key | context | Capture +
/// Reset buttons.
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

/// Open the modal "press the new shortcut" capture dialog for one action.
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
    prompt.set_margin_top(18);
    prompt.set_margin_bottom(18);
    prompt.set_margin_start(18);
    prompt.set_margin_end(18);
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

/// Apply one captured chord: rebind directly when free, otherwise raise
/// the "… is already bound to 'X'. Override?" confirm first.
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

    open_override_confirm(action_id, new_key, losers, list_box, state);
}

/// Modal "… is already bound to 'X'. Override?" confirm.
///
/// Confirm rebinds with [`ConflictPolicy::Override`] — clearing (unbinding)
/// the loser per doc 07 — then persists; decline changes nothing.
fn open_override_confirm(
    action_id: &str,
    new_key: &str,
    losers: Vec<SettingsRow>,
    list_box: &gtk4::ListBox,
    state: &Rc<SettingsState>,
) {
    let message = SettingsView::override_prompt(new_key, &losers);
    let dialog = gtk4::Window::builder()
        .title("Shortcut Conflict")
        .default_width(400)
        .default_height(140)
        .modal(true)
        .build();
    if let Some(parent) = list_box
        .root()
        .and_then(|root| root.downcast::<gtk4::Window>().ok())
    {
        dialog.set_transient_for(Some(&parent));
    }

    let label = gtk4::Label::new(Some(&message));
    label.set_wrap(true);
    label.set_margin_top(18);
    label.set_margin_start(18);
    label.set_margin_end(18);

    let override_button = gtk4::Button::with_label("Override");
    override_button.add_css_class("destructive-action");
    let cancel_button = gtk4::Button::with_label("Cancel");

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    buttons.set_halign(gtk4::Align::End);
    buttons.set_margin_top(12);
    buttons.set_margin_bottom(18);
    buttons.set_margin_start(18);
    buttons.set_margin_end(18);
    buttons.append(&cancel_button);
    buttons.append(&override_button);

    {
        let dialog = dialog.clone();
        let action_id = action_id.to_string();
        let new_key = new_key.to_string();
        let list_box = list_box.clone();
        let state = Rc::clone(state);
        override_button.connect_clicked(move |_| {
            match SettingsView::apply_rebind(
                &mut state.engine.borrow_mut(),
                &state.store,
                &action_id,
                &new_key,
                ConflictPolicy::Override,
            ) {
                Ok(_) => refresh_list(&list_box, &state),
                Err(err) => {
                    eprintln!("Wayterm: failed to override hotkey {action_id} to {new_key}: {err}");
                }
            }
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&label);
    root.append(&buttons);
    dialog.set_child(Some(&root));
    dialog.present();
}
