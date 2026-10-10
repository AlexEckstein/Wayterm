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
//! ## FreeRDP version
//!
//! The binary is probed once per process ([`cached_probe`]). FreeRDP 2.x is
//! refused with an error view ([`usable_freerdp`]): the argv speaks
//! FreeRDP 3 (`rdp::process`). `/from-stdin` was buggy around 3.5–3.17
//! (fixed in 3.20), so below 3.20 the connecting overlay carries a warning
//! ([`warn_below`], pure and tested); it disappears at Connected with the
//! overlay.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use x11rb::rust_connection::RustConnection;
use zeroize::Zeroizing;

use crate::app::default_db_path;
use crate::rdp::cert::{parse_seen_fingerprint, PROBE_FINGERPRINT};
use crate::rdp::{
    cert_arg, decide, is_supported_version, probe_binary, CertDecision, RdpProcess,
    RdpProcessEvent, RdpSpec, CERT_ARG_PREFIX, MIN_FREERDP_MAJOR,
};
use crate::store::model::SessionConfig;
use crate::store::SessionStore;
use crate::transport::credentials::CredentialManager;
use crate::transport::types::TransportState;
use crate::ui::auth_dialog::{run_on_gtk_main_thread, top_level_parent};
use crate::ui::tab::{
    credential_manager, current_gen, publish_status, set_terminal_area, show_error_view,
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
/// treats an unreadable store as first contact.
pub fn rdp_trust_lookup(host: &str, port: u16) -> rusqlite::Result<Option<String>> {
    SessionStore::open(&default_db_path())?.rdp_trust(host, port)
}

/// Record the accepted fingerprint for `(host, port)`.
pub fn rdp_trust_record(host: &str, port: u16, fingerprint: &str) -> rusqlite::Result<()> {
    SessionStore::open(&default_db_path())?.rdp_trust_record(host, port, fingerprint)
}

/// The probed `(binary, version)`, once per process. Blocking (one
/// `--version` spawn): only called from `start` and the dial pool.
fn cached_probe() -> Option<&'static (String, String)> {
    static PROBE: OnceLock<Option<(String, String)>> = OnceLock::new();
    PROBE.get_or_init(probe_binary).as_ref()
}

