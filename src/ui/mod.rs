//! GTK4 user-interface module (doc 01 / doc 10 Phase 1).

pub mod auth_dialog;
pub mod hostkey_dialog;
pub mod main_window;
pub mod session_dialog;
pub mod session_tree;
pub mod sftp_sidebar;
pub mod status_bar;
pub mod tab;

use sftp_sidebar::SftpSidebar;

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

/// No-op that exercises the auth/host-key dialog API so the P2-6 seam stays
/// compiled and lint-clean until P2-7 wires the dialogs into a real session
/// tab. Mirrors [`crate::transport::touch_public_api`]: the bin target
/// compiles this module tree privately, so an unwired `pub fn` would
/// otherwise read as dead code.
///
/// Nothing here opens a window — only function items and constants are
/// referenced, and the display-free helpers are called with inert inputs.
/// Removed once the connect flow calls the dialogs directly.
pub fn touch_dialog_api() {
    // Display-free challenge logic.
    let prompts = [auth_dialog::ChallengePrompt {
        prompt: String::from("Password: "),
        echo: false,
    }];
    for field in auth_dialog::field_specs(&prompts) {
        let _ = field.label.len();
        let _ = field.echo;
    }
    let _ = auth_dialog::window_title("touch");
    let _ = auth_dialog::instructions_label(Some(" "));
    let _ = auth_dialog::instructions_label(None);
    let _ = auth_dialog::responses_from_fields(&prompts, &[String::from("x")]);

    // Challenge dialog + its transport closure adapter.
    let _ = auth_dialog::run_challenges;
    let _ = auth_dialog::run_challenges_orphan;
    let _ = auth_dialog::top_level_parent;
    let _ = auth_dialog::run_on_gtk_main_thread(|| 0u8);
    let _: auth_dialog::ChallengeResponder =
        auth_dialog::challenge_responder_with(String::from("touch"), |_, _, _| None);
    let _: auth_dialog::ChallengeResponder = auth_dialog::challenge_responder("touch");

    // Host-key dialog + its verifier closure adapter.
    let _ = hostkey_dialog::UNKNOWN_BUTTONS;
    let _ = hostkey_dialog::CHANGED_BUTTONS;
    for notice in [
        hostkey_dialog::HostKeyNotice::Unknown,
        hostkey_dialog::HostKeyNotice::Changed,
    ] {
        let facts = crate::transport::hostkey::HostKeyFacts {
            host: String::from("h"),
            port: 22,
            key_type: String::from("t"),
            fingerprint_sha256: String::from("f"),
        };
        let _ = hostkey_dialog::notice_headline(&facts, notice);
        let _ = hostkey_dialog::notice_detail(&facts, notice);
    }
    let _ = hostkey_dialog::decision_from_response(true);
    for policy in [
        crate::store::model::HostKeyPolicy::Strict,
        crate::store::model::HostKeyPolicy::AcceptNew,
        crate::store::model::HostKeyPolicy::Off,
    ] {
        let _ = hostkey_dialog::notice_for_policy(policy);
    }
    let _ = hostkey_dialog::prompt_unknown;
    let _ = hostkey_dialog::prompt_changed;
    let _ = hostkey_dialog::host_key_prompt_with(|_, _| {
        crate::transport::hostkey::HostKeyDecision::Reject
    });
    let _ = hostkey_dialog::host_key_prompt();
}

/// No-op that exercises the SFTP sidebar API (doc 09) so the P3-2 seam stays
/// compiled and lint-clean until the connect flow owns an `SshConnection` to
/// hand [`SftpSidebar::attach`]. Same pattern as
/// [`crate::transport::touch_public_api`]: the bin target compiles this module
/// tree privately, so an unwired `pub fn` would otherwise read as dead code.
///
/// Function items only — constructing the widget needs a display, and this
/// runs before GTK is initialized.
pub fn touch_sftp_api() {
    let _ = SftpSidebar::new;
    let _ = SftpSidebar::attach;
    let _ = SftpSidebar::toggle_visible;
    let _ = SftpSidebar::widget;
}
