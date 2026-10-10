//! The RDP tab page (doc 08 §RDP, §Tabs): an X11-backed `DrawingArea`
//! whose window reparents the xfreerdp child, plus the TOFU connect flow.
//!
//! The connect flow mirrors `vnc_tab::start`: validate the row, resolve the
//! password through the credential chain, resolve the server certificate
//! through the `rdp_trust` store table, then spawn. Everything that blocks
//! (password prompt, trust-store reads, the probe spawn, cert dialogs)
//! runs on the blocking pool inside [`dial`]; the GTK thread only builds
//! widgets and pumps the runner's events.
//!
//! ## Embedding (spike-validated on FreeRDP 3.15 + xrdp + openbox)
//!
//! - **XID acquisition:** after the area maps, a 50 ms poll reads
//!   `native().surface()`, downcasts to [`gdk4_x11::X11Surface`](https://docs.rs/gdk4-x11),
//!   and takes [`xid()`](https://docs.rs/gdk4-x11) once nonzero.
//! - **Spawn** passes the host XID as `parent_xid` (`/parent-window:`).
//! - **Maintenance** keeps one x11rb connection on the GTK thread: the host
//!   tree is snapshotted before OUR spawn, then discovery (~1 s, again ~3 s)
//!   tracks only the newcomer past that baseline ([`track_newcomer`]). A
//!   pre-existing child is never ours: GTK keeps at least one of its own
//!   under the toplevel (measured 0x20000D alongside host 0x20000C — the
//!   tree never holds "exactly one"), and an orphan from a killed previous
//!   run may linger. `ConfigureWindow` positions our tracked child at the
//!   area's surface-relative origin ([`area_origin`] — the host XID is the
//!   toplevel window the area shares, not a per-area window) on resize plus
//!   a 250 ms poll fallback, and X input focus is handed to our child when
//!   focus sits on the parent (without the handoff there is no keyboard
//!   under a WM). Configure and focus only the tracked window, and kill
//!   nothing (teardown signals our own process group via
//!   [`RdpProcess::terminate`](crate::rdp::RdpProcess::terminate)).
//! - **Teardown:** [`EngineSlot`] owns the process handle and the runner
//!   task; page close, retry, or a superseded attempt aborts the runner and
//!   terminates our own child.
//!
//! ## Trust store access
//!
//! The credential chain (`credential_manager`) carries secrets only. The
//! trust table is reached the other allowed way: [`default_db_path`] +
//! [`SessionStore::open`] on the blocking thread at cert-resolution time.
//! A store that cannot be read fails the connect: it is never mistaken for
//! first contact, which would re-prompt for a host already pinned.
//! That is a second read-only-mostly handle to the same SQLite file, which
//! is safe: schema init is idempotent, reads are concurrent under WAL, and
//! trust writes are rare single-row upserts serialized by SQLite itself.
//!
//! ## Wayland
//!
//! When the mapped surface does not downcast to `X11Surface` the page shows
//! the doc-08 choice instead of embedding: relaunch the app under
//! `GDK_BACKEND=x11` (the current instance exits after respawning itself —
//! `Application::quit`-style via the default application handle) or open
//! xfreerdp as a top-level window (`parent_xid = None`, fully supported by
//! `build_args`). Never a silent second window. This path is untestable on
//! an X-only host; it is code-review-only quality.
//!
//! ## Version notice
//!
//! `/from-stdin` was buggy around 3.5–3.17 (fixed in 3.20). The binary is
//! probed once per process ([`cached_probe`]) and, when below 3.20, the
//! connecting overlay carries a warning ([`warn_below`], pure and tested).
//! It disappears at Connected with the overlay.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::app::default_db_path;
use crate::rdp::cert::{parse_seen_fingerprint, PROBE_FINGERPRINT};
use crate::rdp::{
    cert_arg, decide, probe_binary, CertDecision, RdpProcess, RdpProcessEvent, RdpSpec,
    CERT_ARG_PREFIX,
};
use crate::store::model::SessionConfig;
use crate::store::SessionStore;
use crate::transport::credentials::CredentialManager;
use crate::transport::types::TransportState;
use crate::ui::auth_dialog::{run_on_gtk_main_thread, top_level_parent};
use crate::ui::tab::{
    credential_manager, current_gen, publish_status, set_terminal_area, show_connecting_view,
    show_error_view,
};
use crate::ui::widgets::{centered_label, data, set_data, set_margins};

/// The RDP default port (display `:0`-style: 3389).
pub const DEFAULT_PORT: u16 = 3389;
/// Password secret type in the credential chain (doc 06 naming).
const PASSWORD_SECRET: &str = "rdp_password";
/// Page data key for the process slot.
const ENGINE_KEY: &str = "wayterm-rdp-engine";
/// Page data key for our own attempt generation (the tab-level generation
/// cannot be bumped from here, and the overlay Cancel button must still
/// invalidate a dial in flight).
const RDP_GEN_KEY: &str = "wayterm-rdp-gen";
/// How long the acquisition probe may run before it is terminated.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// XID poll interval while waiting for the area to map.
const XID_POLL: Duration = Duration::from_millis(50);
/// Give up waiting for a map after ~10 s.
const XID_POLLS_MAX: u32 = 200;
/// Child discovery: first query ~1 s after spawn, second ~3 s.
const DISCOVER_FIRST: Duration = Duration::from_secs(1);
const DISCOVER_SECOND: Duration = Duration::from_secs(3);
/// `ConfigureWindow` fallback poll.
const CONFIGURE_POLL: Duration = Duration::from_millis(250);
/// X input-focus handoff poll.
const FOCUS_POLL: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------------------
// Pure mapping (display-free; unit-tested here and in tests/rdp_tab_flow.rs)
// ---------------------------------------------------------------------------

