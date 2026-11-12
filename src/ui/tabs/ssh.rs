//! SSH tabs (doc 04): validate the row, show "Connecting…", get a shared
//! connection from the window's supervisor — directly or through the jump
//! host (doc 04 §Jump hosts) — while prompts run on the GTK thread and the
//! dial waits on a blocking-pool thread. Once connected, the page arms the
//! forward engine (doc 04 §Port forwarding) and attaches its SFTP sidebar
//! (doc 07) to the same connection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use vte4::prelude::*;

use super::terminal::finish_connect;
use super::views::{show_connecting_view, show_disconnected, show_error_view, show_failure};
use super::{
    credential_manager, current_gen, filled, page_store, publish_status, settle_redial, state,
    SessionTab, TabType, NO_HOST, NO_USERNAME,
};
use crate::sftp::ShellCwdProbe;
use crate::store::model::{ForwardRule, HostKeyPolicy, SessionConfig, SessionHostKey};
use crate::store::SessionStore;
#[cfg(feature = "backend-ghostty")]
use crate::terminal::ghostty::view::GhosttyView;
use crate::transport::connection::SshConnection;
use crate::transport::forward::{ForwardEngine, ForwardEvent};
use crate::transport::hostkey::{HostKeyVerifier, SessionKeySet};
use crate::transport::jump;
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth, Keepalive};
use crate::transport::types::{TransportError, TransportState};
use crate::ui::dialogs::auth;
use crate::ui::dialogs::hostkey;
use crate::ui::dialogs::tunnels::EngineStatusBridge;
use crate::ui::main_window::SUPERVISOR_KEY;
use crate::ui::panels::sftp_sidebar::{self, DirectoryFollower, SftpSidebar};
use crate::ui::panels::side_dock;
use crate::ui::widgets::{data, data_or_default, window_of};

/// Page data key: the page's [`SshPageState`].
const SSH_PAGE_KEY: &str = "wayterm-ssh-page";
/// Engine event channel capacity: status snapshots, never blocking.
const FWD_EVENT_BUF: usize = 64;
/// After Enter, how long the terminal must stay quiet before the SFTP
/// sidebar asks where the shell is (doc 07 §Following the shell).
const PROBE_QUIET: Duration = Duration::from_millis(300);

/// What an SSH page keeps beside its [`PageState`](super::PageState); a
/// local-shell page keeps its sidebar here too. Main thread only.
#[derive(Default)]
struct SshPageState {
    /// The page's sidebar, and whether it holds the connection (or, on a
    /// local page, browses for a running shell).
    sidebar: RefCell<Option<SftpSidebar>>,
    sidebar_attached: Cell<bool>,
    /// The live forward status source, while the engine is armed.
    forward: RefCell<Option<Arc<EngineStatusBridge>>>,
}

/// `page`'s SSH state, created on first use.
fn ssh_state(page: &gtk4::Widget) -> Rc<SshPageState> {
    data_or_default(page, SSH_PAGE_KEY)
}

/// `page`'s SSH state, if it has any.
fn existing_ssh_state(page: &gtk4::Widget) -> Option<Rc<SshPageState>> {
    data(page, SSH_PAGE_KEY)
}

/// `page`'s sidebar, if it is an SSH or local-shell page.
pub(super) fn sidebar_of(page: &gtk4::Widget) -> Option<SftpSidebar> {
    existing_ssh_state(page)?.sidebar.borrow().clone()
}

/// [`SessionTab::set_sidebar_for_page`].
pub(super) fn set_sidebar(page: &gtk4::Widget, sidebar: SftpSidebar) {
    ssh_state(page).sidebar.replace(Some(sidebar));
}

/// `page`'s live forward status source, while its engine is armed.
pub(super) fn forward_source(page: &gtk4::Widget) -> Option<Arc<EngineStatusBridge>> {
    existing_ssh_state(page)?.forward.borrow().clone()
}

