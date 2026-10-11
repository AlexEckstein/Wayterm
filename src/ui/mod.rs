//! GTK4 user interface (docs 01, 07, 09).

pub mod auth_dialog;
pub mod hostkey_dialog;
pub mod import_dialog;
pub mod main_window;
pub mod rdp_tab;
pub mod session_dialog;
pub mod session_ended;
pub mod session_tree;
pub mod settings;
pub mod sftp_sidebar;
pub mod status_bar;
pub mod tab;
pub mod tab_label;
pub mod tab_resilience;
pub mod tab_restore;
pub mod tab_state;
pub mod terminal_actions;
pub mod tray;
pub mod vnc_tab;
pub mod widgets;

pub use main_window::MainWindow;

/// Widget tests: they need a display (`xvfb-run -a cargo test -- --ignored
/// gtk_tests`). GTK binds to the first thread that initializes it, so every
/// widget check in the library runs from this one test.
#[cfg(test)]
mod gtk_tests {
    #[test]
    #[ignore = "needs a display; run under xvfb-run"]
    fn widgets() {
        gtk4::init().expect("gtk init");
        super::session_dialog::gtk_tests::tmux_group_entry_follows_name_until_user_edits();
        super::session_tree::gtk_tests::context_menu_is_reused_and_parented_cleanly();
        super::settings::gtk_tests::pages_and_general_switches();
        super::tab::gtk_tests::opening_a_tab_focuses_its_terminal();
        super::vnc_tab::gtk_tests::vnc_view_draws_and_routes_input();
        super::rdp_tab::gtk_tests::rdp_embed_builds_and_tracks();
        super::rdp_tab::gtk_tests::rdp_child_follows_the_area_and_dies_with_the_page();
    }
}