/// `RdpSpec` for the session row with `cert_arg` attached (the resolved
/// `/cert:fingerprint:…` token, if any).
///
/// Username and host are trimmed; an empty username maps to `""` — the
/// caller ([`start`]) rejects it with an error view first, like the SSH
/// arm's missing-username. Geometry defaults to 1024×768; `depth` maps to
/// `/bpp:` (`None` skips the arg). Toggles come from `rdp_options`, or the
/// plan-contract defaults when absent: audio off, clipboard/fonts/printer
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
    let compression = session
        .rdp_options
        .as_ref()
        .map(|_| session.rdp_compression)
        .unwrap_or(true);
    RdpSpec {
        host: session.host.clone().unwrap_or_default().trim().to_string(),
        port: session.port.unwrap_or(DEFAULT_PORT),
        username: session
            .username
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string(),
        domain: session
            .rdp_domain
            .clone()
            .map(|domain| domain.trim().to_string())
            .filter(|domain| !domain.is_empty()),
        width: session.rdp_width.unwrap_or(1024),
        height: session.rdp_height.unwrap_or(768),
        depth: session.rdp_color_depth,
        audio,
        clipboard,
        fonts,
        printer,
        compression,
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
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
}

/// The changed-certificate dialog outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeChoice {
    KeepRefusing,
    UpdateAndRetry,
}

/// Show the unknown-certificate modal; `Some` only when the user pressed
/// Accept. Must run on the GTK main thread (callers on the blocking pool go
/// through `run_on_gtk_main_thread`).
pub fn unknown_cert_decision(host: &str, port: u16, seen_fp: &str) -> Option<AcceptRecord> {
    let modal = crate::ui::widgets::Modal::<AcceptRecord>::new(
        "RDP certificate verification",
        top_level_parent().as_ref(),
    );
    modal.window.set_default_width(520);
    modal.window.set_resizable(false);
    let body = gtk4::Label::builder()
        .label(unknown_cert_text(host, port, seen_fp))
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .xalign(0.0)
        .build();
    body.set_selectable(true);
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&vbox, 12);
    vbox.append(&body);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.set_halign(gtk4::Align::End);
    let reject = modal.button("Reject", || None);
    let record = AcceptRecord {
        host: host.to_string(),
        port,
        fingerprint: seen_fp.to_string(),
    };
    let accept = modal.button("Accept", move || Some(record.clone()));
    modal.window.set_default_widget(Some(&accept));
    row.append(&reject);
    row.append(&accept);
    vbox.append(&row);
    modal.window.set_child(Some(&vbox));
    modal.run()
}

/// Show the changed-certificate warning; only an explicit update connects.
/// Must run on the GTK main thread; a closed window refuses.
pub fn changed_cert_decision(host: &str, port: u16, stored: &str, seen: &str) -> ChangeChoice {
    let modal = crate::ui::widgets::Modal::<ChangeChoice>::new(
        "RDP certificate verification",
        top_level_parent().as_ref(),
    );
    modal.window.set_default_width(560);
    modal.window.set_resizable(false);
    let body = gtk4::Label::builder()
        .label(changed_cert_text(stored, seen, host, port))
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .xalign(0.0)
        .build();
    body.set_selectable(true);
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    set_margins(&vbox, 12);
    vbox.append(&body);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.set_halign(gtk4::Align::End);
    let disconnect = modal.button("Disconnect", || Some(ChangeChoice::KeepRefusing));
    modal.window.set_default_widget(Some(&disconnect));
    let update = modal.button("Update and Connect", || Some(ChangeChoice::UpdateAndRetry));
    row.append(&disconnect);
    row.append(&update);
    vbox.append(&row);
    modal.window.set_child(Some(&vbox));
    modal.run().unwrap_or(ChangeChoice::KeepRefusing)
}

/// The stored fingerprint for `(host, port)`, via a throwaway store handle
/// (see the module docs). Errors surface so the caller can decide; the dial
/// fails closed on them.
pub fn rdp_trust_lookup(host: &str, port: u16) -> rusqlite::Result<Option<String>> {
    SessionStore::open(&default_db_path())?.rdp_trust(host, port)
}

/// Record the accepted fingerprint for `(host, port)`.
pub fn rdp_trust_record(host: &str, port: u16, fingerprint: &str) -> rusqlite::Result<()> {
    SessionStore::open(&default_db_path())?.rdp_trust_record(host, port, fingerprint)
}

/// The probed `(binary, version)`, once per process. Blocking (one
/// `--version` spawn): only called from `start` and the dial pool.
fn cached_probe() -> Option<(String, String)> {
    static PROBE: OnceLock<Option<(String, String)>> = OnceLock::new();
    PROBE.get_or_init(probe_binary).clone()
}

// ---------------------------------------------------------------------------
// X11 embedding helpers (GTK thread; x11rb connection stays here)
// ---------------------------------------------------------------------------

/// What the area's surface says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XidState {
    /// Not mapped yet (or a zero XID): keep polling.
    Unmapped,
    /// Mapped but not X11 (Wayland-native GDK): show the choice view.
    Wayland,
    /// The host window id to spawn xfreerdp into.
    Ready(u32),
}

