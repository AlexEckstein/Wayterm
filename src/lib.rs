//! Wayterm: a multi-protocol connection manager and terminal for Linux.
//! The design docs in `docs/` are the source of truth; each module names
//! the doc it implements.
//!
//! - [`ui`]: the GTK window, tabs, dialogs and session tree.
//! - [`terminal`]: the `TerminalBackend` seam and its VTE and Ghostty backends.
//! - [`transport`]: byte-stream transports (SSH, local PTY, serial), SSH
//!   auth, host keys, jump hosts, port forwarding and reconnects.
//! - [`sftp`], [`vnc`], [`rdp`]: the protocol engines behind their tabs.
//! - [`store`]: the SQLite session store; [`transfer`]: import and export.
//! - [`hotkeys`]: the action registry and hotkey engine.
//! - [`macros`]: recording, editing data and playback of keystroke macros.
//! - [`app`]: application startup and data paths; [`backpressure`]: the
//!   GTK-side queue for bounded channels (doc 02 §Backpressure).

pub mod app;
pub mod backpressure;
pub mod hotkeys;
pub mod macros;
pub mod rdp;
pub mod sftp;
pub mod store;
pub mod terminal;
pub mod transfer;
pub mod transport;
pub mod ui;
pub mod vnc;
