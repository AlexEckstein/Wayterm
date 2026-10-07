//! Challenge-response dialog — the single generic credential UI required by
//! doc 11 §Authentication State Machine ("the same UI serves password
//! fallback, key passphrase prompts, and keyboard-interactive rounds").
//!
//! The dialog renders an optional instructions line plus one labelled entry
//! per [`ChallengePrompt`]. Prompts with `echo == false` (passwords, passphrases,
//! OTP secrets) get a visibility-off [`gtk4::PasswordEntry`]; `echo == true`
//! get a plain [`gtk4::Entry`].
//!
//! Split, following [`crate::ui::session_dialog`]:
//!   - a **display-free data layer** ([`ChallengeField`], [`field_specs`],
//!     [`responses_from_fields`], [`window_title`], [`instructions_label`])
//!     which is unit-tested in [`tests`];
//!   - a **thin GTK layer** ([`run_challenges`]) that blocks on nested
//!     [`glib::MainContext`] iteration while the user answers.
//!
//! Security: answers are never logged, never `Debug`-printed and never
//! trimmed. Only [`responses_from_fields`] copies entry text out, and it does
//! so verbatim.

use std::rc::Rc;
use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;

pub use crate::transport::auth::{ChallengePrompt, ChallengeResponder};

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// One rendered row of the dialog: a label and the echo mode of its entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChallengeField {
    /// Text shown to the left of the entry.
    pub label: String,
    /// `false` = secret entry with visibility off, `true` = plain entry.
    pub echo: bool,
}

/// Label used when the server sends an empty prompt string.
const EMPTY_PROMPT_LABEL: &str = "Response";

/// Derive the dialog rows from the server's prompts, preserving order.
pub fn field_specs(prompts: &[ChallengePrompt]) -> Vec<ChallengeField> {
    prompts
        .iter()
        .map(|p| ChallengeField {
            label: {
                let trimmed = p.prompt.trim();
                if trimmed.is_empty() {
                    EMPTY_PROMPT_LABEL.to_string()
                } else {
                    trimmed.to_string()
                }
            },
            echo: p.echo,
        })
        .collect()
}

/// Window title for a challenge round.
pub fn window_title(name: &str) -> String {
    format!("Authentication required: {name}")
}

