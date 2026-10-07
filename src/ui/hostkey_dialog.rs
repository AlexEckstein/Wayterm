//! Host-key dialogs per doc 11 §Host Key Verification.
//!
//! Two dialogs, both driven by [`HostKeyFacts`]:
//!
//! - [`prompt_unknown`] — the OpenSSH-style "authenticity of host can't be
//!   established" prompt for an unknown host under the `strict` policy. The
//!   only affirmative answer is `AcceptAndSave`, which is what makes
//!   `HostKeyVerifier::verify` record the key.
//! - [`prompt_changed`] — the MITM signal for a *changed* key. Doc 11 requires
//!   "a scary dialog … offer 'view fingerprint' and instructions for removing
//!   the old entry, but never a one-click override", so this dialog has a
//!   single button and always resolves to [`HostKeyDecision::Reject`].
//!
//! Both are split into a display-free text/decision layer (unit-tested in
//! [`tests`]) and a thin GTK layer that blocks on nested
//! [`glib::MainContext`] iteration.

use std::rc::Rc;
use std::sync::Arc;

use gtk4::glib;
use gtk4::glib::Propagation;
use gtk4::prelude::*;

use crate::store::model::HostKeyPolicy;
use crate::transport::hostkey::{HostKeyDecision, HostKeyFacts};

use crate::ui::auth_dialog::run_on_gtk_main_thread;

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// Button labels for the unknown-host prompt, in dialog order.
///
/// Index 0 is the cancel button, index 1 the affirmative one.
pub const UNKNOWN_BUTTONS: [&str; 2] = ["Reject", "Accept"];

/// The only button on the changed-key dialog.
///
/// A one-click override is forbidden by doc 11, so there is nothing else to
/// offer: the fingerprint and the `ssh-keygen -R` hint are in the text.
pub const CHANGED_BUTTONS: [&str; 1] = ["Reject"];

/// Which body copy to show, per doc 11 §Host Key Verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyNotice {
    Unknown,
    Changed,
}

/// Headline for the dialog.
///
/// OpenSSH's wording for an unknown host; doc 11's MITM warning for a changed
/// key.
pub fn notice_headline(facts: &HostKeyFacts, notice: HostKeyNotice) -> String {
    match notice {
        HostKeyNotice::Unknown => format!(
            "The authenticity of host '{host}' can't be established.",
            host = facts.host
        ),
        HostKeyNotice::Changed => {
            format!("The host key for '{host}' has changed.", host = facts.host)
        }
    }
}

/// Body copy listing the key type, the SHA-256 fingerprint and the host:port,
/// followed by the policy-specific question.
pub fn notice_detail(facts: &HostKeyFacts, notice: HostKeyNotice) -> String {
    let facts_line = format!(
        "{key_type} key fingerprint is {fingerprint} for {host}:{port}.",
        key_type = facts.key_type,
        fingerprint = facts.fingerprint_sha256,
        host = facts.host,
        port = facts.port
    );

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
             Wayterm will not let you connect to a changed key.",
            host = facts.host,
            port = facts.port
        ),
    }
}

/// Map the dialog outcome to a decision.
///
/// Acceptance means "record the key": `Accept` would bypass
/// `learn_known_hosts_path` in [`crate::transport::hostkey::HostKeyVerifier`]
/// and leave the host unknown, so a strict-policy connection would re-prompt
/// on every connect. Refusal is always [`HostKeyDecision::Reject`].
pub fn decision_from_response(accepted: bool) -> HostKeyDecision {
    if accepted {
        HostKeyDecision::AcceptAndSave
    } else {
        HostKeyDecision::Reject
    }
}

