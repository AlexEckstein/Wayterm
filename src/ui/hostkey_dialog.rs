//! Host-key dialogs (doc 04 §Host key verification): the OpenSSH-style
//! prompt for an unknown host, and the MITM warning for a changed key,
//! which has no accept button at all. The connect flow only shows the
//! first: a changed key is rejected without the warning (doc 04
//! §Limitations).

use std::sync::Arc;

use gtk4::prelude::*;

use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyPrompt};
use crate::ui::auth_dialog::{run_on_gtk_main_thread, top_level_parent};
use crate::ui::widgets::{button_row, set_margins, wrapped_label, Modal};

/// Unknown-host buttons: cancel first, then the affirmative one.
pub const UNKNOWN_BUTTONS: [&str; 2] = ["Reject", "Accept"];
/// A changed key can never be accepted with one click.
pub const CHANGED_BUTTONS: [&str; 1] = ["Reject"];

/// Which host-key dialog to show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyNotice {
    /// A host not in known_hosts (Accept / Cancel).
    Unknown,
    /// A key that differs from known_hosts (no accept).
    Changed,
}

/// The dialog's headline for `notice`.
pub fn notice_headline(facts: &HostKeyFacts, notice: HostKeyNotice) -> String {
    let host = &facts.host;
    match notice {
        HostKeyNotice::Unknown => {
            format!("The authenticity of host '{host}' can't be established.")
        }
        HostKeyNotice::Changed => format!("The host key for '{host}' has changed."),
    }
}

/// Key type, SHA-256 fingerprint and host:port, then the question (unknown)
/// or the warning and the `ssh-keygen -R` remedy (changed).
pub fn notice_detail(facts: &HostKeyFacts, notice: HostKeyNotice) -> String {
    let HostKeyFacts {
        host,
        port,
        key_type,
        fingerprint_sha256,
    } = facts;
    let facts_line =
        format!("{key_type} key fingerprint is {fingerprint_sha256} for {host}:{port}.");
    match notice {
        HostKeyNotice::Unknown => {
            format!("{facts_line}\nAre you sure you want to continue connecting?")
        }
        HostKeyNotice::Changed => format!(
            "{facts_line}\n\
             This is the man-in-the-middle signal: either the host was rebuilt \
             or reinstalled without clearing its entry, or something is \
             intercepting your connection.\n\
             Compare the fingerprint above with the one your administrator \
             gives you, out of band. If the key really did change, remove the \
             stale entry and reconnect:\n\
             \x20   ssh-keygen -R '{host}:{port}'\n\
             Wayterm will not let you connect to a changed key."
        ),
    }
}

/// Accepting always records the key: a bare `Accept` would leave the host
/// unknown and re-prompt on every connect.
pub fn decision_from_response(accepted: bool) -> HostKeyDecision {
    if accepted {
        HostKeyDecision::AcceptAndSave
    } else {
        HostKeyDecision::Reject
    }
}

/// Prompt about an unknown host (strict policy).
pub fn prompt_unknown(facts: &HostKeyFacts) -> HostKeyDecision {
    decision_from_response(show(facts, HostKeyNotice::Unknown) == Some(true))
}

/// The changed-key warning; always rejects. Not called by the connect
/// flow.
pub fn prompt_changed(facts: &HostKeyFacts) -> HostKeyDecision {
    show(facts, HostKeyNotice::Changed);
    HostKeyDecision::Reject
}

/// Show a notice; `Some(true)` only if the user pressed Accept.
fn show(facts: &HostKeyFacts, notice: HostKeyNotice) -> Option<bool> {
    let modal = Modal::new("Host key verification", top_level_parent().as_ref());
    modal.window.set_default_width(520);
    modal.window.set_resizable(false);

    let (icon, body_class, buttons): (_, _, &[&str]) = match notice {
        HostKeyNotice::Unknown => ("dialog-question-symbolic", "dim-label", &UNKNOWN_BUTTONS),
        HostKeyNotice::Changed => ("dialog-warning-symbolic", "error", &CHANGED_BUTTONS),
    };
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    header.append(&gtk4::Image::from_icon_name(icon));
    header.append(&wrapped_label(&notice_headline(facts, notice)));
    let body = wrapped_label(&notice_detail(facts, notice));
    body.set_selectable(true);
    body.add_css_class(body_class);

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&vbox, 12);
    vbox.append(&header);
    vbox.append(&body);
    // The changed-key notice has no buttons: closing the window is the
    // only (rejecting) answer.
    if buttons.len() > 1 {
        let mut row = Vec::new();
        for (index, label) in buttons.iter().enumerate() {
            let accepted = index == 1;
            let button = modal.button(label, move || Some(accepted));
            if accepted {
                modal.window.set_default_widget(Some(&button));
            }
            row.push(button);
        }
        vbox.append(&button_row(&row.iter().collect::<Vec<_>>()));
    }
    modal.window.set_child(Some(&vbox));
    modal.run()
}

/// A [`HostKeyPrompt`] that runs `handler` on the GTK main thread (the
/// handler is injectable for tests); a gone main loop rejects.
pub fn host_key_prompt_with<H>(handler: H) -> HostKeyPrompt
where
    H: Fn(HostKeyNotice, &HostKeyFacts) -> HostKeyDecision + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |facts: &HostKeyFacts| {
        let (handler, facts) = (handler.clone(), facts.clone());
        run_on_gtk_main_thread(move || handler(HostKeyNotice::Unknown, &facts))
            .unwrap_or(HostKeyDecision::Reject)
    })
}

/// The GTK-backed prompt for [`HostKeyVerifier`]. The verifier only asks
/// about unknown hosts; a changed key is rejected without prompting.
///
/// [`HostKeyVerifier`]: crate::transport::hostkey::HostKeyVerifier
pub fn host_key_prompt() -> HostKeyPrompt {
    host_key_prompt_with(|_, facts| prompt_unknown(facts))
}

#[cfg(test)]
mod tests;