/// Instructions line, or `None` when the server sent nothing useful.
pub fn instructions_label(instructions: Option<&str>) -> Option<String> {
    instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Collect the entry values into the response vector handed to the auth chain.
///
/// Guarantees `responses.len() == prompts.len()`: extra entry values are
/// dropped and missing ones are padded with the empty string, so a malformed
/// widget tree can never desynchronise the answers from the prompts.
///
/// Entry text is copied **verbatim** — no trimming, no normalisation. A
/// password with a leading space must reach the server unchanged.
pub fn responses_from_fields(prompts: &[ChallengePrompt], fields: &[String]) -> Vec<String> {
    (0..prompts.len())
        .map(|i| fields.get(i).cloned().unwrap_or_default())
        .collect()
}

// ---------------------------------------------------------------------------
// GTK main-thread marshalling
// ---------------------------------------------------------------------------

/// Run `f` on the GTK main thread and return its result.
///
/// Doc 11 §CredentialManager 3 ("the prompt runs on the GTK main thread … +
/// oneshot back to the transport task"). Implemented as
/// [`glib::MainContext::invoke`] plus a channel back to the caller; if the
/// caller already owns the main context (the dialog was opened from the GTK
/// main thread) `f` runs inline, which is the case that would otherwise
/// deadlock.
///
/// `None` means the main loop is gone (channel dropped), i.e. the app is
/// shutting down — callers must treat that as a negative answer
/// (cancelled prompt / rejected host key), never as a default accept.
///
/// Note for the async layer: the transport closures (`ChallengeResponder`,
/// `HostKeyVerifier`'s prompt) are synchronous, so the *calling* thread blocks
/// here for the duration of the dialog. P2-7 must invoke them from a blocking
/// context (e.g. `tokio::task::spawn_blocking`) rather than from a reactor
/// worker, so the runtime is not stalled.
pub fn run_on_gtk_main_thread<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let ctx = glib::MainContext::default();
    if ctx.is_owner() {
        return Some(f());
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    ctx.invoke(move || {
        let _ = tx.send(f());
    });
    rx.recv().ok()
}

/// First toplevel GTK window, used as the transient parent for dialogs that
/// are opened from a transport closure that cannot hold a widget reference.
pub fn top_level_parent() -> Option<gtk4::Window> {
    let toplevels = gtk4::Window::toplevels();
    (0..toplevels.n_items())
        .filter_map(|i| toplevels.item(i))
        .find_map(|obj| obj.downcast::<gtk4::Window>().ok())
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// Run the challenge-response dialog and return the answers.
///
/// One row per prompt: a label with the prompt text and an entry.
/// `echo == false` → [`gtk4::PasswordEntry`] with visibility off;
/// `echo == true` → plain [`gtk4::Entry`].
///
/// Returns `None` when the user cancels (Cancel button, Esc, or window close)
/// and also when `prompts` is empty — nothing to ask means nothing to answer,
/// and reporting "cancelled" makes the auth chain abort the connect instead of
/// sending an empty answer set (doc 11 §CredentialManager 3: "The dialog is
/// cancellable; cancellation aborts the connect"). This `Option` shape matches
/// [`ChallengeResponder`], whose `None` is exactly that abort signal.
///
/// P2-7 wiring seam: the transport-side [`ChallengeResponder`] cannot forward
/// the per-round `name`/`instructions` that russh's `InfoRequest` carries (see
/// [`challenge_responder`]), so keyboard-interactive rounds reach this dialog
/// with `name` = the connection's display name and `instructions = None`.
pub fn run_challenges(
    parent: &gtk4::Window,
    name: &str,
    instructions: Option<&str>,
    prompts: &[ChallengePrompt],
) -> Option<Vec<String>> {
    if prompts.is_empty() {
        return None;
    }
    build_and_run(Some(parent), name, instructions, prompts)
}

/// [`run_challenges`] without a transient parent.
///
/// Used by [`challenge_responder`], whose closure cannot capture a
/// [`gtk4::Window`] (not `Send`); the dialog resolves the parent itself via
/// [`top_level_parent`].
pub fn run_challenges_orphan(
    name: &str,
    instructions: Option<&str>,
    prompts: &[ChallengePrompt],
) -> Option<Vec<String>> {
    if prompts.is_empty() {
        return None;
    }
    build_and_run(None, name, instructions, prompts)
}

/// Read an entry's text through the [`gtk4::Editable`] interface, which both
/// [`gtk4::Entry`] and [`gtk4::PasswordEntry`] implement.
fn entry_text(widget: &gtk4::Widget) -> String {
    widget
        .downcast_ref::<gtk4::Editable>()
        .map(|e| e.text().to_string())
        .unwrap_or_default()
}

fn build_and_run(
    parent: Option<&gtk4::Window>,
    name: &str,
    instructions: Option<&str>,
    prompts: &[ChallengePrompt],
) -> Option<Vec<String>> {
    let window = gtk4::ApplicationWindow::builder()
        .title(window_title(name))
        .modal(true)
        .default_width(420)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
        window.set_destroy_with_parent(true);
    }

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    if let Some(text) = instructions_label(instructions) {
        let label = gtk4::Label::builder()
            .label(text)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .xalign(0.0)
            .build();
        label.add_css_class("dim-label");
        vbox.append(&label);
    }

    let grid = gtk4::Grid::builder()
        .row_spacing(6)
        .column_spacing(12)
        .build();

    let specs = field_specs(prompts);
    let mut entries: Vec<gtk4::Widget> = Vec::with_capacity(specs.len());
    for (row, spec) in specs.iter().enumerate() {
        let entry: gtk4::Widget = if spec.echo {
            gtk4::Entry::builder().hexpand(true).build().upcast()
        } else {
            // GtkPasswordEntry masks its content by default (the
            // `visibility` property is FALSE); the peek icon is the only way
            // to reveal it, and it is removed here so a secret cannot be
            // revealed by a stray click. gtk4-rs 0.11 exposes no
            // `set_visibility` binding for PasswordEntry — the masked default
            // is what is relied on.
            let pe = gtk4::PasswordEntry::builder().show_peek_icon(false).build();
            pe.upcast()
        };
        entry.set_hexpand(true);
        // GtkPasswordEntry derives from GtkWidget (not GtkEntry) but exposes the
        // same "Enter submits" property, so both concrete types are handled.
        if let Some(e) = entry.downcast_ref::<gtk4::Entry>() {
            e.set_activates_default(true);
        } else if let Some(e) = entry.downcast_ref::<gtk4::PasswordEntry>() {
            e.set_activates_default(true);
        }
        entries.push(entry.clone());

        let label = gtk4::Label::builder()
            .label(&spec.label)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Center)
            .build();
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(&entry, 1, row as i32, 1, 1);
    }
    vbox.append(&grid);

    let ok_button = gtk4::Button::with_label("OK");
    let cancel_button = gtk4::Button::with_label("Cancel");
    cancel_button.set_margin_end(12);
    let button_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    button_box.set_halign(gtk4::Align::End);
    button_box.set_margin_top(6);
    button_box.append(&cancel_button);
    button_box.append(&ok_button);
    vbox.append(&button_box);

    window.set_child(Some(&vbox));

    let result: Rc<std::cell::RefCell<Option<Vec<String>>>> =
        Rc::new(std::cell::RefCell::new(None));
    let done = Rc::new(std::cell::RefCell::new(false));
    let prompts_for_ok = prompts.to_vec();

    ok_button.connect_clicked({
        let window = window.clone();
        let result = result.clone();
        let done = done.clone();
        let entries = entries.clone();
        move |_| {
            let values: Vec<String> = entries.iter().map(entry_text).collect();
            *result.borrow_mut() = Some(responses_from_fields(&prompts_for_ok, &values));
            *done.borrow_mut() = true;
            window.close();
        }
    });

    let done_cancel = done.clone();
    cancel_button.connect_clicked({
        let window = window.clone();
        move |_| {
            *done_cancel.borrow_mut() = true;
            window.close();
        }
    });

    let done_close = done.clone();
    window.connect_close_request(move |window| {
        *done_close.borrow_mut() = true;
        window.set_visible(false);
        glib::Propagation::Proceed
    });

    window.set_default_widget(Some(&ok_button));

    window.present();
    if let Some(first) = entries.first() {
        first.grab_focus();
    }

    while !*done.borrow() {
        glib::MainContext::default().iteration(true);
    }

    result.take()
}

