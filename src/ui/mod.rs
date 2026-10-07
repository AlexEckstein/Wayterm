//! GTK4 user-interface module (doc 01 / doc 10 Phase 1).

pub mod main_window;
pub mod session_dialog;
pub mod session_tree;
pub mod status_bar;
pub mod tab;

pub use main_window::MainWindow;
#[cfg(test)]
#[allow(unused_imports)]
pub use session_dialog::config_from_fields;
#[allow(unused_imports)]
pub use session_dialog::SessionDialog;
pub use session_tree::SessionTree;
pub use status_bar::StatusBar;
#[allow(unused_imports)]
pub use tab::SessionTab;
