//! The GTK4 user interface: the main window, tabs, dialogs, panels and the
//! Settings window (doc 01).

pub mod appearance;
pub mod auth_dialog;
pub mod folder_dialog;
pub mod host_picker;
pub mod hostkey_dialog;
pub mod icons;
pub mod macro_editor;
pub mod macros_panel;
pub mod main_window;
pub mod rdp_tab;
pub mod session_dialog;
pub mod session_ended;
pub mod session_tree;
pub mod settings;
pub mod sftp_sidebar;
pub mod side_dock;
pub mod status_bar;
pub mod tab;
pub mod tab_actions;
pub mod tab_bar;
pub mod tab_label;
pub mod tab_resilience;
pub mod tab_restore;
pub mod tab_state;
pub mod terminal_actions;
pub mod terminal_tab;
pub mod tray;
pub mod tunnels_dialog;
pub mod vnc_tab;
pub mod widgets;
pub use main_window::MainWindow;

/// Widget tests: they need an X display (`env -u WAYLAND_DISPLAY
/// GDK_BACKEND=x11 xvfb-run -a cargo test -- --ignored gtk_tests`). GTK binds to the first thread that initializes it, so every
/// widget check in the library runs from this one test.
#[cfg(test)]
mod gtk_tests {
    use gtk4::glib;

    /// Makes GLib warnings and criticals abort until dropped, then puts
    /// the previous mask back. The mask is process-wide and every widget
    /// check shares the process, so a check must not leave it set for
    /// the ones after it.
    pub(super) struct FatalWarnings(glib::ffi::GLogLevelFlags);

    impl FatalWarnings {
        pub(super) fn new() -> Self {
            // SAFETY: plain flag change on the process-wide GLib log mask.
            Self(unsafe {
                glib::ffi::g_log_set_always_fatal(
                    glib::ffi::G_LOG_LEVEL_CRITICAL | glib::ffi::G_LOG_LEVEL_WARNING,
                )
            })
        }
    }

    impl Drop for FatalWarnings {
        fn drop(&mut self) {
            // SAFETY: as in `new`, restoring the mask it replaced.
            unsafe {
                glib::ffi::g_log_set_always_fatal(self.0);
            }
        }
    }

    #[test]
    #[ignore = "needs a display; run under xvfb-run"]
    fn widgets() {
        gtk4::init().expect("gtk init");
        super::session_dialog::gtk_tests::tmux_group_entry_follows_name_until_user_edits();
        super::session_dialog::gtk_tests::jump_picker_and_forward_editor();
        super::tunnels_dialog::gtk_tests::rows_status_and_toggle();
        super::session_tree::gtk_tests::context_menu_is_reused_and_parented_cleanly();
        super::session_tree::gtk_tests::rows_nest_expand_and_select();
        super::session_tree::gtk_tests::moving_a_session_keeps_folders_open();
        super::session_tree::gtk_tests::moving_a_folder_refuses_cycles();
        super::sftp_sidebar::gtk_tests::listing_renders_and_selection_drives_toolbar();
        super::sftp_sidebar::gtk_tests::follow_checkbox_toggles_following();
        super::side_dock::gtk_tests::icon_tabs_switch_the_panel();
        super::side_dock::gtk_tests::sftp_panel_follows_the_current_tab();
        super::host_picker::gtk_tests::picker_opens_the_selected_host();
        super::macro_editor::gtk_tests::editor_rows_follow_the_draft();
        super::macros_panel::gtk_tests::panel_lists_macros_and_needs_a_terminal();
        super::macros_panel::gtk_tests::panel_records_and_plays_through_the_terminal();
        super::icons::gtk_tests::custom_svg_resolves_and_picker_round_trips();
        super::folder_dialog::gtk_tests::edit_folder_prefills_name_and_icon();
        super::settings::gtk_tests::pages_and_general_switches();
        super::tab::gtk_tests::focusing_a_tab_moves_focus_to_its_terminal();
        super::tab::gtk_tests::reconnect_does_not_steal_focus();
        super::tab::gtk_tests::closing_a_tab_focuses_the_next_terminal();
        super::tab::gtk_tests::sftp_sidebar_follows_the_shell_directory();
        super::tab::gtk_tests::a_local_shell_tab_connects_and_closes();
        super::tab::gtk_tests::terminals_follow_the_color_scheme_and_system_theme();
        super::appearance::gtk_tests::user_css_reloads_when_its_files_change();
        super::appearance::gtk_tests::the_chrome_follows_the_desktop_preference();
        super::vnc_tab::gtk_tests::vnc_view_draws_and_routes_input();
        super::rdp_tab::gtk_tests::rdp_embed_builds_and_tracks();
        super::rdp_tab::gtk_tests::rdp_child_follows_the_area_and_dies_with_the_page();
        super::tab_bar::gtk_tests::plus_follows_the_tabs_then_pins_when_they_scroll();
        super::main_window::gtk_tests::every_shortcut_has_an_action();
    }
}
