//! The RDP tab page (doc 08 §RDP): an X11-backed `DrawingArea` that
//! xfreerdp reparents into, and the TOFU connect flow.
//!
//! - **Connect** mirrors `vnc_tab::start`: validate the row, resolve the
//!   password and the server certificate (`rdp_trust`, a probe run on first
//!   contact), then spawn. Everything that blocks runs on the blocking pool
//!   in `dial`; the GTK thread builds widgets and pumps the runner's events.
//! - **Embedding:** poll for the area's XID, spawn with `/parent-window:`,
//!   then on one x11rb connection track only the child that appeared after
//!   our spawn (`track_newcomer`; GTK keeps children of its own there),
//!   keep it at the area's origin and size, and hand it X input focus —
//!   without that there is no keyboard under a window manager.
//! - **Teardown:** `EngineSlot` owns the process and its runner; close,
//!   retry or a superseded attempt terminates only our own process group.
//! - **Trust store:** read through a second [`SessionStore`] handle on the
//!   blocking thread (WAL makes it safe); the credential chain carries
//!   secrets only.
//! - **Wayland:** without an X11 surface the page offers a relaunch under
//!   `GDK_BACKEND=x11` or a separate xfreerdp window, never a silent one.
//! - **Version:** probed once (`cached_probe`); 2.x is refused, below
//!   3.20 the connecting overlay warns ([`warn_below`]).
//!
//! [`SessionStore`]: crate::store::SessionStore

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use x11rb::rust_connection::RustConnection;
use zeroize::Zeroizing;

use crate::rdp::cert::{parse_seen_fingerprint, PROBE_FINGERPRINT};
use crate::rdp::{
    cert_arg, decide, CertDecision, RdpProcess, RdpProcessEvent, RdpSpec, CERT_ARG_PREFIX,
};
use crate::store::model::SessionConfig;
use crate::transport::credentials::CredentialManager;
use crate::transport::types::TransportState;
use crate::ui::auth_dialog::run_on_gtk_main_thread;
use crate::ui::tab::{
    connecting_view, credential_manager, current_gen, filled, notice_view, page_store,
    publish_status, set_terminal_area, show_disconnected, show_error_view, show_failure, NO_HOST,
    NO_USERNAME,
};
use crate::ui::widgets::{centered_label, data_or_default, set_data};

mod preflight;
mod x11;

pub use preflight::*;
use x11::*;

/// The RDP default port (display `:0`-style: 3389).
pub const DEFAULT_PORT: u16 = 3389;
/// Password secret type in the credential chain (doc 05).
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
// Connect flow
// ---------------------------------------------------------------------------

type Proc = Arc<tokio::sync::Mutex<RdpProcess>>;

/// Everything one attempt needs on the GTK thread.
#[derive(Clone)]
struct Attempt {
    notebook: gtk4::Notebook,
    /// Weak: every timer and task of the attempt holds one, and a strong
    /// ref would keep a closed page alive — with its [`EngineSlot`], so our
    /// xfreerdp would outlive the tab (doc 03 §Lifecycle).
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
        show_failure(
            &self.notebook,
            page,
            &self.session,
            &message,
            &self.host,
            self.port,
        );
    }

    fn disconnected(&self, page: &gtk4::Widget) {
        show_disconnected(page, &self.host, self.port);
    }
}

fn rdp_gen_slot(page: &gtk4::Widget) -> Rc<Cell<u64>> {
    data_or_default(page, RDP_GEN_KEY)
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
        let tail = digest_tail(&digest).map_or_else(String::new, |tail| format!(" ({tail})"));
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
    let Some(host) = filled(&session.host) else {
        return show_error_view(notebook, page, session, NO_HOST);
    };
    if filled(&session.username).is_none() {
        return show_error_view(notebook, page, session, NO_USERNAME);
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
    let mut text = format!("Connecting to {}:{}…", attempt.host, attempt.port);
    if let Some(warning) = warning {
        text.push('\n');
        text.push_str(warning);
    }
    let attempt = attempt.clone();
    connecting_view(&text, move || {
        let Some(page) = attempt.page() else {
            return;
        };
        bump_rdp_gen(&page);
        retire_slot(&page);
        attempt.disconnected(&page);
    })
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
    let store = attempt.page().as_ref().and_then(page_store);
    let creds = credential_manager(attempt.session.id, &attempt.session.name, store.as_deref());
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

/// The plain-ended message: exit code/signal plus the stderr tail.
fn ended_message(code: Option<i32>, signal: Option<i32>, digest: &[String]) -> String {
    let ending = match (code, signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "an unknown cause".to_string(),
    };
    match digest_tail(digest) {
        Some(tail) => format!("The RDP session ended ({ending}): {tail}"),
        None => format!("The RDP session ended ({ending})."),
    }
}

/// The last three stderr lines, newest first, for an error message (they
/// never carry the password — it only travels on stdin); `None` when
/// xfreerdp printed nothing.
fn digest_tail(digest: &[String]) -> Option<String> {
    let tail: Vec<&str> = digest.iter().rev().take(3).map(String::as_str).collect();
    (!tail.is_empty()).then(|| tail.join(" | "))
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
    let view = notice_view(
        "RDP embedding needs X11",
        "This display is Wayland-native, and xfreerdp can only embed into an \
         X11 window. Relaunch Wayterm under XWayland, or open this session \
         in a separate xfreerdp window.",
        &[relaunch, separate],
    );
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
mod tests;

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    pub(in crate::ui) fn rdp_embed_builds_and_tracks() {
        // GLib criticals abort instead of scrolling by.
        let _fatal = crate::ui::gtk_tests::FatalWarnings::new();

        // The embed area installs no controllers at all: the desktop keeps
        // its keys, and in particular there is no shortcut controller of
        // its own (doc 08 §Tabs).
        let area = build_area();
        assert!(area.is_focusable());
        let controllers = area.observe_controllers();
        assert_eq!(controllers.n_items(), 0, "no controllers, no shortcuts");

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