/// Which notice a [`HostKeyPolicy`] shows for an unverifiable host.
///
/// `Strict` is the only policy that prompts; the other two are handled
/// inside `HostKeyVerifier::verify` without any UI.
pub fn notice_for_policy(policy: HostKeyPolicy) -> Option<HostKeyNotice> {
    match policy {
        HostKeyPolicy::Strict => Some(HostKeyNotice::Unknown),
        HostKeyPolicy::AcceptNew | HostKeyPolicy::Off => None,
    }
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// Prompt for an unknown host (strict policy).
///
/// Returns [`HostKeyDecision::AcceptAndSave`] if the user accepts, otherwise
/// [`HostKeyDecision::Reject`]. Never returns bare `Accept` — see
/// [`decision_from_response`].
pub fn prompt_unknown(facts: &HostKeyFacts) -> HostKeyDecision {
    let Some(decision) = show(facts, HostKeyNotice::Unknown) else {
        return HostKeyDecision::Reject;
    };
    decision_from_response(decision)
}

/// Prompt for a host whose key changed.
///
/// Doc 11 forbids a one-click override, so the only action closes the dialog
/// with [`HostKeyDecision::Reject`]. It exists as its own function (rather than
/// a flag on [`prompt_unknown`]) so the MITM path can never grow an affirmative
/// button, and so the wiring is explicit when P2-7 surfaces the MITM dialog.
pub fn prompt_changed(facts: &HostKeyFacts) -> HostKeyDecision {
    let _decision = show(facts, HostKeyNotice::Changed);
    HostKeyDecision::Reject
}

/// Show the notice and return whether the user accepted. `None` when the main
/// loop is gone — the caller must treat that as a refusal.
fn show(facts: &HostKeyFacts, notice: HostKeyNotice) -> Option<bool> {
    let headline = notice_headline(facts, notice);
    let detail = notice_detail(facts, notice);
    let buttons: &[&str] = match notice {
        HostKeyNotice::Unknown => &UNKNOWN_BUTTONS,
        HostKeyNotice::Changed => &CHANGED_BUTTONS,
    };

    let result: Rc<std::cell::RefCell<Option<bool>>> = Rc::new(std::cell::RefCell::new(None));
    let done = Rc::new(std::cell::RefCell::new(false));

    let window = gtk4::ApplicationWindow::builder()
        .title("Host key verification")
        .modal(true)
        .default_width(520)
        .resizable(false)
        .build();

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    let (icon_name, heading_class) = match notice {
        HostKeyNotice::Unknown => ("dialog-question-symbolic", "dim-label"),
        HostKeyNotice::Changed => ("dialog-warning-symbolic", "error"),
    };
    let icon = gtk4::Image::from_icon_name(icon_name);
    let heading = gtk4::Label::builder()
        .label(&headline)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .build();
    let body = gtk4::Label::builder()
        .label(&detail)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .selectable(true)
        .xalign(0.0)
        .build();
    body.add_css_class(heading_class);

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    header.append(&icon);
    header.append(&heading);
    vbox.append(&header);
    vbox.append(&body);

    let mut buttons_box: Option<gtk4::Box> = None;
    if buttons.len() > 1 {
        let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        box_.set_halign(gtk4::Align::End);
        box_.set_margin_top(6);
        for (index, label) in buttons.iter().enumerate() {
            let button = gtk4::Button::with_label(label);
            let result = result.clone();
            let done = done.clone();
            let window_for_click = window.clone();
            let accepted = index == 1;
            button.connect_clicked(move |_| {
                *result.borrow_mut() = Some(accepted);
                *done.borrow_mut() = true;
                window_for_click.close();
            });
            box_.append(&button);
            if index == 1 {
                window.set_default_widget(Some(&button));
            }
        }
        buttons_box = Some(box_);
    }
    if let Some(box_) = buttons_box {
        vbox.append(&box_);
    }

    window.set_child(Some(&vbox));

    let done_close = done.clone();
    window.connect_close_request(move |window| {
        *done_close.borrow_mut() = true;
        window.set_visible(false);
        Propagation::Proceed
    });

    if let Some(parent) = super::auth_dialog::top_level_parent() {
        window.set_transient_for(Some(&parent));
        window.set_destroy_with_parent(true);
    }

    window.present();

    while !*done.borrow() {
        glib::MainContext::default().iteration(true);
    }

    result.take()
}

// ---------------------------------------------------------------------------
// Adapters to the transport closures
// ---------------------------------------------------------------------------

/// Build a `HostKeyVerifier` prompt closure around an arbitrary notice handler.
///
/// `handler` receives the notice and returns the decision. Extracting it makes
/// the marshalling testable without a display.
pub fn host_key_prompt_with<H>(
    handler: H,
) -> Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>
where
    H: Fn(HostKeyNotice, &HostKeyFacts) -> HostKeyDecision + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |facts: &HostKeyFacts| {
        // Cloning the handler keeps the outer closure `Fn`; the verifier may
        // prompt for more than one host over a session's lifetime.
        let handler = Arc::clone(&handler);
        let facts = facts.clone();
        run_on_gtk_main_thread(move || handler(HostKeyNotice::Unknown, &facts))
            .unwrap_or(HostKeyDecision::Reject)
    })
}

/// GTK-backed prompt closure for [`crate::transport::hostkey::HostKeyVerifier`].
///
/// P2-7 wiring seam: this is the closure a connect attempt passes as
/// `HostKeyVerifier::new`'s `prompt`. `HostKeyVerifier::verify` only calls it
/// for *unknown* hosts under `strict`; the changed-key path returns
/// [`HostKeyDecision::Reject`] inside the verifier without prompting, so
/// [`prompt_changed`] is surfaced by the connection layer (which owns the
/// `TransportError::HostKeyRejected` presentation) rather than by this closure.
/// The closure cannot capture a [`gtk4::Window`] (not `Send`), so the dialog
/// resolves its transient parent itself.
pub fn host_key_prompt() -> Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync> {
    host_key_prompt_with(|_, facts| prompt_unknown(facts))
}

#[cfg(test)]
mod tests;