/// A new page's sidebar: an SSH page's hidden while `sftp_on_connect` is
/// off, a `local` page's until the SFTP panel first shows it.
pub(super) fn install_sidebar(page: &gtk4::Widget, local: bool) {
    let sidebar = if local {
        SftpSidebar::local(false)
    } else if sftp_sidebar::open_on_connect() {
        SftpSidebar::new()
    } else {
        SftpSidebar::hidden()
    };
    set_sidebar(page, sidebar);
}

/// Dial `session` for attempt `gen` and finish connecting the page.
pub(super) fn start(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    gen: u64,
) {
    let Some(host) = filled(&session.host).map(str::to_string) else {
        return show_error_view(notebook, page, session, NO_HOST);
    };
    let Some(username) = filled(&session.username).map(str::to_string) else {
        return show_error_view(notebook, page, session, NO_USERNAME);
    };
    let port = session.port.unwrap_or(22);
    let Some(window) = window_of(page) else {
        return;
    };
    let Some(supervisor) = data::<Arc<ConnectionSupervisor>>(&window, SUPERVISOR_KEY) else {
        return show_error_view(
            notebook,
            page,
            session,
            "Connection supervisor unavailable.",
        );
    };

    show_connecting_view(page, &host, port);
    publish_status(page, TransportState::Connecting, &host, port);

    let store = page_store(page);
    let auth = hop_auth(session, store.as_deref());
    // Each hop's session key set and whose row it is: what the user
    // accepts at a changed-key prompt is saved back after the dial,
    // whatever its outcome (doc 04 §Host key verification).
    let mut trusted = vec![(session.id, auth.verifier.session_keys().clone())];
    let params = ConnectParams {
        host: host.clone(),
        port,
        username,
    };
    // Jump sessions dial through their bastion (doc 04 §Jump hosts): the
    // dial closure below is the only branch that differs, and everything
    // downstream receives the same `Arc<SshConnection>`.
    // The prompt closures block until the user answers, so the dial must
    // not occupy a reactor worker.
    let dial = if uses_jump_dial(session) {
        let jump_id = session.jump_session_id.expect("uses_jump_dial checked");
        let Some(jump_cfg) = resolve_jump_config(notebook, page, session, jump_id, &host, port)
        else {
            return;
        };
        // Each hop authenticates as its own session (doc 04).
        let jump_auth = hop_auth(&jump_cfg, store.as_deref());
        trusted.push((jump_cfg.id, jump_auth.verifier.session_keys().clone()));
        // Plain data: the blocking pool owns this copy (`Rc` is not `Send`).
        let target: SessionConfig = (**session).clone();
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(jump::dial_with_jump(
                &supervisor,
                &jump_cfg,
                &jump_auth,
                &target,
                &auth,
            ))
        })
    } else {
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(supervisor.get_or_connect(&params, &auth))
        })
    };

    let (notebook, page, session) = (notebook.clone(), page.clone(), session.clone());
    glib::MainContext::default().spawn_local(async move {
        let result = dial.await;
        // Before anything else: a key accepted for a session stays
        // accepted even when the dial then failed, was cancelled or was
        // superseded.
        save_accepted_keys(page_store(&page).as_deref(), &trusted);
        // Superseded or cancelled: a late connection is just dropped, which
        // releases it to the supervisor.
        if current_gen(&page) != gen {
            return;
        }
        let error = match result {
            Ok(Ok(conn)) => return finish_connect(&notebook, &session, &page, Some(conn)).await,
            // A cancelled prompt is a clean disconnect, not an error.
            Ok(Err(err @ (TransportError::Cancelled | TransportError::Disconnected))) => {
                settle_redial(&page, Err(err));
                return show_disconnected(&page, &host, port);
            }
            Ok(Err(err)) => err.to_string(),
            Err(join_err) => format!("connect task failed: {join_err}"),
        };
        show_failure(&notebook, &page, &session, &error, &host, port);
    });
}