/// Read the host XID off a (possibly unmapped) area.
fn surface_xid(area: &gtk4::DrawingArea) -> XidState {
    let Some(native) = area.native() else {
        return XidState::Unmapped;
    };
    let Some(surface) = native.surface() else {
        return XidState::Unmapped;
    };
    match surface.downcast::<gdk4_x11::X11Surface>() {
        Ok(x11) => match u32::try_from(x11.xid()) {
            Ok(xid) if xid != 0 => XidState::Ready(xid),
            _ => XidState::Unmapped,
        },
        Err(_) => XidState::Wayland,
    }
}

/// All current children of the host window; empty when the query fails.
fn child_windows(conn: &x11rb::rust_connection::RustConnection, host: u32) -> Vec<u32> {
    use x11rb::protocol::xproto::ConnectionExt;
    conn.query_tree(host)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|tree| tree.children)
        .unwrap_or_default()
}

/// Our freshly spawned child: the youngest window under `host` that was not
/// in `baseline` (the tree snapshot taken before OUR spawn). A pre-existing
/// child — GTK itself keeps at least one under the toplevel, and an orphan
/// from a killed previous run may linger — is never ours. `None` when
/// nothing new appeared yet.
fn track_newcomer(baseline: &[u32], children: &[u32]) -> Option<u32> {
    children
        .iter()
        .rev()
        .find(|child| !baseline.contains(child))
        .copied()
}

/// Resize our tracked child to the area allocation at the area's
/// surface-relative origin; a zero child or size is a no-op. Errors are
/// ignored: the 250 ms poll retries.
fn configure_child(
    conn: &x11rb::rust_connection::RustConnection,
    child: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt};
    if child == 0 || width <= 0 || height <= 0 {
        return;
    }
    let aux = ConfigureWindowAux::new()
        .x(x)
        .y(y)
        .width(width as u32)
        .height(height as u32);
    let _ = conn
        .configure_window(child, &aux)
        .map(|cookie| cookie.check());
    let _ = conn.flush();
}

/// The area's origin in toplevel-surface coordinates. The host XID comes
/// from `native().surface()` — the toplevel window, which the area shares —
/// so the child must sit at the translated origin, not at (0, 0).
fn area_origin(area: &gtk4::DrawingArea) -> Option<(i32, i32)> {
    let root = area.root()?;
    let point = area.compute_point(&root, &gtk4::graphene::Point::new(0.0, 0.0))?;
    Some((point.x() as i32, point.y() as i32))
}

/// Hand X input focus to our child when focus sits on the parent window
/// (gtk owns it). Without this there is no keyboard under a WM. Never
/// steals focus from anywhere else; on teardown focus is left alone.
fn hand_focus_to_child(conn: &x11rb::rust_connection::RustConnection, host: u32, child: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, InputFocus};
    if child == 0 {
        return;
    }
    let Ok(cookie) = conn.get_input_focus() else {
        return;
    };
    let Ok(focus) = cookie.reply() else {
        return;
    };
    if focus.focus == host {
        let _ = conn.set_input_focus(InputFocus::PARENT, child, x11rb::CURRENT_TIME);
        let _ = conn.flush();
    }
}

/// The embed container: one X11-backed child area. No controllers — the
/// remote desktop consumes input, and Tier-2 shortcuts must not intercept
/// keys (doc 08 §Tabs).
fn build_area() -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_focusable(true);
    area.set_focus_on_click(true);
    area
}

// ---------------------------------------------------------------------------
// Connect flow
// ---------------------------------------------------------------------------

type Proc = Arc<tokio::sync::Mutex<RdpProcess>>;

/// Everything one attempt needs on the GTK thread.
#[derive(Clone)]
struct Attempt {
    notebook: gtk4::Notebook,
    page: gtk4::Widget,
    session: Rc<SessionConfig>,
    tab_gen: u64,
    our_gen: u64,
}

impl Attempt {
    fn live(&self) -> bool {
        current_gen(&self.page) == self.tab_gen && rdp_gen_value(&self.page) == self.our_gen
    }

    fn host(&self) -> String {
        self.session
            .host
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn port(&self) -> u16 {
        self.session.port.unwrap_or(DEFAULT_PORT)
    }
}

fn rdp_gen_slot(page: &gtk4::Widget) -> Rc<Cell<u64>> {
    data(page, RDP_GEN_KEY).unwrap_or_else(|| {
        let slot = Rc::new(Cell::new(0));
        set_data(page, RDP_GEN_KEY, slot.clone());
        slot
    })
}

fn rdp_gen_value(page: &gtk4::Widget) -> u64 {
    rdp_gen_slot(page).get()
}

/// Invalidate our in-flight attempt (overlay Cancel, retry).
fn bump_rdp_gen(page: &gtk4::Widget) -> u64 {
    let slot = rdp_gen_slot(page);
    slot.set(slot.get().wrapping_add(1));
    slot.get()
}

/// Owns the page's process and its runner task; dropping it (page
/// finalized, replaced by a retry, or superseded) aborts the runner and
/// terminates our own child (SIGTERM pgroup → grace → SIGKILL).
struct EngineSlot(RefCell<Option<SlotState>>);

struct SlotState {
    proc: Proc,
    runner: tokio::task::JoinHandle<io::Result<()>>,
}

impl EngineSlot {
    fn with_runner(proc: Proc, runner: tokio::task::JoinHandle<io::Result<()>>) -> Self {
        Self(RefCell::new(Some(SlotState { proc, runner })))
    }
}

impl Drop for EngineSlot {
    fn drop(&mut self) {
        if let Some(state) = self.0.borrow_mut().take() {
            dispose_process(state.proc, state.runner);
        }
    }
}

/// Cancel the page's current process, if any.
fn retire_slot(page: &gtk4::Widget) {
    set_data(page, ENGINE_KEY, Rc::new(EngineSlot(RefCell::new(None))));
}

/// Abort the runner and terminate our own child. When no runtime handle is
/// reachable (off-thread drop in tests), the child's `kill_on_drop` is the
/// backstop: dropping the last `Arc` reaps it.
fn dispose_process(proc: Proc, runner: tokio::task::JoinHandle<io::Result<()>>) {
    runner.abort();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            proc.lock().await.terminate().await;
        });
    }
}

