//! The challenge-response dialog: one generic credential UI for password
//! fallback, key passphrases, and keyboard-interactive rounds (doc 11).
//! Prompts with `echo == false` get a masked entry. Answers are never
//! logged or trimmed.

use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{glib, pango};

pub use crate::transport::auth::{ChallengePrompt, ChallengeResponder};
use crate::ui::widgets::{ok_cancel_row, set_margins, Modal};

/// One dialog row: its label and whether the entry echoes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChallengeField {
    pub label: String,
    pub echo: bool,
}

/// The rows for `prompts`, in order; an empty prompt gets a generic label.
pub fn field_specs(prompts: &[ChallengePrompt]) -> Vec<ChallengeField> {
    prompts
        .iter()
        .map(|p| ChallengeField {
            label: match p.prompt.trim() {
                "" => "Response".to_string(),
                label => label.to_string(),
            },
            echo: p.echo,
        })
        .collect()
}

pub fn window_title(name: &str) -> String {
    format!("Authentication required: {name}")
}

/// The instructions line, or `None` when the server sent nothing useful.
pub fn instructions_label(instructions: Option<&str>) -> Option<String> {
    instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Exactly one answer per prompt, verbatim: extra values are dropped,
/// missing ones are empty.
pub fn responses_from_fields(prompts: &[ChallengePrompt], fields: &[String]) -> Vec<String> {
    (0..prompts.len())
        .map(|i| fields.get(i).cloned().unwrap_or_default())
        .collect()
}

/// Run `f` on the GTK main thread and wait for its result (inline if this
/// thread already owns the main context). `None` means the main loop is
/// gone, which callers must treat as a refusal. Blocks the calling thread,
/// so transport-side callers run on the blocking pool.
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

/// The first toplevel window: the dialog parent for prompts raised from
/// transport closures, which cannot hold a widget.
pub fn top_level_parent() -> Option<gtk4::Window> {
    let toplevels = gtk4::Window::toplevels();
    (0..toplevels.n_items())
        .filter_map(|i| toplevels.item(i))
        .find_map(|obj| obj.downcast::<gtk4::Window>().ok())
}

/// Show the dialog and return the answers. `None` when the user cancels —
/// and for an empty prompt list, so the chain aborts instead of sending
/// nothing.
pub fn run_challenges(
    name: &str,
    instructions: Option<&str>,
    prompts: &[ChallengePrompt],
) -> Option<Vec<String>> {
    if prompts.is_empty() {
        return None;
    }
    let modal = Modal::new(&window_title(name), top_level_parent().as_ref());
    modal.window.set_default_width(420);
    modal.window.set_resizable(false);

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    set_margins(&vbox, 12);
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
    let mut entries: Vec<gtk4::Widget> = Vec::new();
    for (row, spec) in field_specs(prompts).iter().enumerate() {
        let entry: gtk4::Widget = if spec.echo {
            let entry = gtk4::Entry::new();
            entry.set_activates_default(true);
            entry.upcast()
        } else {
            // Masked by default; no peek icon, so a stray click cannot
            // reveal the secret.
            gtk4::PasswordEntry::builder()
                .show_peek_icon(false)
                .activates_default(true)
                .build()
                .upcast()
        };
        entry.set_hexpand(true);
        let label = gtk4::Label::builder()
            .label(&spec.label)
            .halign(gtk4::Align::End)
            .build();
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(&entry, 1, row as i32, 1, 1);
        entries.push(entry);
    }
    vbox.append(&grid);

    let (for_ok, prompts) = (entries.clone(), prompts.to_vec());
    vbox.append(&ok_cancel_row(&modal, move || {
        let values: Vec<String> = for_ok
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk4::Editable>())
            .map(|e| e.text().to_string())
            .collect();
        Some(responses_from_fields(&prompts, &values))
    }));
    modal.window.set_child(Some(&vbox));
    if let Some(first) = entries.first() {
        first.grab_focus();
    }
    modal.run()
}

/// A [`ChallengeResponder`] that runs `runner(name, instructions, prompts)`
/// on the GTK main thread (the runner is injectable for tests). The
/// responder's signature carries no per-round name or instructions, so
/// `name` is the connection's and instructions are `None`.
pub fn challenge_responder_with<R>(name: String, runner: R) -> ChallengeResponder
where
    R: Fn(&str, Option<&str>, &[ChallengePrompt]) -> Option<Vec<String>> + Send + Sync + 'static,
{
    let runner = Arc::new(runner);
    Arc::new(move |prompts: &[ChallengePrompt]| {
        let (runner, name, prompts) = (runner.clone(), name.clone(), prompts.to_vec());
        run_on_gtk_main_thread(move || runner(&name, None, &prompts)).flatten()
    })
}

/// The GTK-backed responder for a connect attempt.
pub fn challenge_responder(name: impl Into<String>) -> ChallengeResponder {
    challenge_responder_with(name.into(), run_challenges)
}

#[cfg(test)]
pub(crate) mod tests;