/// How the hop `session` describes authenticates (doc 04): its stored
/// method, its own credential chain, a prompt-backed host-key check under
/// its own policy — else the global one, or strict when the page has no
/// store to read that from — trusting its own host keys, the challenge
/// dialog, and its stored keepalives.
fn hop_auth(session: &SessionConfig, store: Option<&SessionStore>) -> HopAuth {
    let policy = session
        .host_key_policy
        .clone()
        .unwrap_or_else(|| store.map_or(HostKeyPolicy::Strict, SessionStore::host_key_policy));
    let verifier = HostKeyVerifier::asking(policy, hostkey::host_key_prompt(), None)
        .with_session_keys(session_key_set(session, store));
    HopAuth {
        method: jump::hop_auth_method(session),
        creds: credential_manager(session.id, &session.name, store),
        verifier,
        responder: Some(auth::challenge_responder(session.name.clone())),
        keepalive: Keepalive::from_options(session.ssh_options.as_ref()),
    }
}

/// The host keys `session` trusts, read fresh from its row so a redial
/// after an acceptance never asks again; the tab's copy of the row only
/// when there is no store or no row to read.
fn session_key_set(session: &SessionConfig, store: Option<&SessionStore>) -> SessionKeySet {
    let stored: Option<Vec<SessionHostKey>> = match store.map(|s| s.get_session(session.id)) {
        Some(Ok(row)) => row.host_keys,
        _ => session.host_keys.clone(),
    };
    SessionKeySet::new(stored.unwrap_or_default())
}

/// Save to each hop's row the keys its session was told to trust during
/// the dial (doc 04 §Host key verification). Main thread only, like the
/// store.
fn save_accepted_keys(store: Option<&SessionStore>, trusted: &[(i64, SessionKeySet)]) {
    for (session_id, keys) in trusted {
        let added = keys.added();
        if added.is_empty() {
            continue;
        }
        let why = match store.map(|store| store.add_session_host_keys(*session_id, &added)) {
            Some(Ok(_)) => continue,
            Some(Err(err)) => err.to_string(),
            None => "no session store".to_string(),
        };
        eprintln!("Wayterm: the host key accepted for session {session_id} was not saved: {why}");
    }
}

/// Whether `session` dials through a bastion (doc 04 §Jump hosts): SSH
/// with a jump host set. VNC, RDP, local and serial dispatch elsewhere, so
/// this only ever fires for SSH.
fn uses_jump_dial(session: &SessionConfig) -> bool {
    TabType::of(&session.session_type) == TabType::Ssh && session.jump_session_id.is_some()
}

/// The jump host's config at dial time, through the page's store. A
/// deleted jump host (or a page with no store) is a connect error shown on
/// the page — the same `gen` guard and redial verdicts apply, since the
/// caller returns before spawning the dial.
fn resolve_jump_config(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    jump_id: i64,
    host: &str,
    port: u16,
) -> Option<SessionConfig> {
    let message = match page_store(page) {
        Some(store) => match resolve_jump_session(&store, jump_id) {
            Ok(jump_cfg) => return Some(jump_cfg),
            Err(message) => message,
        },
        None => "Jump session lookup unavailable.".to_string(),
    };
    show_failure(notebook, page, session, &message, host, port);
    None
}

/// Look one jump host up by id. `Err` names a deleted host; the store's
/// own `QueryReturnedNoRows` never reaches the UI.
fn resolve_jump_session(store: &SessionStore, jump_id: i64) -> Result<SessionConfig, String> {
    store
        .get_session(jump_id)
        .map_err(|_| format!("Jump session no longer exists (id {jump_id})."))
}

/// The connected half of the flow, after the terminal is wired: arm the
/// forward engine on every connect, so a redial re-establishes the same
/// rules on the fresh connection (doc 04 §Port forwarding), and attach the
/// sidebar to the same connection.
pub(super) fn on_connect(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    conn: Arc<SshConnection>,
    terminal: &gtk4::Widget,
) {
    arm_forward_engine(page, session, &conn);

    let Some(sidebar) = sidebar_of(page) else {
        return;
    };
    if session.jump_session_id.is_some() {
        // No SFTP task runs over a jumped connection (doc 04
        // §Limitations): the sidebar says so instead of attaching.
        sidebar.show_jump_placeholder();
    } else {
        sidebar.follow_new_shell(ShellCwdProbe::new(&conn, session.tmux_remote_name()));
        sidebar.attach_on_connect(conn);
        mark_sidebar_attached(page);
        follow_terminal_directory(page, terminal);
        // Like the terminal's focus: only a connect the user started
        // brings the SFTP panel forward, never a reconnect.
        if sftp_sidebar::open_on_connect() && state(page).focus_on_connect.get() {
            side_dock::reveal_sftp(notebook, page);
        }
    }
}