enum Dial {
    /// The password prompt was cancelled, or the user rejected a TOFU
    /// prompt: a clean disconnect, not an error.
    Cancelled,
    Failed(String),
    Ready(ReadyDial),
}

struct ReadyDial {
    proc: Proc,
    runner: tokio::task::JoinHandle<io::Result<()>>,
    events: mpsc::Receiver<RdpProcessEvent>,
    /// The fingerprint the real spawn was pinned to (for the changed-cert
    /// check on pre-connect failures).
    stored: Option<String>,
}

/// Resolve the password, then the certificate, then spawn. Blocks on the
/// prompt and the probe, so it runs on the blocking pool.
fn dial(session: SessionConfig, parent_xid: Option<u32>, creds: CredentialManager) -> Dial {
    let host = session.host.clone().unwrap_or_default().trim().to_string();
    let port = session.port.unwrap_or(DEFAULT_PORT);
    let id = session.id.to_string();
    let Some(password) = creds.lookup(&id, PASSWORD_SECRET) else {
        return Dial::Cancelled;
    };
    let Some((binary, _)) = cached_probe() else {
        return Dial::Failed("xfreerdp not found on PATH (looked for xfreerdp3, xfreerdp)".into());
    };
    // Fail closed: an unreadable store is not first contact. Treating it as
    // one would re-prompt for — and let the user re-pin — a host whose
    // stored pin we merely failed to read.
    let stored = match rdp_trust_lookup(&host, port) {
        Ok(stored) => stored,
        Err(err) => return Dial::Failed(format!("RDP trust store unreadable: {err}")),
    };
    if let Some(fingerprint) = stored.clone() {
        // Known host: pass-through spawn, xfreerdp enforces the pin.
        let decision = CertDecision::PassStored { fingerprint };
        let token = cert_arg(&decision);
        return spawn_real(
            &binary,
            &spec_from_session(&session, token),
            parent_xid,
            &password,
            stored,
        );
    }
    // First contact: probe spawn with the guaranteed-mismatch fingerprint.
    // xfreerdp refuses at TLS and prints the seen fingerprint; it writes
    // nothing to any store and exits within ~1 s. The probe gets an empty
    // password: nothing secret goes to a server the user has not accepted,
    // whatever the peer negotiates.
    let probe_spec = spec_from_session(
        &session,
        Some(format!("{CERT_ARG_PREFIX}{PROBE_FINGERPRINT}")),
    );
    let mut probe = RdpProcess::new(&binary, &probe_spec, parent_xid);
    let (tx, _) = mpsc::channel(8);
    let outcome = tokio::runtime::Handle::current().block_on(async {
        tokio::time::timeout(PROBE_TIMEOUT, probe.run(&Zeroizing::default(), tx)).await
    });
    match outcome {
        Err(_) => {
            tokio::runtime::Handle::current().block_on(probe.terminate());
            return Dial::Failed(format!("certificate probe for {host}:{port} timed out"));
        }
        Ok(Err(err)) => {
            return Dial::Failed(format!("certificate probe failed: {err}"));
        }
        Ok(Ok(())) => {}
    }
    let digest = probe.error_digest();
    let Some(seen) = parse_seen_fingerprint(&digest) else {
        let tail: Vec<&str> = digest.iter().rev().take(3).map(String::as_str).collect();
        let tail = if tail.is_empty() {
            String::new()
        } else {
            format!(" ({})", tail.join(" | "))
        };
        return Dial::Failed(format!("no certificate presented by {host}:{port}{tail}"));
    };
    let decision = run_on_gtk_main_thread({
        let (dialog_host, dialog_seen) = (host.clone(), seen.clone());
        move || unknown_cert_decision(&dialog_host, port, &dialog_seen)
    })
    .flatten();
    match decision {
        Some(record) => {
            if let Err(err) = rdp_trust_record(&record.host, record.port, &record.fingerprint) {
                eprintln!("Wayterm: RDP trust record failed: {err}");
            }
            let token = cert_arg(&CertDecision::PassStored {
                fingerprint: record.fingerprint.clone(),
            });
            spawn_real(
                &binary,
                &spec_from_session(&session, token),
                parent_xid,
                &password,
                Some(record.fingerprint),
            )
        }
        None => Dial::Cancelled,
    }
}

/// Spawn the real session: the runner task owns a process lock guard only
/// across `run`, and the handle triple returns to the GTK thread.
fn spawn_real(
    binary: &str,
    spec: &RdpSpec,
    parent_xid: Option<u32>,
    password: &Zeroizing<Vec<u8>>,
    stored: Option<String>,
) -> Dial {
    let proc: Proc = Arc::new(tokio::sync::Mutex::new(RdpProcess::new(
        binary, spec, parent_xid,
    )));
    let (tx, rx) = mpsc::channel(16);
    let (task_proc, task_password) = (proc.clone(), password.clone());
    let runner = tokio::runtime::Handle::current()
        .spawn(async move { task_proc.lock().await.run(&task_password, tx).await });
    Dial::Ready(ReadyDial {
        proc,
        runner,
        events: rx,
        stored,
    })
}

