//! Hotkey engine (doc 11): the action registry, trigger parsing, the
//! conflict rules, the default table, and SQLite/JSON persistence. All of
//! it is display-free except [`parse_trigger`], which hands accelerator
//! strings to GTK.

pub mod defaults;
pub mod engine;
pub mod registry;
pub mod transfer;
pub mod trigger;

pub use defaults::{DEFAULT_ACTIONS, DefaultAction, default_registry};
pub use engine::{HotkeyEngine, HotkeyLoadReport};
pub use registry::{
    Action, ActionContext, ActionRegistry, ConflictPolicy, RebindError, can_co_fire,
    conflicting_actions,
};
pub use transfer::{ExportedBinding, HotkeyExport, HotkeyImportReport};
pub use trigger::{KeyChord, accel_to_ui_key, parse_trigger, ui_key_is_bindable, ui_key_to_accel};
