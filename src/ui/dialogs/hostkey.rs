//! Host-key dialogs (doc 04 §Host key verification): the OpenSSH-style
//! prompt for an unknown host, and the MITM warning for a changed key,
//! which can deny the connection or trust the new key for this session
//! only, never in `~/.ssh/known_hosts`.

use std::sync::Arc;

use gtk4::prelude::*;

pub use crate::transport::hostkey::HostKeyQuestion;
use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts, HostKeyPrompt};
use crate::ui::dialogs::auth::{run_on_gtk_main_thread, top_level_parent};
use crate::ui::widgets::{Modal, button_row, set_margins, wrapped_label};

/// Unknown-host buttons: cancel first, then the affirmative one, which is
/// the default.
pub const UNKNOWN_BUTTONS: [&str; 2] = ["Reject", "Accept"];
/// Changed-key buttons: Deny first, and the default, so Enter never trusts
/// a changed key.
pub const CHANGED_BUTTONS: [&str; 2] = ["Deny", "Accept and add to this session"];

pub fn notice_headline(facts: &HostKeyFacts, question: HostKeyQuestion) -> String {
    let host = &facts.host;
    match question {
        HostKeyQuestion::Unknown => {
            format!("The authenticity of host '{host}' can't be established.")
        }
        HostKeyQuestion::Changed => format!("The host key for '{host}' has changed."),
    }
}

/// Key type, SHA-256 fingerprint and host:port, then the question (unknown)
/// or the warning, what accepting does, and the `ssh-keygen -R` remedy
/// (changed).
pub fn notice_detail(facts: &HostKeyFacts, question: HostKeyQuestion) -> String {
    let HostKeyFacts {
        host,
        port,
        key_type,
        fingerprint_sha256,
    } = facts;
    let facts_line =
        format!("{key_type} key fingerprint is {fingerprint_sha256} for {host}:{port}.");
    match question {
        HostKeyQuestion::Unknown => {
            format!("{facts_line}\nAre you sure you want to continue connecting?")
        }
        HostKeyQuestion::Changed => format!(
            "{facts_line}\n\
             This is the man-in-the-middle signal: either the host was rebuilt \
             or reinstalled, or something is intercepting your connection.\n\
             Compare the fingerprint above with the one your administrator \
             gives you, out of band, before you accept it.\n\
             Accepting remembers the new key for this session bookmark only; \
             the global ~/.ssh/known_hosts is not touched. If its entry for \
             this host is stale, remove that entry with:\n\
             \x20   ssh-keygen -R '{}'",
            known_hosts_name(host, *port)
        ),
    }
}

/// How `ssh-keygen -R` names the host: `host`, or `[host]:port` off
/// port 22, as known_hosts does.
fn known_hosts_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
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

/// The decision for the user's answer to `question`: `Some(true)` is its
/// accepting button, anything else (the other button, a closed window)
/// rejects. An unknown host's key is recorded in known_hosts, a changed
/// one only in the session's own set.
pub fn decision_for(question: HostKeyQuestion, answer: Option<bool>) -> HostKeyDecision {
    match (question, answer) {
        (HostKeyQuestion::Unknown, Some(accepted)) => decision_from_response(accepted),
        (HostKeyQuestion::Changed, Some(true)) => HostKeyDecision::AcceptAndSaveSession,
        (_, Some(false) | None) => HostKeyDecision::Reject,
    }
}

/// Ask `question` about `facts` through `show`, which returns the user's
/// answer (injectable for tests; the app shows the dialog).
pub fn ask_with(
    question: HostKeyQuestion,
    facts: &HostKeyFacts,
    show: impl FnOnce(&HostKeyFacts, HostKeyQuestion) -> Option<bool>,
) -> HostKeyDecision {
    decision_for(question, show(facts, question))
}

/// Prompt about an unknown host (strict policy).
pub fn prompt_unknown(facts: &HostKeyFacts) -> HostKeyDecision {
    ask_with(HostKeyQuestion::Unknown, facts, show)
}

/// The changed-key warning: Deny rejects, "Accept and add to this session"
/// trusts the key for this session only.
pub fn prompt_changed(facts: &HostKeyFacts) -> HostKeyDecision {
    ask_with(HostKeyQuestion::Changed, facts, show)
}

/// Show a notice; `Some(true)` only if the user pressed its accepting
/// button, `Some(false)` for the other one, `None` if the window closed.
fn show(facts: &HostKeyFacts, question: HostKeyQuestion) -> Option<bool> {
    let modal = Modal::new("Host key verification", top_level_parent().as_ref());
    modal.window.set_default_width(520);
    modal.window.set_resizable(false);

    let (icon, body_class, buttons) = match question {
        HostKeyQuestion::Unknown => ("dialog-question-symbolic", "dim-label", UNKNOWN_BUTTONS),
        HostKeyQuestion::Changed => ("dialog-warning-symbolic", "error", CHANGED_BUTTONS),
    };
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    header.append(&gtk4::Image::from_icon_name(icon));
    header.append(&wrapped_label(&notice_headline(facts, question)));
    let body = wrapped_label(&notice_detail(facts, question));
    body.set_selectable(true);
    body.add_css_class(body_class);

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&vbox, 12);
    vbox.append(&header);
    vbox.append(&body);
    let mut row = Vec::new();
    for (index, label) in buttons.iter().enumerate() {
        let accepted = index == 1;
        let button = modal.button(label, move || Some(accepted));
        let is_default = match question {
            HostKeyQuestion::Unknown => accepted,
            HostKeyQuestion::Changed => !accepted,
        };
        if is_default {
            modal.window.set_default_widget(Some(&button));
        }
        if accepted && question == HostKeyQuestion::Changed {
            button.add_css_class("destructive-action");
        }
        row.push(button);
    }
    vbox.append(&button_row(&row.iter().collect::<Vec<_>>()));
    modal.window.set_child(Some(&vbox));
    modal.run()
}

/// A [`HostKeyPrompt`] that runs `handler` on the GTK main thread (the
/// handler is injectable for tests); a gone main loop rejects.
pub fn host_key_prompt_with<H>(handler: H) -> HostKeyPrompt
where
    H: Fn(HostKeyQuestion, &HostKeyFacts) -> HostKeyDecision + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |question, facts: &HostKeyFacts| {
        let (handler, facts) = (handler.clone(), facts.clone());
        run_on_gtk_main_thread(move || handler(question, &facts)).unwrap_or(HostKeyDecision::Reject)
    })
}

/// The GTK-backed prompt for [`HostKeyVerifier`]: the unknown-host dialog
/// or the changed-key warning, whichever the verifier asks.
///
/// [`HostKeyVerifier`]: crate::transport::hostkey::HostKeyVerifier
pub fn host_key_prompt() -> HostKeyPrompt {
    host_key_prompt_with(|question, facts| match question {
        HostKeyQuestion::Unknown => prompt_unknown(facts),
        HostKeyQuestion::Changed => prompt_changed(facts),
    })
}

#[cfg(test)]
mod tests;