/// The embedded widgets an attempt spawned into, if any. `baseline` is the
/// host tree snapshot taken before OUR spawn, so discovery tracks only the
/// newcomer (see [`track_newcomer`]).
struct Embed {
    area: gtk4::DrawingArea,
    overlay: gtk4::Overlay,
    status: gtk4::Widget,
    host_xid: u32,
    baseline: Vec<u32>,
}

/// Start (or retry) the RDP connect for `page`; `gen` is the attempt
/// generation `start_connect` just bumped.
pub(crate) fn start(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    gen: u64,
) {
    retire_slot(page);
    let attempt = Attempt {
        notebook: notebook.clone(),
        page: page.clone(),
        session: session.clone(),
        tab_gen: gen,
        our_gen: bump_rdp_gen(page),
    };
    let filled = |value: &Option<String>| value.clone().filter(|v| !v.trim().is_empty());
    let Some(host) = filled(&session.host) else {
        return show_error_view(
            notebook,
            page,
            session,
            "This session has no host configured.",
        );
    };
    if filled(&session.username).is_none() {
        return show_error_view(
            notebook,
            page,
            session,
            "This session has no username configured.",
        );
    };
    let port = session.port.unwrap_or(DEFAULT_PORT);
    let Some((_, version)) = cached_probe() else {
        return show_error_view(
            notebook,
            page,
            session,
            "xfreerdp not found on PATH (looked for xfreerdp3, xfreerdp)",
        );
    };
    // The FreeRDP version notice lives in the connecting overlay and
    // disappears at Connected with it.
    let warning = warn_below(&version).then(|| {
        format!(
            "FreeRDP {version} — credential pipe buggy band (fixed 3.20); \
             if the password is rejected, check stdin auth"
        )
    });
    show_connecting_view(page, &host, port);
    publish_status(page, TransportState::Connecting, &host, port);

    let area = build_area();
    let overlay = gtk4::Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.set_child(Some(&area));
    let status = connecting_overlay(&host, port, warning.as_deref(), &attempt);
    overlay.add_overlay(&status);
    set_terminal_area(page, &overlay);
    wait_for_xid(attempt, area, overlay, status, 0);
}

/// The connecting overlay: host:port, the optional FreeRDP warning, and a
/// Cancel button. Removed at Connected; any error view replaces the whole
/// overlay.
fn connecting_overlay(
    host: &str,
    port: u16,
    warning: Option<&str>,
    attempt: &Attempt,
) -> gtk4::Widget {
    let view = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    view.set_vexpand(true);
    view.set_hexpand(true);
    view.set_valign(gtk4::Align::Center);
    view.set_halign(gtk4::Align::Center);
    let mut text = format!("Connecting to {host}:{port}…");
    if let Some(warning) = warning {
        text.push('\n');
        text.push_str(warning);
    }
    view.append(&centered_label(&text));
    let cancel = gtk4::Button::with_label("Cancel");
    cancel.set_halign(gtk4::Align::Center);
    let attempt = attempt.clone();
    cancel.connect_clicked(move |_| {
        bump_rdp_gen(&attempt.page);
        retire_slot(&attempt.page);
        set_terminal_area(&attempt.page, &centered_label("Disconnected"));
        publish_status(
            &attempt.page,
            TransportState::Disconnected,
            &attempt.host(),
            attempt.port(),
        );
    });
    view.append(&cancel);
    view.upcast()
}

/// Poll for the area's XID, then dial. `ticks` counts down
/// [`XID_POLLS_MAX`] while unmapped.
fn wait_for_xid(
    attempt: Attempt,
    area: gtk4::DrawingArea,
    overlay: gtk4::Overlay,
    status: gtk4::Widget,
    ticks: u32,
) {
    let weak_area = area.downgrade();
    glib::timeout_add_local(XID_POLL, move || {
        if !attempt.live() {
            return glib::ControlFlow::Break;
        }
        let Some(area) = weak_area.upgrade() else {
            return glib::ControlFlow::Break;
        };
        match surface_xid(&area) {
            XidState::Ready(xid) => {
                // Baseline BEFORE the spawn: discovery later tracks only
                // the newcomer, never GTK's own children or an orphan.
                let baseline = x11rb::connect(None)
                    .ok()
                    .map(|(conn, _)| child_windows(&conn, xid))
                    .unwrap_or_default();
                launch_dial(
                    &attempt,
                    Some(xid),
                    Some(Embed {
                        area: area.clone(),
                        overlay: overlay.clone(),
                        status: status.clone(),
                        host_xid: xid,
                        baseline,
                    }),
                );
                glib::ControlFlow::Break
            }
            XidState::Wayland => {
                show_wayland_choice(&attempt);
                glib::ControlFlow::Break
            }
            XidState::Unmapped if ticks >= XID_POLLS_MAX => {
                let message = "The RDP container never mapped to a window.".to_string();
                show_error_view(&attempt.notebook, &attempt.page, &attempt.session, &message);
                publish_status(
                    &attempt.page,
                    TransportState::Error(message),
                    &attempt.host(),
                    attempt.port(),
                );
                glib::ControlFlow::Break
            }
            XidState::Unmapped => {
                wait_for_xid(
                    attempt.clone(),
                    area.clone(),
                    overlay.clone(),
                    status.clone(),
                    ticks + 1,
                );
                glib::ControlFlow::Break
            }
        }
    });
}

