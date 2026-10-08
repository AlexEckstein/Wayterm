//! GTK4 user interface (docs 01, 07, 09).

pub mod auth_dialog;
pub mod hostkey_dialog;
pub mod main_window;
pub mod session_dialog;
pub mod session_tree;
pub mod settings;
pub mod sftp_sidebar;
pub mod status_bar;
pub mod tab;
pub mod tab_label;
pub mod tab_resilience;
pub mod tab_restore;
pub mod tab_state;
pub mod tray;

pub use main_window::MainWindow;