/// The probed `(binary, version)` if RDP can run on it: on `PATH`, and
/// FreeRDP 3 or later. `Err` carries the error view's message.
fn usable_freerdp() -> Result<&'static (String, String), String> {
    let probe = cached_probe()
        .ok_or_else(|| "xfreerdp not found on PATH (looked for xfreerdp3, xfreerdp)".to_string())?;
    let (binary, version) = probe;
    if is_supported_version(version) {
        Ok(probe)
    } else {
        Err(freerdp_too_old_text(binary, version))
    }
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
fn child_windows(conn: &RustConnection, host: u32) -> Vec<u32> {
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
fn configure_child(conn: &RustConnection, child: u32, x: i32, y: i32, width: i32, height: i32) {
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

/// Map or unmap our tracked child. GTK knows nothing of the foreign
/// window, so when the area is hidden (another tab selected, the window
/// sent to the tray, the tab closed) the remote desktop would otherwise
/// stay drawn over whatever the toplevel shows next.
fn set_child_mapped(conn: &RustConnection, child: u32, mapped: bool) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    let _ = if mapped {
        conn.map_window(child)
    } else {
        conn.unmap_window(child)
    };
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
fn hand_focus_to_child(conn: &RustConnection, host: u32, child: u32) {
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
    /// Weak: every timer and task of the attempt holds one, and a strong
    /// ref would keep a closed page alive — with its [`EngineSlot`], so our
    /// xfreerdp would outlive the tab (doc 04 §Lifecycle).
    page: glib::WeakRef<gtk4::Widget>,
    session: Rc<SessionConfig>,
    /// Trimmed, as the dial uses it.
    host: Rc<str>,
    port: u16,
    tab_gen: u64,
    our_gen: u64,
}

impl Attempt {
    /// The page, while it exists and this is still its newest attempt.
    fn page(&self) -> Option<gtk4::Widget> {
        self.page
            .upgrade()
            .filter(|page| current_gen(page) == self.tab_gen && rdp_gen_value(page) == self.our_gen)
    }

    fn live(&self) -> bool {
        self.page().is_some()
    }

    fn publish(&self, page: &gtk4::Widget, state: TransportState) {
        publish_status(page, state, &self.host, self.port);
    }

    fn fail(&self, page: &gtk4::Widget, message: String) {
        show_error_view(&self.notebook, page, &self.session, &message);
        self.publish(page, TransportState::Error(message));
    }

    fn disconnected(&self, page: &gtk4::Widget) {
        set_terminal_area(page, &centered_label("Disconnected"));
        self.publish(page, TransportState::Disconnected);
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

/// Abort the runner and terminate our own child. The runner holds the
/// process lock for the whole session, so it is aborted first (see
/// `RdpProcess`). When no runtime handle is reachable (off-thread drop in
/// tests), the child's `kill_on_drop` is the backstop: dropping the last
/// `Arc` reaps it.
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
    /// The fingerprint the spawn was pinned to (for the changed-cert check
    /// on pre-connect failures).
    pinned: String,
}

/// Resolve the password, then the certificate, then spawn. Blocks on the
/// prompts and the probe, so it runs on the blocking pool. `base` is the
/// session's spec without a certificate token.
fn dial(
    base: RdpSpec,
    session_id: &str,
    parent_xid: Option<u32>,
    creds: &CredentialManager,
) -> Dial {
    let Some(password) = creds.lookup(session_id, PASSWORD_SECRET) else {
        return Dial::Cancelled;
    };
    let (binary, _) = match usable_freerdp() {
        Ok(probe) => probe,
        Err(message) => return Dial::Failed(message),
    };
    // Fail closed: an unreadable store is not first contact. Treating it as
    // one would re-prompt for — and let the user re-pin — a host whose
    // stored pin we merely failed to read.
    let stored = match rdp_trust_lookup(&base.host, base.port) {
        Ok(stored) => stored,
        Err(err) => return Dial::Failed(format!("RDP trust store unreadable: {err}")),
    };
    let pinned = match stored {
        // Known host: pass-through spawn, xfreerdp enforces the pin.
        Some(fingerprint) => fingerprint,
        None => match first_contact(binary, &base, parent_xid) {
            Ok(fingerprint) => fingerprint,
            Err(dial) => return dial,
        },
    };
    let spec = RdpSpec {
        cert_arg: cert_arg(&CertDecision::PassStored {
            fingerprint: pinned.clone(),
        }),
        ..base
    };
    spawn_real(binary, &spec, parent_xid, &password, pinned)
}

/// First contact: learn the server's fingerprint without trusting it, then
/// ask the user. The probe pins a fingerprint no certificate can match, so
/// xfreerdp refuses at TLS — after printing what it saw, before any
/// credential could leave — writes nothing to its own store, and exits
/// (~1 s on 3.15, ~0.1 s on 3.32). It is fed an empty password regardless:
/// nothing secret goes to a server the user has not accepted, whatever the
/// peer negotiates. Returns the accepted (and recorded) fingerprint.
fn first_contact(binary: &str, base: &RdpSpec, parent_xid: Option<u32>) -> Result<String, Dial> {
    let (host, port) = (base.host.as_str(), base.port);
    let probe_spec = RdpSpec {
        cert_arg: Some(format!("{CERT_ARG_PREFIX}{PROBE_FINGERPRINT}")),
        ..base.clone()
    };
    let mut probe = RdpProcess::new(binary, &probe_spec, parent_xid);
    let (tx, _) = mpsc::channel(8);
    let runtime = tokio::runtime::Handle::current();
    let outcome = runtime.block_on(tokio::time::timeout(
        PROBE_TIMEOUT,
        probe.run(&Zeroizing::default(), tx),
    ));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(Dial::Failed(format!("certificate probe failed: {err}"))),
        Err(_) => {
            runtime.block_on(probe.terminate());
            return Err(Dial::Failed(format!(
                "certificate probe for {host}:{port} timed out"
            )));
        }
    }
    let digest = probe.error_digest();
    let Some(seen) = parse_seen_fingerprint(&digest) else {
        let tail: Vec<&str> = digest.iter().rev().take(3).map(String::as_str).collect();
        let tail = if tail.is_empty() {
            String::new()
        } else {
            format!(" ({})", tail.join(" | "))
        };
        return Err(Dial::Failed(format!(
            "could not read the certificate fingerprint of {host}:{port}{tail}"
        )));
    };
    let dialog_host = host.to_string();
    let accepted = run_on_gtk_main_thread(move || unknown_cert_decision(&dialog_host, port, &seen))
        .flatten()
        .ok_or(Dial::Cancelled)?;
    if let Err(err) = rdp_trust_record(&accepted.host, accepted.port, &accepted.fingerprint) {
        eprintln!("Wayterm: RDP trust record failed: {err}");
    }
    Ok(accepted.fingerprint)
}

/// Spawn the real session: the runner task owns a process lock guard only
/// across `run`, and the handle triple returns to the GTK thread.
fn spawn_real(
    binary: &str,
    spec: &RdpSpec,
    parent_xid: Option<u32>,
    password: &Zeroizing<Vec<u8>>,
    pinned: String,
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
        pinned,
    })
}

