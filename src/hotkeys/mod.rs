//! Hotkey engine (doc 07) — data model and logic, display-free.
//!
//! This module tree is everything doc 07 specifies *except* the GTK
//! binding layer: the [`ActionRegistry`] data model, trigger parsing and
//! key capture, the conflict rules, doc 07's default table, and the doc
//! 06 `hotkeys` persistence path. Layering, bottom-up:
//!
//! | Module | Doc 07 section | Role |
//! |---|---|---|
//! | [`trigger`] | §"Trigger Parsing" | UI format ⇄ accelerator ⇄ `gtk4::ShortcutTrigger`; `gdk::Key` capture |
//! | [`registry`] | §"Action Registry", §"Conflict Detection" | the action data model, rebinding, the four conflict rules |
//! | [`defaults`] | §"Default Actions" | the compiled-in default table |
//! | [`engine`] | §"HotkeyEngine", §"Persistence" | registry + SQLite reconciliation |
//! | [`transfer`] | §"Persistence" (last paragraph) | JSON export/import of a layout |
//!
//! The GTK half — `HotkeyEngine::install`, the Tier-1 accels on the
//! `GtkApplication`, the per-context `ShortcutController`s, the settings
//! page and its "Capture Key" button — is the UI task's, and lands with
//! it; the seam is marked on [`engine::HotkeyEngine`]. Nothing in this
//! tree opens a window or opens a display connection, so all of it is
//! testable headless: the only functions that touch GTK are
//! [`parse_trigger`] and [`KeyChord::to_trigger`], which the tests
//! deliberately leave to the UI layer.
//!
//! The `#[allow(dead_code)]` below mirrors `store/mod.rs`: the bin
//! target compiles this tree privately, so until the UI task wires it in
//! its `pub fn`s would read as dead code there. Drop them with the
//! module declarations in `main.rs` once it does.

#[allow(dead_code)] // bin-target seam until the UI task wires the GTK layer
pub mod defaults;
#[allow(dead_code)]
pub mod engine;
#[allow(dead_code)]
pub mod registry;
#[allow(dead_code)]
pub mod transfer;
#[allow(dead_code)]
pub mod trigger;

// Mirrors store/mod.rs: the bin target compiles this tree privately, so
// the re-exports it does not use yet would be warnings there.
#[allow(unused_imports)]
pub use defaults::{default_registry, register_defaults, DefaultAction, SeedRow, DEFAULT_ACTIONS};
#[allow(unused_imports)]
pub use engine::{HotkeyEngine, HotkeyLoadReport};
#[allow(unused_imports)]
pub use registry::{
    can_co_fire, conflicting_actions, Action, ActionContext, ActionRegistry, ConflictPolicy,
    RebindError,
};
#[allow(unused_imports)]
pub use transfer::{export_json, import_json, ExportedBinding, HotkeyExport, HotkeyImportReport};
#[allow(unused_imports)]
pub use trigger::{
    accel_to_ui_key, key_token, key_token_is_bindable, parse_trigger, ui_key_is_bindable,
    ui_key_to_accel, KeyChord,
};