/// Run the dial on the blocking pool and attach its result.
fn launch_dial(attempt: &Attempt, parent_xid: Option<u32>, embed: Option<Embed>) {
    let creds = credential_manager(&attempt.session.name);
    let session = attempt.session.as_ref().clone();
    let task = tokio::task::spawn_blocking(move || dial(session, parent_xid, creds));
    let (attempt, embed) = (attempt.clone(), embed);
    glib::MainContext::default().spawn_local(async move {
        let result = task.await;
        if !attempt.live() {
            // Superseded: a late process must not leak its child.
            if let Ok(Dial::Ready(ready)) = result {
                dispose_process(ready.proc, ready.runner);
            }
            return;
        }
        let host = attempt.host();
        let port = attempt.port();
        let error = match result {
            Ok(Dial::Ready(ready)) => return attach(&attempt, ready, embed),
            Ok(Dial::Cancelled) => {
                set_terminal_area(&attempt.page, &centered_label("Disconnected"));
                publish_status(&attempt.page, TransportState::Disconnected, &host, port);
                return;
            }
            Ok(Dial::Failed(error)) => error,
            Err(join_err) => format!("connect task failed: {join_err}"),
        };
        show_error_view(&attempt.notebook, &attempt.page, &attempt.session, &error);
        publish_status(&attempt.page, TransportState::Error(error), &host, port);
    });
}