/// The embedded widgets an attempt spawned into, if any. `baseline` is the
/// host tree snapshot taken before OUR spawn, so discovery tracks only the
/// newcomer (see [`track_newcomer`]); `conn` took it and runs maintenance
/// (`None` when the display refused the connection).
struct Embed {
    area: gtk4::DrawingArea,
    overlay: gtk4::Overlay,
    status: gtk4::Widget,
    host_xid: u32,
    conn: Option<RustConnection>,
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
    let our_gen = bump_rdp_gen(page);
    fn filled(value: &Option<String>) -> Option<&str> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
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
    let (_, version) = match usable_freerdp() {
        Ok(probe) => probe,
        Err(message) => return show_error_view(notebook, page, session, &message),
    };
    let attempt = Attempt {
        notebook: notebook.clone(),
        page: page.downgrade(),
        session: session.clone(),
        host: host.into(),
        port: session.port.unwrap_or(DEFAULT_PORT),
        tab_gen: gen,
        our_gen,
    };
    // The FreeRDP version notice lives in the connecting overlay and
    // disappears at Connected with it.
    let warning = warn_below(version).then(|| {
        format!(
            "FreeRDP {version} — credential pipe buggy band (fixed 3.20); \
             if the password is rejected, check stdin auth"
        )
    });
    attempt.publish(page, TransportState::Connecting);

    let area = build_area();
    let overlay = gtk4::Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.set_child(Some(&area));
    let status = connecting_overlay(&attempt, warning.as_deref());
    overlay.add_overlay(&status);
    set_terminal_area(page, &overlay);
    wait_for_xid(attempt, area, overlay, status);
}

/// The connecting overlay: host:port, the optional FreeRDP warning, and a
/// Cancel button. Removed at Connected; any error view replaces the whole
/// overlay.
fn connecting_overlay(attempt: &Attempt, warning: Option<&str>) -> gtk4::Widget {
    let view = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    view.set_vexpand(true);
    view.set_hexpand(true);
    view.set_valign(gtk4::Align::Center);
    view.set_halign(gtk4::Align::Center);
    let mut text = format!("Connecting to {}:{}…", attempt.host, attempt.port);
    if let Some(warning) = warning {
        text.push('\n');
        text.push_str(warning);
    }
    view.append(&centered_label(&text));
    let cancel = gtk4::Button::with_label("Cancel");
    cancel.set_halign(gtk4::Align::Center);
    let attempt = attempt.clone();
    cancel.connect_clicked(move |_| {
        let Some(page) = attempt.page() else {
            return;
        };
        bump_rdp_gen(&page);
        retire_slot(&page);
        attempt.disconnected(&page);
    });
    view.append(&cancel);
    view.upcast()
}

