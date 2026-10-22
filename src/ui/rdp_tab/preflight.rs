//! Everything the RDP tab settles before it dials: the session → argv
//! mapping, the FreeRDP probe, and the certificate trust prompts and
//! store lookups. The text and mapping helpers are display-free
//! (unit-tested here and in `tests/rdp_tab_flow.rs`); the two
//! `*_cert_decision` functions show modals on the GTK main thread.

use std::sync::OnceLock;

use gtk4::prelude::*;

use super::DEFAULT_PORT;
use crate::app::default_db_path;
use crate::rdp::{is_supported_version, probe_binary, RdpSpec, MIN_FREERDP_MAJOR};
use crate::store::model::SessionConfig;
use crate::store::SessionStore;
use crate::ui::auth_dialog::top_level_parent;
use crate::ui::widgets::{button_row, set_margins, wrapped_label, Modal};

/// `RdpSpec` for the session row with `cert_arg` attached (the resolved
/// `/cert:fingerprint:…` token, if any).
///
/// Username and host are trimmed; an empty username maps to `""` — the
/// caller (`start`) rejects it with an error view first, like the SSH
/// arm's missing-username. Geometry defaults to 1024×768; `depth` maps to
/// `/bpp:` (`None` skips the arg). Toggles come from `rdp_options`, or these
/// defaults when absent: audio off, clipboard/fonts/printer
/// on, compression on (the row bool carries compression when options are
/// present; dialog-created RDP rows default it on).
pub fn spec_from_session(session: &SessionConfig, cert_arg: Option<String>) -> RdpSpec {
    let (audio, clipboard, fonts, printer) = match &session.rdp_options {
        Some(options) => (
            options.audio,
            options.clipboard,
            options.fonts,
            options.printer,
        ),
        None => (false, true, true, true),
    };
    let trimmed = |value: &Option<String>| value.as_deref().unwrap_or_default().trim().to_string();
    RdpSpec {
        host: trimmed(&session.host),
        port: session.port.unwrap_or(DEFAULT_PORT),
        username: trimmed(&session.username),
        domain: Some(trimmed(&session.rdp_domain)).filter(|domain| !domain.is_empty()),
        width: session.rdp_width.unwrap_or(1024),
        height: session.rdp_height.unwrap_or(768),
        depth: session.rdp_color_depth,
        audio,
        clipboard,
        fonts,
        printer,
        compression: session.rdp_options.is_none() || session.rdp_compression,
        cert_arg,
    }
}

/// True when `version` (dotted, e.g. `3.15.0`) is below the fixed
/// `/from-stdin` band (3.20). Unparseable input warns nothing: without
/// evidence there is no warning.
pub fn warn_below(version: &str) -> bool {
    let mut parts = version.split('.');
    let number = |part: Option<&str>| part.and_then(|digits| digits.parse::<u32>().ok());
    match (number(parts.next()), number(parts.next())) {
        (Some(major), Some(minor)) => (major, minor) < (3, 20),
        _ => false,
    }
}

/// The error for a FreeRDP older than the argv speaks: which binary, which
/// version, and what to install instead.
pub fn freerdp_too_old_text(binary: &str, version: &str) -> String {
    format!(
        "{binary} is FreeRDP {version}, but RDP tabs need FreeRDP \
         {MIN_FREERDP_MAJOR} or later. Install FreeRDP 3 — Wayterm looks for \
         xfreerdp3 first, then xfreerdp."
    )
}

/// The unknown-certificate prompt body: host:port, the seen SHA-256
/// fingerprint, and the remember question.
pub fn unknown_cert_text(host: &str, port: u16, fingerprint: &str) -> String {
    format!(
        "The authenticity of host '{host}:{port}' can't be established.\n\
         RDP server certificate fingerprint (SHA-256): {fingerprint}\n\
         Connect and remember this certificate?"
    )
}

/// The changed-certificate warning body: host:port, both fingerprints, and
/// the MITM caution (wording mirrors the SSH changed-key notice).
pub fn changed_cert_text(stored: &str, seen: &str, host: &str, port: u16) -> String {
    format!(
        "The server certificate for '{host}:{port}' has changed.\n\
         Stored fingerprint: {stored}\n\
         Presented fingerprint: {seen}\n\
         This is the man-in-the-middle signal: either the host was rebuilt \
         or reinstalled without clearing its entry, or something is \
         intercepting your connection. Compare the presented fingerprint \
         with the one your administrator gives you, out of band. Wayterm \
         will not connect to a changed certificate unless you explicitly \
         update it."
    )
}