/// The rules the forward engine arms on `finish_connect`: the session's
/// current set, so every redial re-enters with the same rules and a
/// reconnect re-establishes them with no separate hook (doc 04).
fn forward_rules_for(session: &SessionConfig) -> Vec<ForwardRule> {
    session.forward_rules.clone().unwrap_or_default()
}

/// Arm the caller-owned forward engine for a fresh connection (doc 04
/// §Port forwarding): start the session's rules — the engine spawns its
/// listeners and never blocks — and bridge the event stream into the
/// page's live status source for the Tunnels dialog. Replaces any engine
/// still on the page, so a redial never stacks listeners.
fn arm_forward_engine(page: &gtk4::Widget, session: &Rc<SessionConfig>, conn: &Arc<SshConnection>) {
    stop_forward_engine(page);
    let (tx, rx) = mpsc::channel::<ForwardEvent>(FWD_EVENT_BUF);
    let engine = ForwardEngine::new(conn.clone(), tx);
    let bridge = bridge_events(engine.clone(), rx);
    ssh_state(page).forward.replace(Some(bridge));
    engine.start(forward_rules_for(session));
}

/// Wrap `engine` in a status bridge fed from `rx`. The feed holds the
/// bridge weakly: the bridge owns the engine, whose sender keeps `rx` open,
/// so a strong hold would keep the engine and its connection forever.
fn bridge_events(
    engine: ForwardEngine,
    mut rx: mpsc::Receiver<ForwardEvent>,
) -> Arc<EngineStatusBridge> {
    let bridge = Arc::new(EngineStatusBridge::new(engine));
    let weak = Arc::downgrade(&bridge);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let Some(bridge) = weak.upgrade() else { break };
            bridge.handle_event(event);
        }
    });
    bridge
}

/// Stop the page's forward engine, if any: listeners close and terminal
/// `Stopped` snapshots emit deterministically (a dropped engine would
/// abort its tasks anyway). Stopping clears the page key, so a
/// disconnected tab falls back to the dialog's offline state until the
/// next `finish_connect` arms a fresh engine.
pub(super) fn stop_forward_engine(page: &gtk4::Widget) {
    if let Some(bridge) = existing_ssh_state(page).and_then(|ssh| ssh.forward.take()) {
        bridge.engine().stop();
    }
}

/// End the sidebar's SFTP task, cancelling its transfers (or drop the
/// connection it keeps for its first show), and swap in a fresh sidebar
/// that keeps the old one's shown and following state, and the files it
/// has open in editors (doc 07 §Opening files).
pub(super) fn release_sidebar(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    let Some(ssh) = existing_ssh_state(page) else {
        return;
    };
    if !ssh.sidebar_attached.replace(false) {
        return;
    }
    let Some(old) = sidebar_of(page) else {
        return;
    };
    let sidebar = if old.is_local() {
        SftpSidebar::local(old.is_shown())
    } else if old.is_shown() {
        SftpSidebar::new()
    } else {
        SftpSidebar::hidden()
    };
    sidebar.adopt_open_files(&old);
    old.detach();
    sidebar.set_following(old.is_following());
    SessionTab::set_sidebar_for_page(page, sidebar);
    side_dock::sidebar_replaced(notebook);
}

/// The page is closing: end its sidebar's SFTP task now, as a transfer
/// still running would otherwise keep it, and its connection, going.
pub(super) fn detach_sidebar(page: &gtk4::Widget) {
    if let Some(sidebar) = sidebar_of(page) {
        sidebar.detach();
    }
}