/// Poll for the area's XID, then dial; give up after [`XID_POLLS_MAX`]
/// polls while unmapped.
fn wait_for_xid(
    attempt: Attempt,
    area: gtk4::DrawingArea,
    overlay: gtk4::Overlay,
    status: gtk4::Widget,
) {
    let weak_area = area.downgrade();
    let mut parts = Some((overlay, status));
    let mut ticks = 0;
    glib::timeout_add_local(XID_POLL, move || {
        let (Some(page), Some(area)) = (attempt.page(), weak_area.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        match surface_xid(&area) {
            XidState::Ready(xid) => {
                let Some((overlay, status)) = parts.take() else {
                    return glib::ControlFlow::Break;
                };
                // Baseline BEFORE the spawn: discovery later tracks only
                // the newcomer, never GTK's own children or an orphan.
                let conn = x11rb::connect(None).ok().map(|(conn, _)| conn);
                let baseline = conn
                    .as_ref()
                    .map(|conn| child_windows(conn, xid))
                    .unwrap_or_default();
                launch_dial(
                    &attempt,
                    Some(xid),
                    Some(Embed {
                        area,
                        overlay,
                        status,
                        host_xid: xid,
                        conn,
                        baseline,
                    }),
                );
                glib::ControlFlow::Break
            }
            XidState::Wayland => {
                show_wayland_choice(&attempt, &page);
                glib::ControlFlow::Break
            }
            XidState::Unmapped if ticks >= XID_POLLS_MAX => {
                attempt.fail(
                    &page,
                    "The RDP container never mapped to a window.".to_string(),
                );
                glib::ControlFlow::Break
            }
            XidState::Unmapped => {
                ticks += 1;
                glib::ControlFlow::Continue
            }
        }
    });
}

/// Run the dial on the blocking pool and attach its result.
fn launch_dial(attempt: &Attempt, parent_xid: Option<u32>, embed: Option<Embed>) {
    let creds = credential_manager(&attempt.session.name);
    let session_id = attempt.session.id.to_string();
    let base = spec_from_session(&attempt.session, None);
    let task = tokio::task::spawn_blocking(move || dial(base, &session_id, parent_xid, &creds));
    let attempt = attempt.clone();
    glib::MainContext::default().spawn_local(async move {
        let result = task.await;
        let Some(page) = attempt.page() else {
            // Superseded or closed: a late process must not leak its child.
            if let Ok(Dial::Ready(ready)) = result {
                dispose_process(ready.proc, ready.runner);
            }
            return;
        };
        match result {
            Ok(Dial::Ready(ready)) => attach(&attempt, &page, ready, embed),
            Ok(Dial::Cancelled) => attempt.disconnected(&page),
            Ok(Dial::Failed(error)) => attempt.fail(&page, error),
            Err(join_err) => attempt.fail(&page, format!("connect task failed: {join_err}")),
        }
    });
}

/// Show the framebuffer's host area, start X11 maintenance, and pump the
/// runner's events into the page.
fn attach(attempt: &Attempt, page: &gtk4::Widget, ready: ReadyDial, embed: Option<Embed>) {
    let ReadyDial {
        proc,
        runner,
        mut events,
        pinned,
    } = ready;
    set_data(
        page,
        ENGINE_KEY,
        Rc::new(EngineSlot::with_runner(proc.clone(), runner)),
    );
    let overlay = match embed {
        Some(embed) => {
            start_maintenance(
                attempt,
                &embed.area,
                embed.conn,
                embed.host_xid,
                embed.baseline,
            );
            Some((embed.overlay, embed.status))
        }
        None => {
            // A top-level window has no host area: say where the desktop went.
            set_terminal_area(
                page,
                &centered_label("Running in a separate xfreerdp window…"),
            );
            None
        }
    };
    let attempt = attempt.clone();
    glib::MainContext::default().spawn_local(async move {
        let mut connected = false;
        while let Some(event) = events.recv().await {
            let Some(page) = attempt.page() else {
                return;
            };
            match event {
                RdpProcessEvent::Connected => {
                    connected = true;
                    match &overlay {
                        Some((overlay, status)) => overlay.remove_overlay(status),
                        None => set_terminal_area(
                            &page,
                            &centered_label(
                                "Connected — the desktop is in the separate xfreerdp window.",
                            ),
                        ),
                    }
                    attempt.publish(&page, TransportState::Connected);
                }
                RdpProcessEvent::Ended { code, signal } => {
                    let digest = proc.lock().await.error_digest();
                    // A pre-connect failure may be a certificate change:
                    // xfreerdp enforces the pin and prints the seen
                    // fingerprint on stderr.
                    let seen = (!connected)
                        .then(|| parse_seen_fingerprint(&digest))
                        .flatten();
                    if let Some(CertDecision::ChangedCert { stored, seen }) =
                        seen.map(|seen| decide(Some(pinned.as_str()), Some(seen.as_str())))
                    {
                        changed_cert(&attempt, &stored, &seen);
                    } else {
                        attempt.fail(&page, ended_message(code, signal, &digest));
                    }
                    return;
                }
            }
        }
    });
}

/// The pinned connect met a different certificate: warn, and only an
/// explicit update records it and retries.
fn changed_cert(attempt: &Attempt, stored: &str, seen: &str) {
    let choice = changed_cert_decision(&attempt.host, attempt.port, stored, seen);
    // The modal ran a nested main loop: the tab may be closed or retried.
    let Some(page) = attempt.page() else {
        return;
    };
    match choice {
        ChangeChoice::UpdateAndRetry => {
            if let Err(err) = rdp_trust_record(&attempt.host, attempt.port, seen) {
                eprintln!("Wayterm: RDP trust record failed: {err}");
            }
            start(&attempt.notebook, &page, &attempt.session, attempt.tab_gen);
        }
        ChangeChoice::KeepRefusing => attempt.fail(
            &page,
            "server certificate changed; connection refused".to_string(),
        ),
    }
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

/// Start child discovery, visibility, configure, and focus maintenance for
/// our tracked window. Every callback stops itself once the attempt is
/// superseded or its page is gone.
fn start_maintenance(
    attempt: &Attempt,
    area: &gtk4::DrawingArea,
    conn: Option<RustConnection>,
    host: u32,
    baseline: Vec<u32>,
) {
    let Some(conn) = conn else {
        eprintln!("Wayterm: RDP x11rb connect failed; the child window will not be resized");
        return;
    };
    let maintainer = Rc::new(Maintainer {
        conn,
        host,
        child: Cell::new(0),
        shown: Cell::new(false),
    });

    // Discovery: once ~1 s after spawn, once more ~3 s (a late mapper or a
    // retry's second child). Only the newcomer past the pre-spawn baseline
    // is ever tracked; the second pass additionally replaces the tracked
    // window when it died.
    let baseline: Rc<[u32]> = baseline.into();
    for delay in [DISCOVER_FIRST, DISCOVER_SECOND] {
        let (attempt, maintainer, baseline, weak_area) = (
            attempt.clone(),
            maintainer.clone(),
            baseline.clone(),
            area.downgrade(),
        );
        glib::timeout_add_local_once(delay, move || {
            let (true, Some(area)) = (attempt.live(), weak_area.upgrade()) else {
                return;
            };
            let children = child_windows(&maintainer.conn, maintainer.host);
            let Some(newcomer) = track_newcomer(&baseline, &children) else {
                return;
            };
            let known = maintainer.child.get();
            if delay == DISCOVER_FIRST || known == 0 || !children.contains(&known) {
                maintainer.track(newcomer);
                maintainer.sync(&area, area.is_mapped());
            }
        });
    }

    // Follow the area: hide with it, show with it, fit it on resize, plus a
    // 250 ms poll fallback.
    {
        let maintainer = maintainer.clone();
        area.connect_map(move |area| maintainer.sync(area, true));
    }
    {
        let maintainer = maintainer.clone();
        area.connect_unmap(move |area| maintainer.sync(area, false));
    }
    {
        let maintainer = maintainer.clone();
        area.connect_resize(move |area, _, _| maintainer.sync(area, area.is_mapped()));
    }
    {
        let (attempt, maintainer, weak_area) =
            (attempt.clone(), maintainer.clone(), area.downgrade());
        glib::timeout_add_local(CONFIGURE_POLL, move || {
            let (true, Some(area)) = (attempt.live(), weak_area.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            maintainer.sync(&area, area.is_mapped());
            glib::ControlFlow::Continue
        });
    }

    // Focus handoff: when X input focus sits on our parent, hand it to our
    // child — only while the area is on screen. On teardown the polls stop
    // and focus is left alone.
    let (attempt, weak_area) = (attempt.clone(), area.downgrade());
    glib::timeout_add_local(FOCUS_POLL, move || {
        let (true, Some(area)) = (attempt.live(), weak_area.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        maintainer.hand_focus(&area);
        glib::ControlFlow::Continue
    });
}

/// The x11rb side of one embedded attempt: the host window, our tracked
/// child, and whether we last left that child mapped.
struct Maintainer {
    conn: RustConnection,
    host: u32,
    child: Cell<u32>,
    shown: Cell<bool>,
}

impl Maintainer {
    /// Track `child`; xfreerdp maps its own window when it creates it.
    fn track(&self, child: u32) {
        self.child.set(child);
        self.shown.set(true);
    }

    /// Mirror the area onto our child: fitted to the area while `visible`,
    /// unmapped while it is not.
    fn sync(&self, area: &gtk4::DrawingArea, visible: bool) {
        let child = self.child.get();
        if child == 0 {
            return;
        }
        if visible {
            let (x, y) = area_origin(area).unwrap_or((0, 0));
            configure_child(&self.conn, child, x, y, area.width(), area.height());
        }
        if visible != self.shown.get() {
            set_child_mapped(&self.conn, child, visible);
            self.shown.set(visible);
        }
    }

    /// The keyboard follows the desktop only while it is on screen: a
    /// hidden RDP tab must never take the keys typed into another tab.
    fn hand_focus(&self, area: &gtk4::DrawingArea) {
        if area.is_mapped() {
            hand_focus_to_child(&self.conn, self.host, self.child.get());
        }
    }
}

/// The doc-08 choice for Wayland-native GDK: relaunch under X11 or open
/// xfreerdp un-embedded. Never a silent second window.
fn show_wayland_choice(attempt: &Attempt, page: &gtk4::Widget) {
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
    let attempt = attempt.clone();
    separate.connect_clicked(move |_| {
        let Some(page) = attempt.page() else {
            return;
        };
        set_terminal_area(
            &page,
            &centered_label("Running in a separate xfreerdp window…"),
        );
        attempt.publish(&page, TransportState::Connecting);
        launch_dial(&attempt, None, None);
    });
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    buttons.set_halign(gtk4::Align::Center);
    buttons.append(&relaunch);
    buttons.append(&separate);
    view.append(&heading);
    view.append(&detail);
    view.append(&buttons);
    set_terminal_area(page, &view);
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
        let (window, host) = present(&area);

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

    /// Present `area` in a window and wait for the map; returns the window
    /// and the host XID.
    fn present(area: &gtk4::DrawingArea) -> (gtk4::Window, u32) {
        let window = gtk4::Window::new();
        window.set_child(Some(area));
        window.present();
        for _ in 0..200 {
            pump();
            if let XidState::Ready(xid) = surface_xid(area) {
                return (window, xid);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("DrawingArea maps to a real X window under xvfb");
    }

    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    pub(in crate::ui) fn rdp_child_follows_the_area_and_dies_with_the_page() {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            ConnectionExt, CreateWindowAux, InputFocus, MapState, WindowClass,
        };

        let area = build_area();
        area.set_size_request(320, 200);
        let (window, host) = present(&area);

        // A stand-in for xfreerdp's window: a plain child of the toplevel.
        let (conn, _) = x11rb::connect(None).expect("x11rb connect under xvfb");
        let child = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            child,
            host,
            0,
            0,
            10,
            10,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )
        .unwrap();
        conn.map_window(child).unwrap();
        conn.flush().unwrap();
        let maintainer = Maintainer {
            conn,
            host,
            child: Cell::new(0),
            shown: Cell::new(false),
        };
        maintainer.track(child);
        let state = |m: &Maintainer| {
            m.conn
                .get_window_attributes(child)
                .unwrap()
                .reply()
                .unwrap()
                .map_state
        };
        let focus = |m: &Maintainer| m.conn.get_input_focus().unwrap().reply().unwrap().focus;

        // Shown: fitted to the area.
        maintainer.sync(&area, area.is_mapped());
        let geometry = maintainer
            .conn
            .get_geometry(child)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(
            (i32::from(geometry.width), i32::from(geometry.height)),
            (area.width(), area.height())
        );
        assert_eq!(state(&maintainer), MapState::VIEWABLE);

        // Hidden (another tab selected): the keyboard stays where it is —
        // checked while the child is still viewable, since X itself
        // refuses focus to an unmapped window — and the child unmaps.
        area.set_visible(false);
        pump();
        assert!(!area.is_mapped());
        maintainer
            .conn
            .set_input_focus(InputFocus::PARENT, host, x11rb::CURRENT_TIME)
            .unwrap();
        assert_eq!(state(&maintainer), MapState::VIEWABLE);
        maintainer.hand_focus(&area);
        assert_eq!(focus(&maintainer), host, "a hidden tab takes no keys");
        maintainer.sync(&area, area.is_mapped());
        assert_eq!(state(&maintainer), MapState::UNMAPPED);

        // Shown again: mapped, and the keyboard follows the desktop.
        area.set_visible(true);
        pump();
        maintainer.sync(&area, area.is_mapped());
        assert_eq!(state(&maintainer), MapState::VIEWABLE);
        maintainer.hand_focus(&area);
        assert_eq!(focus(&maintainer), child);
        window.close();

        // An attempt holds its page weakly: once the tab is closed, every
        // timer and task of the attempt stops (and the page's EngineSlot,
        // finalized with it, terminates xfreerdp).
        let notebook = gtk4::Notebook::new();
        let page: gtk4::Widget = gtk4::Paned::new(gtk4::Orientation::Horizontal).upcast();
        notebook.append_page(&page, None::<&gtk4::Widget>);
        let attempt = Attempt {
            notebook: notebook.clone(),
            page: page.downgrade(),
            session: Rc::new(SessionConfig::default()),
            host: "rdp.example.lan".into(),
            port: DEFAULT_PORT,
            tab_gen: current_gen(&page),
            our_gen: bump_rdp_gen(&page),
        };
        assert!(attempt.live());
        bump_rdp_gen(&page);
        assert!(!attempt.live(), "a retry supersedes the attempt");
        let attempt = Attempt {
            our_gen: rdp_gen_value(&page),
            ..attempt
        };
        assert!(attempt.live());
        notebook.remove_page(Some(0));
        drop(page);
        assert!(!attempt.live(), "a closed tab ends the attempt");
    }
}