// ---------------------------------------------------------------------------
// Adapters to the transport closures
// ---------------------------------------------------------------------------

/// Build a [`ChallengeResponder`] around an arbitrary dialog runner.
///
/// `runner` is called on the GTK main thread (see
/// [`run_on_gtk_main_thread`]) with `(name, instructions, prompts)` and returns
/// the answers, or `None` for a cancelled prompt. Splitting the runner out is
/// what makes the marshalling logic testable without a display.
pub fn challenge_responder_with<R>(name: String, runner: R) -> ChallengeResponder
where
    R: Fn(&str, Option<&str>, &[ChallengePrompt]) -> Option<Vec<String>> + Send + Sync + 'static,
{
    let runner = Arc::new(runner);
    Arc::new(move |prompts: &[ChallengePrompt]| {
        // Both captures are cloned so the outer closure stays `Fn`; the
        // transport may call the responder once per challenge round.
        let runner = Arc::clone(&runner);
        let name = name.clone();
        let prompts = prompts.to_vec();
        run_on_gtk_main_thread(move || runner(&name, None, &prompts)).flatten()
    })
}

/// GTK-backed [`ChallengeResponder`] for the keyboard-interactive chain.
///
/// P2-7 wiring seam: this is the closure a connect attempt passes as
/// `AuthConfig::challenge_responder`. Two adaptations are baked in, both
/// consequences of the closure's signature (changing it is an
/// orchestration-level decision, not ours):
///
/// 1. `ChallengeResponder` receives only `&[ChallengePrompt]`, so the per-round
///    `name` / `instructions` from russh's `InfoRequest` cannot reach the dialog;
///    `name` is the connection's display name and `instructions` is `None`.
/// 2. The closure cannot capture a [`gtk4::Window`] (not `Send`), so the dialog
///    resolves its transient parent via [`top_level_parent`].
pub fn challenge_responder(name: impl Into<String>) -> ChallengeResponder {
    challenge_responder_with(
        name.into(),
        |name, instructions, prompts| match top_level_parent() {
            Some(parent) => run_challenges(&parent, name, instructions, prompts),
            None => run_challenges_orphan(name, instructions, prompts),
        },
    )
}

#[cfg(test)]
pub(crate) mod tests;