/// `page`'s sidebar now browses for the connected session, until
/// [`release_sidebar`] lets it go.
pub(super) fn mark_sidebar_attached(page: &gtk4::Widget) {
    ssh_state(page).sidebar_attached.set(true);
}

/// Arm `page` with a forward engine whose opener never dials, as
/// [`on_connect`] arms a real one.
#[cfg(test)]
pub(super) fn arm_fake_forwards(page: &gtk4::Widget) {
    let opener: crate::transport::forward::ChannelOpener =
        Arc::new(|_, _| Box::pin(async { Err(TransportError::Cancelled) }));
    let (tx, _) = mpsc::channel(FWD_EVENT_BUF);
    let engine = ForwardEngine::with_opener(opener, tx);
    let bridge = Arc::new(EngineStatusBridge::new(engine));
    ssh_state(page).forward.replace(Some(bridge));
}

/// Keep the page's SFTP sidebar on the shell's working directory (doc 07
/// §Following the shell): the terminal's OSC 7 reports, and after each
/// command — Enter, then [`PROBE_QUIET`] without output — a probe. The tab
/// connects to the widget directly: VTE's signals, or the Ghostty view's
/// matching hooks. The sidebar is looked up each time, since a redial
/// replaces it. A local page takes reports from this machine only.
pub(super) fn follow_terminal_directory(page: &gtk4::Widget, terminal: &gtk4::Widget) {
    let (for_report, for_command) = (page.downgrade(), page.downgrade());
    let follower = if sidebar_of(page).is_some_and(|sidebar| sidebar.is_local()) {
        DirectoryFollower::local(&glib::host_name())
    } else {
        DirectoryFollower::default()
    };
    let shell = Rc::new(ShellWatch::new(
        PROBE_QUIET,
        follower,
        move |path| {
            if let Some(sidebar) = for_report.upgrade().and_then(|page| sidebar_of(&page)) {
                sidebar.follow_shell(path);
            }
        },
        move || {
            if let Some(sidebar) = for_command.upgrade().and_then(|page| sidebar_of(&page)) {
                sidebar.probe_shell();
            }
        },
    ));
    if let Some(vte) = terminal.downcast_ref::<vte4::Terminal>() {
        let (reports, inputs, outputs) = (shell.clone(), shell.clone(), shell.clone());
        vte.connect_current_directory_uri_changed(move |term| {
            if let Some(uri) = term.current_directory_uri() {
                reports.reported(&uri);
            }
        });
        vte.connect_commit(move |_, text, _| inputs.input(text.as_bytes()));
        vte.connect_contents_changed(move |_| outputs.output());
    }
    #[cfg(feature = "backend-ghostty")]
    if let Some(view) = terminal.downcast_ref::<GhosttyView>() {
        let (reports, inputs, outputs) = (shell.clone(), shell.clone(), shell.clone());
        view.connect_directory(move |uri| reports.reported(uri));
        view.connect_input(move |data| inputs.input(data));
        view.connect_output(move || outputs.output());
    }
}

/// One terminal's shell, watched for the sidebar: reports go through a
/// [`DirectoryFollower`] to `report`, and each command, Enter and then
/// `quiet` without output, calls `command`.
struct ShellWatch {
    quiet: Duration,
    follower: RefCell<DirectoryFollower>,
    report: Box<dyn Fn(String)>,
    command: Box<dyn Fn()>,
    /// Enter was typed; the output after it starts the quiet wait.
    armed: Cell<bool>,
    pending: RefCell<Option<glib::SourceId>>,
}

impl ShellWatch {
    fn new(
        quiet: Duration,
        follower: DirectoryFollower,
        report: impl Fn(String) + 'static,
        command: impl Fn() + 'static,
    ) -> Self {
        Self {
            quiet,
            follower: RefCell::new(follower),
            report: Box::new(report),
            command: Box::new(command),
            armed: Cell::new(false),
            pending: RefCell::default(),
        }
    }

    fn reported(&self, uri: &str) {
        let path = self.follower.borrow_mut().report(uri);
        if let Some(path) = path {
            (self.report)(path);
        }
    }