/// What the user accepted in the unknown-certificate dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptRecord {
    /// Server host.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// The accepted fingerprint.
    pub fingerprint: String,
}

/// The changed-certificate dialog outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeChoice {
    /// Disconnect: leave the stored pin.
    KeepRefusing,
    /// Update and Connect: record the new fingerprint and retry.
    UpdateAndRetry,
}

/// Show the unknown-certificate modal; `Some` only when the user pressed
/// Accept. Must run on the GTK main thread (callers on the blocking pool go
/// through `run_on_gtk_main_thread`).
pub fn unknown_cert_decision(host: &str, port: u16, seen_fp: &str) -> Option<AcceptRecord> {
    let modal = Modal::<AcceptRecord>::new(CERT_TITLE, top_level_parent().as_ref());
    let reject = modal.button("Reject", || None);
    let record = AcceptRecord {
        host: host.to_string(),
        port,
        fingerprint: seen_fp.to_string(),
    };
    let accept = modal.button("Accept", move || Some(record.clone()));
    modal.window.set_default_widget(Some(&accept));
    fill_cert_modal(
        &modal,
        520,
        &unknown_cert_text(host, port, seen_fp),
        &[&reject, &accept],
    );
    modal.run()
}

/// Show the changed-certificate warning; only an explicit update connects.
/// Must run on the GTK main thread; a closed window refuses.
pub fn changed_cert_decision(host: &str, port: u16, stored: &str, seen: &str) -> ChangeChoice {
    let modal = Modal::<ChangeChoice>::new(CERT_TITLE, top_level_parent().as_ref());
    let disconnect = modal.button("Disconnect", || Some(ChangeChoice::KeepRefusing));
    modal.window.set_default_widget(Some(&disconnect));
    let update = modal.button("Update and Connect", || Some(ChangeChoice::UpdateAndRetry));
    let text = changed_cert_text(stored, seen, host, port);
    fill_cert_modal(&modal, 560, &text, &[&disconnect, &update]);
    modal.run().unwrap_or(ChangeChoice::KeepRefusing)
}

/// Title of both certificate dialogs.
const CERT_TITLE: &str = "RDP certificate verification";

/// Lay out a certificate modal: fixed width, selectable body text, then
/// `buttons` right-aligned.
fn fill_cert_modal<T: 'static>(
    modal: &Modal<T>,
    width: i32,
    text: &str,
    buttons: &[&gtk4::Button],
) {
    modal.window.set_default_width(width);
    modal.window.set_resizable(false);
    let body = wrapped_label(text);
    body.set_selectable(true);
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&vbox, 12);
    vbox.append(&body);
    vbox.append(&button_row(buttons));
    modal.window.set_child(Some(&vbox));
}

/// The stored fingerprint for `(host, port)`, via a throwaway store handle
/// (see the module docs). Errors surface so the caller can decide; the dial
/// fails closed on them, since an unreadable store is not first contact.
pub fn rdp_trust_lookup(host: &str, port: u16) -> rusqlite::Result<Option<String>> {
    SessionStore::open(&default_db_path())?.rdp_trust(host, port)
}

/// Record the accepted fingerprint for `(host, port)`.
pub fn rdp_trust_record(host: &str, port: u16, fingerprint: &str) -> rusqlite::Result<()> {
    SessionStore::open(&default_db_path())?.rdp_trust_record(host, port, fingerprint)
}

/// The probed `(binary, version)`, once per process. Blocking (one
/// `--version` spawn): only called from `start` and the dial pool.
pub(super) fn cached_probe() -> Option<&'static (String, String)> {
    static PROBE: OnceLock<Option<(String, String)>> = OnceLock::new();
    PROBE.get_or_init(probe_binary).as_ref()
}

/// The probed `(binary, version)` if RDP can run on it: on `PATH`, and
/// FreeRDP 3 or later. `Err` carries the error view's message.
pub(super) fn usable_freerdp() -> Result<&'static (String, String), String> {
    let probe = cached_probe()
        .ok_or_else(|| "xfreerdp not found on PATH (looked for xfreerdp3, xfreerdp)".to_string())?;
    let (binary, version) = probe;
    if is_supported_version(version) {
        Ok(probe)
    } else {
        Err(freerdp_too_old_text(binary, version))
    }
}