/// Show the framebuffer's host area, start X11 maintenance, and pump the
/// runner's events into the page.
fn attach(attempt: &Attempt, ready: ReadyDial, embed: Option<Embed>) {
    let host = attempt.host();
    let port = attempt.port();
    let ReadyDial {
        proc,
        runner,
        mut events,
        stored,
    } = ready;
    set_data(
        &attempt.page,
        ENGINE_KEY,
        Rc::new(EngineSlot::with_runner(proc.clone(), runner)),
    );
    if let Some(embed) = &embed {
        start_maintenance(attempt, &embed.area, embed.host_xid, &embed.baseline);
    }
    // A top-level window has no host area: say where the desktop went.
    if embed.is_none() {
        set_terminal_area(
            &attempt.page,
            &centered_label("Running in a separate xfreerdp window…"),
        );
    }
    let attempt = attempt.clone();
    glib::MainContext::default().spawn_local(async move {
        let mut connected = false;
        while let Some(event) = events.recv().await {
            if !attempt.live() {
                return;
            }
            match event {
                RdpProcessEvent::Connected => {
                    connected = true;
                    if let Some(embed) = &embed {
                        embed.overlay.remove_overlay(&embed.status);
                    } else {
                        set_terminal_area(
                            &attempt.page,
                            &centered_label(
                                "Connected — the desktop is in the separate xfreerdp window.",
                            ),
                        );
                    }
                    publish_status(&attempt.page, TransportState::Connected, &host, port);
                }
                RdpProcessEvent::Ended { code, signal } => {
                    // A pre-connect failure against a pinned host may be a
                    // certificate change: xfreerdp enforces the pin and
                    // prints the seen fingerprint on stderr.
                    if !connected {
                        if let Some(stored) = stored.as_deref() {
                            let digest = proc.lock().await.error_digest();
                            if let Some(seen) = parse_seen_fingerprint(&digest) {
                                let expected = CertDecision::ChangedCert {
                                    stored: stored.to_string(),
                                    seen: seen.clone(),
                                };
                                if decide(Some(stored), Some(&seen)) == expected {
                                    match changed_cert_decision(&host, port, stored, &seen) {
                                        ChangeChoice::UpdateAndRetry => {
                                            if let Err(err) = rdp_trust_record(&host, port, &seen) {
                                                eprintln!(
                                                    "Wayterm: RDP trust record failed: {err}"
                                                );
                                            }
                                            start(
                                                &attempt.notebook,
                                                &attempt.page,
                                                &attempt.session,
                                                attempt.tab_gen,
                                            );
                                            return;
                                        }
                                        ChangeChoice::KeepRefusing => {
                                            let message =
                                                "server certificate changed; connection refused"
                                                    .to_string();
                                            show_error_view(
                                                &attempt.notebook,
                                                &attempt.page,
                                                &attempt.session,
                                                &message,
                                            );
                                            publish_status(
                                                &attempt.page,
                                                TransportState::Error(message),
                                                &host,
                                                port,
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let reason = ended_message(code, signal, &proc.lock().await.error_digest());
                    show_error_view(&attempt.notebook, &attempt.page, &attempt.session, &reason);
                    publish_status(&attempt.page, TransportState::Error(reason), &host, port);
                    return;
                }
            }
        }
    });
}

/// The plain-ended message: exit code/signal plus the stderr tail (which
/// never carries the password — it only travels on stdin).
fn ended_message(code: Option<i32>, signal: Option<i32>, digest: &[String]) -> String {
    let ending = match (code, signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "an unknown cause".to_string(),
    };
    let tail: Vec<&str> = digest.iter().rev().take(3).map(String::as_str).collect();
    if tail.is_empty() {
        format!("The RDP session ended ({ending}).")
    } else {
        format!("The RDP session ended ({ending}): {}", tail.join(" | "))
    }
}

/// Start child discovery, configure, and focus maintenance for our tracked
/// window. Every callback stops itself once the attempt is superseded.
fn start_maintenance(attempt: &Attempt, area: &gtk4::DrawingArea, host_xid: u32, baseline: &[u32]) {
    let Ok((conn, _)) = x11rb::connect(None) else {
        eprintln!("Wayterm: RDP x11rb connect failed; the child window will not be resized");
        return;
    };
    let maintainer = Rc::new(Maintainer {
        conn,
        host: host_xid,
        child: Cell::new(0),
    });

    // Discovery: once ~1 s after spawn, once more ~3 s (a late mapper or a
    // retry's second child). Only the newcomer past the pre-spawn baseline
    // is ever tracked; the second pass additionally replaces the tracked
    // window when it died.
    for delay in [DISCOVER_FIRST, DISCOVER_SECOND] {
        let (attempt, maintainer, baseline) =
            (attempt.clone(), maintainer.clone(), baseline.to_vec());
        glib::timeout_add_local_once(delay, move || {
            if !attempt.live() {
                return;
            }
            let children = child_windows(&maintainer.conn, maintainer.host);
            let Some(newcomer) = track_newcomer(&baseline, &children) else {
                return;
            };
            let known = maintainer.child.get();
            if delay == DISCOVER_FIRST || known == 0 || !children.contains(&known) {
                maintainer.child.set(newcomer);
            }
        });
    }

    // Configure on resize (fast path) plus a 250 ms poll fallback.
    let configure = Rc::new({
        let (maintainer, weak_area) = (maintainer.clone(), area.downgrade());
        move || {
            let (Some(area), child) = (weak_area.upgrade(), maintainer.child.get()) else {
                return;
            };
            let (x, y) = area_origin(&area).unwrap_or((0, 0));
            configure_child(&maintainer.conn, child, x, y, area.width(), area.height());
        }
    });
    {
        let configure = configure.clone();
        area.connect_resize(move |_, _, _| configure());
    }
    {
        let (attempt, configure) = (attempt.clone(), configure.clone());
        glib::timeout_add_local(CONFIGURE_POLL, move || {
            if !attempt.live() {
                return glib::ControlFlow::Break;
            }
            configure();
            glib::ControlFlow::Continue
        });
    }

    // Focus handoff: when X input focus sits on our parent, hand it to our
    // child. On teardown the polls stop and focus is left alone.
    {
        let (attempt, maintainer) = (attempt.clone(), maintainer.clone());
        glib::timeout_add_local(FOCUS_POLL, move || {
            if !attempt.live() {
                return glib::ControlFlow::Break;
            }
            hand_focus_to_child(&maintainer.conn, maintainer.host, maintainer.child.get());
            glib::ControlFlow::Continue
        });
    }
}

struct Maintainer {
    conn: x11rb::rust_connection::RustConnection,
    host: u32,
    child: Cell<u32>,
}

/// The doc-08 choice for Wayland-native GDK: relaunch under X11 or open
/// xfreerdp un-embedded. Never a silent second window.
fn show_wayland_choice(attempt: &Attempt) {
    let view = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    view.set_vexpand(true);
    view.set_hexpand(true);
    view.set_valign(gtk4::Align::Center);
    view.set_halign(gtk4::Align::Center);
    set_margins(&view, 24);
    let heading = gtk4::Label::new(Some("RDP embedding needs X11"));
    heading.add_css_class("heading");
    let detail = gtk4::Label::new(Some(
        "This display is Wayland-native, and xfreerdp can only embed into an \
         X11 window. Relaunch Wayterm under XWayland, or open this session \
         in a separate xfreerdp window.",
    ));
    detail.set_wrap(true);
    detail.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    detail.set_justify(gtk4::Justification::Center);
    let relaunch = gtk4::Button::with_label("Relaunch under GDK_BACKEND=x11");
    relaunch.add_css_class("suggested-action");
    relaunch.connect_clicked(|_| relaunch_under_x11());
    let separate = gtk4::Button::with_label("Open window instead");
    let separate_attempt = attempt.clone();
    separate.connect_clicked(move |_| {
        set_terminal_area(
            &separate_attempt.page,
            &centered_label("Running in a separate xfreerdp window…"),
        );
        publish_status(
            &separate_attempt.page,
            TransportState::Connecting,
            &separate_attempt.host(),
            separate_attempt.port(),
        );
        launch_dial(&separate_attempt, None, None);
    });
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    buttons.set_halign(gtk4::Align::Center);
    buttons.append(&relaunch);
    buttons.append(&separate);
    view.append(&heading);
    view.append(&detail);
    view.append(&buttons);
    set_terminal_area(&attempt.page, &view);
}

/// Respawn ourselves with `GDK_BACKEND=x11`, then exit this instance.
fn relaunch_under_x11() {
    if let Ok(exe) = std::env::current_exe() {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let _ = std::process::Command::new(exe)
            .args(args)
            .env("GDK_BACKEND", "x11")
            .spawn();
    }
    if let Some(app) =
        gtk4::gio::Application::default().and_then(|app| app.downcast::<gtk4::Application>().ok())
    {
        app.quit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::RdpOptions;

    fn session() -> SessionConfig {
        SessionConfig {
            id: 7,
            name: "rdp-prod".to_string(),
            host: Some(" rdp.example.lan ".to_string()),
            port: None,
            username: Some(" ada ".to_string()),
            rdp_domain: Some(" CORP ".to_string()),
            rdp_width: None,
            rdp_height: None,
            rdp_color_depth: Some(16),
            rdp_compression: true,
            rdp_options: None,
            ..Default::default()
        }
    }

    #[test]
    fn spec_maps_the_row_with_plan_defaults() {
        let spec = spec_from_session(&session(), None);
        assert_eq!(spec.host, "rdp.example.lan");
        assert_eq!(spec.port, 3389);
        assert_eq!(spec.username, "ada");
        assert_eq!(spec.domain.as_deref(), Some("CORP"));
        assert_eq!((spec.width, spec.height), (1024, 768));
        assert_eq!(spec.depth, Some(16));
        assert!(!spec.audio);
        assert!(spec.clipboard && spec.fonts && spec.printer && spec.compression);
        assert_eq!(spec.cert_arg, None);
    }

    #[test]
    fn spec_honors_explicit_options_and_geometry() {
        let mut session = session();
        session.port = Some(3390);
        session.rdp_width = Some(1920);
        session.rdp_height = Some(1080);
        session.rdp_color_depth = None;
        session.rdp_compression = false;
        session.rdp_options = Some(RdpOptions {
            audio: true,
            clipboard: false,
            fonts: false,
            printer: false,
        });
        let spec = spec_from_session(&session, Some("token".to_string()));
        assert_eq!((spec.port, spec.width, spec.height), (3390, 1920, 1080));
        assert_eq!(spec.depth, None);
        assert!(spec.audio);
        assert!(!spec.clipboard && !spec.fonts && !spec.printer && !spec.compression);
        assert_eq!(spec.cert_arg.as_deref(), Some("token"));
    }

    #[test]
    fn version_warning_fires_below_3_20() {
        for version in ["2.0.0", "3.5.1", "3.15.0", "3.19.9"] {
            assert!(warn_below(version), "{version}");
        }
        for version in ["3.20.0", "3.21.1", "4.0.0", "", "not-a-version", "3"] {
            assert!(!warn_below(version), "{version}");
        }
    }

    #[test]
    fn cert_texts_carry_endpoint_and_fingerprints() {
        let fp = "a".repeat(64);
        let unknown = unknown_cert_text("rdp.example.lan", 3389, &fp);
        assert!(unknown.contains("rdp.example.lan:3389"));
        assert!(unknown.contains(&fp));
        let changed = changed_cert_text("stored", "seen", "rdp.example.lan", 3390);
        assert!(changed.contains("rdp.example.lan:3390"));
        assert!(changed.contains("stored") && changed.contains("seen"));
    }

    #[test]
    fn newcomer_tracking_ignores_baseline_and_orphans() {
        assert_eq!(track_newcomer(&[], &[]), None);
        assert_eq!(track_newcomer(&[1, 2], &[1, 2]), None);
        assert_eq!(track_newcomer(&[], &[3, 5]), Some(5));
        assert_eq!(track_newcomer(&[1], &[1, 7]), Some(7));
        // An orphan in the baseline is never tracked, even when youngest.
        assert_eq!(track_newcomer(&[1, 9], &[1, 7, 9]), Some(7));
        assert_eq!(track_newcomer(&[1, 7, 9], &[1, 7, 9]), None);
    }

    #[test]
    fn ended_message_names_the_cause_and_tails_stderr() {
        assert_eq!(
            ended_message(Some(3), None, &[]),
            "The RDP session ended (exit code 3)."
        );
        assert_eq!(
            ended_message(None, Some(9), &[]),
            "The RDP session ended (signal 9)."
        );
        let digest = vec!["first".to_string(), "ERRCONNECT_LOGON_FAILURE".to_string()];
        let message = ended_message(Some(1), None, &digest);
        assert!(message.contains("exit code 1") && message.contains("ERRCONNECT_LOGON_FAILURE"));
    }
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    pub(in crate::ui) fn rdp_embed_builds_and_tracks() {
        // GLib criticals abort instead of scrolling by.
        // SAFETY: plain flag change on the process-wide GLib log mask.
        unsafe {
            glib::ffi::g_log_set_always_fatal(
                glib::ffi::G_LOG_LEVEL_CRITICAL | glib::ffi::G_LOG_LEVEL_WARNING,
            );
        }

        // The embed area installs no controllers at all: the desktop keeps
        // its keys, and in particular there is no Tier-2 shortcut
        // controller (doc 08 §Tabs).
        let area = build_area();
        assert!(area.is_focusable());
        let controllers = area.observe_controllers();
        assert_eq!(controllers.n_items(), 0, "no controllers, no Tier-2");

        // Unmapped: no XID yet, cleanly Unmapped (not Wayland).
        assert_eq!(surface_xid(&area), XidState::Unmapped);

        // Real X round-trip: present in a window and wait for the map.
        let window = gtk4::Window::new();
        window.set_child(Some(&area));
        window.present();
        let mut xid = None;
        for _ in 0..200 {
            while glib::MainContext::default().iteration(false) {}
            if let XidState::Ready(id) = surface_xid(&area) {
                xid = Some(id);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let host = xid.expect("DrawingArea maps to a real X window under xvfb");
        assert_ne!(host, 0);

        // The host tree round-trips over a real X connection. GTK keeps its
        // own children under the toplevel, so discovery is baseline-relative
        // (`track_newcomer`, unit-tested above): with no newcomers there is
        // nothing to track, and maintenance on nothing is a silent no-op.
        let (conn, _) = x11rb::connect(None).expect("x11rb connect under xvfb");
        let baseline = child_windows(&conn, host);
        assert_eq!(track_newcomer(&baseline, &baseline), None);
        let mut with_new = baseline.clone();
        with_new.push(0x0D_ECAF);
        assert_eq!(track_newcomer(&baseline, &with_new), Some(0x0D_ECAF));
        configure_child(&conn, 0, 0, 0, 800, 600);
        hand_focus_to_child(&conn, host, 0);
        window.close();
    }
}