    fn input(&self, data: &[u8]) {
        if data.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
            self.armed.set(true);
        }
    }

    fn output(self: &Rc<Self>) {
        if !self.armed.get() {
            return;
        }
        if let Some(pending) = self.pending.take() {
            pending.remove();
        }
        let watch = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(self.quiet, move || {
            let Some(watch) = watch.upgrade() else {
                return;
            };
            // A fired source must not be removed again.
            watch.pending.take();
            watch.armed.set(false);
            (watch.command)();
        });
        *self.pending.borrow_mut() = Some(source);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::store::model::{ForwardKind, SessionType};
    use crate::ui::dialogs::auth::tests::acquire_default_ctx;

    #[test]
    fn a_command_is_probed_once_its_output_goes_quiet() {
        let _owned = acquire_default_ctx();
        let ctx = glib::MainContext::default();
        let quiet = Duration::from_millis(10);
        let spin = |until: &dyn Fn() -> bool, limit: Duration| {
            let end = Instant::now() + limit;
            while !until() && Instant::now() < end {
                ctx.iteration(false);
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let probes = Rc::new(Cell::new(0));
        let count = Rc::clone(&probes);
        let shell = Rc::new(ShellWatch::new(
            quiet,
            DirectoryFollower::default(),
            |_| {},
            move || count.set(count.get() + 1),
        ));

        shell.input(b"cd /tmp");
        shell.output();
        spin(&|| false, quiet * 5);
        assert_eq!(probes.get(), 0, "no Enter, no command");

        shell.input(b"\r");
        shell.output();
        shell.output();
        spin(&|| probes.get() > 0, Duration::from_secs(2));
        assert_eq!(probes.get(), 1);

        shell.output();
        spin(&|| false, quiet * 5);
        assert_eq!(probes.get(), 1, "output after the probe is no new command");
    }

    #[test]
    fn jump_dial_needs_ssh_with_a_jump_host() {
        let ssh_jumped = SessionConfig {
            session_type: SessionType::SSH,
            jump_session_id: Some(7),
            ..Default::default()
        };
        assert!(
            uses_jump_dial(&ssh_jumped),
            "an SSH session with a jump host dials through the bastion"
        );
        assert!(
            !uses_jump_dial(&SessionConfig {
                jump_session_id: None,
                ..ssh_jumped.clone()
            }),
            "a direct SSH session keeps the supervisor dial"
        );
        for session_type in [
            SessionType::RDP,
            SessionType::VNC,
            SessionType::Serial,
            SessionType::LocalShell,
        ] {
            assert!(
                !uses_jump_dial(&SessionConfig {
                    session_type,
                    jump_session_id: Some(7),
                    ..Default::default()
                }),
                "only SSH reaches the jump branch (other types dispatch elsewhere)"
            );
        }
    }

    #[test]
    fn forward_engine_arms_the_sessions_current_rules() {
        assert!(
            forward_rules_for(&SessionConfig::default()).is_empty(),
            "no stored rules is an empty engine set, not a missing one"
        );
        let rules = vec![ForwardRule {
            kind: ForwardKind::Local,
            bind_addr: "127.0.0.1".to_string(),
            bind_port: 8080,
            remote_host: "web1.lan".to_string(),
            remote_port: 80,
            enabled: true,
        }];
        let session = SessionConfig {
            forward_rules: Some(rules.clone()),
            ..Default::default()
        };
        assert_eq!(forward_rules_for(&session), rules);
    }

    #[tokio::test]
    async fn a_dropped_bridge_lets_its_engine_go() {
        let held = Arc::new(());
        let probe = Arc::downgrade(&held);
        let opener: crate::transport::forward::ChannelOpener = Arc::new(move |_, _| {
            let _held = &held;
            Box::pin(async { Err(TransportError::Cancelled) })
        });
        let (tx, rx) = mpsc::channel(FWD_EVENT_BUF);
        let bridge = bridge_events(ForwardEngine::with_opener(opener, tx), rx);
        tokio::task::yield_now().await;
        assert!(probe.upgrade().is_some());

        drop(bridge);
        tokio::task::yield_now().await;
        assert!(probe.upgrade().is_none(), "the engine outlived its bridge");
    }

    fn host_key(key: &str) -> SessionHostKey {
        SessionHostKey {
            key_type: "ssh-ed25519".to_string(),
            key: key.to_string(),
        }
    }

    /// A hop's verifier trusts the keys its row holds now, not those of the
    /// tab's older copy of the row, so a redial after an acceptance does
    /// not ask again; without a store or a row, the copy is all there is.
    #[test]
    fn a_hop_trusts_the_keys_its_row_holds_now() {
        let store = SessionStore::in_memory().expect("in-memory store");
        let stale = SessionConfig {
            name: "web".to_string(),
            host_keys: Some(vec![host_key("OLD")]),
            ..Default::default()
        };
        let id = store.insert_session(&stale).expect("insert");
        store
            .add_session_host_keys(id, &[host_key("NEW")])
            .expect("accept a key");
        let stale = SessionConfig { id, ..stale };

        assert_eq!(
            session_key_set(&stale, Some(&store)).keys(),
            [host_key("OLD"), host_key("NEW")]
        );
        assert_eq!(session_key_set(&stale, None).keys(), [host_key("OLD")]);
        let unsaved = SessionConfig {
            id: id + 1,
            ..stale.clone()
        };
        assert_eq!(
            session_key_set(&unsaved, Some(&store)).keys(),
            [host_key("OLD")]
        );
    }

    /// What each hop's session gained during the dial goes to that hop's
    /// own row — the jump host's to the jump host's — and a hop that
    /// gained nothing leaves its row alone.
    #[test]
    fn accepted_keys_are_saved_to_each_hops_own_row() {
        let store = SessionStore::in_memory().expect("in-memory store");
        let target = store
            .insert_session(&SessionConfig::default())
            .expect("insert target");
        let jump = store
            .insert_session(&SessionConfig::default())
            .expect("insert jump");
        let untouched = store
            .insert_session(&SessionConfig {
                host_keys: Some(vec![host_key("KEPT")]),
                ..Default::default()
            })
            .expect("insert");

        let target_keys = SessionKeySet::new([host_key("SEEDED")]);
        let jump_keys = SessionKeySet::default();
        target_keys.add(host_key("T"));
        jump_keys.add(host_key("J"));
        let quiet = SessionKeySet::new([host_key("KEPT")]);
        save_accepted_keys(
            Some(&store),
            &[(target, target_keys), (jump, jump_keys), (untouched, quiet)],
        );

        let keys = |id| store.get_session(id).expect("row").host_keys;
        assert_eq!(
            keys(target),
            Some(vec![host_key("T")]),
            "only what it gained"
        );
        assert_eq!(keys(jump), Some(vec![host_key("J")]));
        assert_eq!(keys(untouched), Some(vec![host_key("KEPT")]));
        // No store, or a deleted row: nothing to write to, no panic.
        let gained = SessionKeySet::default();
        gained.add(host_key("X"));
        save_accepted_keys(None, &[(target, gained.clone())]);
        save_accepted_keys(Some(&store), &[(target + 99, gained)]);
    }

    #[test]
    fn jump_resolution_reports_a_deleted_jump_host() {
        let store = SessionStore::in_memory().expect("in-memory store");
        let jump_id = store
            .insert_session(&SessionConfig {
                name: "bastion".to_string(),
                host: Some("bastion.test".to_string()),
                username: Some("jump".to_string()),
                ..Default::default()
            })
            .expect("insert jump host");
        let resolved = resolve_jump_session(&store, jump_id).expect("jump host exists");
        assert_eq!(resolved.name, "bastion");

        // A deleted jump host is a connect error naming the loss, never a
        // dial to the wrong place.
        store.delete_session(jump_id).expect("delete jump host");
        let err = resolve_jump_session(&store, jump_id).unwrap_err();
        assert!(
            err.contains("no longer exists"),
            "must name the loss, got: {err}"
        );
        assert!(err.contains(&jump_id.to_string()), "must name the id");
    }
}
