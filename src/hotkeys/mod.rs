//! Hotkey engine (doc 07): the action registry, trigger parsing, the
//! conflict rules, the default table, and SQLite/JSON persistence. All of
//! it is display-free except [`parse_trigger`], which hands accelerator
//! strings to GTK.

pub mod defaults;
pub mod engine;
pub mod registry;
pub mod transfer;
pub mod trigger;

pub use defaults::{default_registry, DefaultAction, DEFAULT_ACTIONS};
pub use engine::{HotkeyEngine, HotkeyLoadReport};
pub use registry::{
    can_co_fire, conflicting_actions, Action, ActionContext, ActionRegistry, ConflictPolicy,
    RebindError,
};
pub use transfer::{ExportedBinding, HotkeyExport, HotkeyImportReport};
pub use trigger::{accel_to_ui_key, parse_trigger, ui_key_is_bindable, ui_key_to_accel, KeyChord};
