//! Session tabs and the SSH connect flow (doc 04).
//!
//! A tab's page holds the session's area: a terminal for SSH, local and
//! serial, the remote desktop for VNC and RDP (`vnc_tab`, `rdp_tab`). An
//! SSH page also owns an SFTP sidebar (doc 09) — per tab, since each
//! browses its own connection — which the side dock's SFTP panel shows
//! while the tab is current (`side_dock`).
//!
//! Connecting: validate the row, show "Connecting…", get a shared
//! connection from the window's supervisor (prompts run on the GTK thread
//! while the dial waits on a blocking-pool thread), then measure the real
//! terminal grid, wire the [`TerminalTab`], and attach the sidebar to the
//! same connection. Every attempt bumps a per-page generation, so a
//! cancelled or superseded attempt discards its result.
//!
//! Reconnecting (doc 04 §Lifecycle): every streaming tab registers as a
//! [`LiveTab`](tab_resilience::LiveTab) whose connect closure redials
//! through the same machinery. One state drain per page feeds the registry,
//! the status bar and the disconnect alert; on `ConnectionLost` it starts
//! the reconnect driver per policy. `SessionEnded` (the shell exited) never
//! reconnects by itself: the terminal shows the `session_ended` prompt.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender, WeakUnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use vte4::prelude::*;
use zeroize::Zeroizing;

use crate::macros::InputTap;
use crate::sftp::ShellCwdProbe;
use crate::store::model::{ForwardRule, HostKeyPolicy, SessionConfig, SessionType};
use crate::store::SessionStore;
use crate::terminal::{create_terminal, TerminalBackend};
use crate::transport::connection::SshConnection;
use crate::transport::credentials::{
    BitwardenSource, CredentialManager, CredentialSource, KeyringSource, PromptSource,
};
use crate::transport::forward::{ForwardEngine, ForwardEvent};
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::jump;
use crate::transport::reconnect::{ConnectFn, ConnectFuture};
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth};
use crate::transport::types::{TransportError, TransportEvent, TransportState};
use crate::ui::auth_dialog::{self, ChallengePrompt};
use crate::ui::hostkey_dialog;
use crate::ui::main_window::{route_status, SUPERVISOR_KEY};
use crate::ui::rdp_tab;
use crate::ui::session_ended::{self, EndedChoice};
use crate::ui::settings::credentials::CredentialSettings;
use crate::ui::sftp_sidebar::{self, supports_sftp, DirectoryFollower, SftpSidebar};
use crate::ui::side_dock;
use crate::ui::tab_label;
use crate::ui::tab_resilience;
use crate::ui::terminal_tab::TerminalTab;
use crate::ui::tray;
use crate::ui::tunnels_dialog::{EngineStatusBridge, ForwardStatusSource};
use crate::ui::vnc_tab;
use crate::ui::widgets::{centered_label, data, data_or_default, set_data, set_margins, window_of};

/// Page data key: the tab's [`PageState`].
const PAGE_KEY: &str = "wayterm-session-page";
/// Engine event channel capacity: status snapshots, never blocking.
const FWD_EVENT_BUF: usize = 64;
// Notebook data key.
const TEARDOWN_HOOK_KEY: &str = "wayterm-teardown-hook";
/// The next page's tab id ([`PageState::tab_id`]).
static NEXT_TAB_ID: AtomicI64 = AtomicI64::new(1);

/// An unallocated VTE reports 24×80, so measuring waits (≈2 s at most)
/// for the widget to get a size.
const ALLOC_WAIT_STEPS: u32 = 125;
/// After Enter, how long the terminal must stay quiet before the SFTP
/// sidebar asks where the shell is (doc 09 §Following the shell).
const PROBE_QUIET: Duration = Duration::from_millis(300);

/// The last state a page published, replayed into the status bar when the
/// page becomes current.
#[derive(Clone, Debug)]
pub(crate) struct PageStatus {
    pub(crate) state: TransportState,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Default for PageStatus {
    fn default() -> Self {
        Self {
            state: TransportState::Disconnected,
            host: String::new(),
            port: 0,
        }
    }
}

/// A driver-driven redial waiting for its attempt's verdict.
type RedialReply = oneshot::Sender<Result<(), TransportError>>;

/// Everything a session tab keeps about itself, held once on its page
/// widget ([`state`]). Main thread only.
#[derive(Default)]
struct PageState {
    /// The session the tab shows ([`page_content`]).
    session: OnceCell<Rc<SessionConfig>>,
    /// Unique per page, unlike the session id: one session can have
    /// several tabs open. The resilience registry's key.
    tab_id: Cell<i64>,
    /// Where the connect flow resolves the jump host and the credential
    /// settings ([`SessionTab::open`]).
    store: OnceCell<Rc<SessionStore>>,
    /// The tab label, which follows the terminal's title.
    label: OnceCell<gtk4::Widget>,
    /// Bumped by every attempt, so a superseded one drops its result.
    gen: Cell<u64>,
    /// A driver-driven redial awaiting the verdict of the attempt tagged.
    redial: RefCell<Option<(u64, RedialReply)>>,
    /// The next connect was asked for by the user, so it takes focus.
    focus_on_connect: Cell<bool>,
    status: RefCell<PageStatus>,
    /// The page's state channel, once its drain runs ([`page_events`]).
    events: RefCell<Option<UnboundedSender<TransportEvent>>>,
    /// The live transport wiring.
    terminal_tab: RefCell<Option<TerminalTab>>,
    /// The connected terminal and the tap on its input (doc 12).
    terminal: RefCell<Option<Arc<dyn TerminalBackend>>>,
    input_tap: RefCell<Option<InputTap>>,
    /// SSH pages' sidebar, and whether it holds the connection.
    sidebar: RefCell<Option<SftpSidebar>>,
    sidebar_attached: Cell<bool>,
    /// The live forward status source, while the engine is armed.
    forward: RefCell<Option<Arc<EngineStatusBridge>>>,
}

/// `page`'s state, created on first use.
fn state(page: &gtk4::Widget) -> Rc<PageState> {
    data_or_default(page, PAGE_KEY)
}

/// `page`'s state, if it has any (not the home tab, say).
fn existing_state(page: &gtk4::Widget) -> Option<Rc<PageState>> {
    data(page, PAGE_KEY)
}

/// Opening and querying session tabs.
pub struct SessionTab;

impl SessionTab {
    /// Open a new tab for `session`, even when one is already open, and
    /// start connecting. A duplicate drops tmux (doc 04 §Tmux Session
    /// Persistence) and tells the user so, unless they opted out. Returns
    /// the page widget. `store` is kept on the page so the connect flow can
    /// resolve the session's jump host at dial time (doc 11 §Jump hosts).
    pub fn open(
        notebook: &gtk4::Notebook,
        session: &SessionConfig,
        store: &Rc<SessionStore>,
    ) -> gtk4::Widget {
        let (page, dropped_tmux) = Self::open_page(notebook, session, store);
        if dropped_tmux && duplicate::notice_enabled(store) {
            duplicate::show_tmux_notice(window_of(notebook).as_ref(), store);
        }
        page
    }

    /// [`open`](Self::open) without the notice, for tabs restored at
    /// startup.
    pub fn restore(
        notebook: &gtk4::Notebook,
        session: &SessionConfig,
        store: &Rc<SessionStore>,
    ) -> gtk4::Widget {
        Self::open_page(notebook, session, store).0
    }

    /// Open the page; `true` when it is a duplicate that dropped tmux.
    fn open_page(
        notebook: &gtk4::Notebook,
        session: &SessionConfig,
        store: &Rc<SessionStore>,
    ) -> (gtk4::Widget, bool) {
        let already_open = (0..notebook.n_pages()).any(|page_num| {
            notebook
                .nth_page(Some(page_num))
                .is_some_and(|page| session_id_of(&page) == Some(session.id))
        });
        let (session, dropped_tmux) = duplicate::tab_config(session, already_open);
        let session = Rc::new(session);
        let page = page_content(&session);
        let tab_label = build_tab_label(notebook, &page, &session);
        let page_num = notebook.append_page(&page, Some(&tab_label));
        notebook.set_current_page(Some(page_num));
        let state = state(&page);
        let _ = state.label.set(tab_label);
        let _ = state.store.set(store.clone());
        install_teardown_on_remove(notebook);
        start_connect(notebook, &page, &session);
        (page, dropped_tmux)
    }

    pub fn sidebar_for_page(page: &gtk4::Widget) -> Option<SftpSidebar> {
        existing_state(page)?.sidebar.borrow().clone()
    }

    /// Give `page` its sidebar; the side dock hosts it while the page is
    /// current.
    pub(in crate::ui) fn set_sidebar_for_page(page: &gtk4::Widget, sidebar: SftpSidebar) {
        state(page).sidebar.replace(Some(sidebar));
    }

    /// The last state `page` published, if it is a session tab.
    pub(crate) fn page_status(page: &gtk4::Widget) -> Option<PageStatus> {
        Some(existing_state(page)?.status.borrow().clone())
    }

    /// The current tab's live forward status source, while its engine is
    /// armed. Tabs without one (offline, non-SSH, never connected) get the
    /// Tunnels dialog's offline fallback instead.
    pub(crate) fn current_forward_source(
        notebook: &gtk4::Notebook,
    ) -> Option<Arc<dyn ForwardStatusSource>> {
        let bridge = existing_state(&current_page(notebook)?)?
            .forward
            .borrow()
            .clone()?;
        Some(bridge)
    }

    /// The current tab's terminal, once connected (the terminal actions'
    /// target).
    pub fn active_terminal(notebook: &gtk4::Notebook) -> Option<Arc<dyn TerminalBackend>> {
        existing_state(&current_page(notebook)?)?
            .terminal
            .borrow()
            .clone()
    }

    /// The current tab's input tap, once connected (the macro recorder's
    /// target, doc 12).
    pub fn active_input_tap(notebook: &gtk4::Notebook) -> Option<InputTap> {
        existing_state(&current_page(notebook)?)?
            .input_tap
            .borrow()
            .clone()
    }

    /// Give `page` its connected terminal and the tap on its input.
    pub(in crate::ui) fn set_terminal_for_page(
        page: &gtk4::Widget,
        backend: Arc<dyn TerminalBackend>,
        tap: InputTap,
    ) {
        let state = state(page);
        state.terminal.replace(Some(backend));
        state.input_tap.replace(Some(tap));
    }
}

pub(crate) fn current_page(notebook: &gtk4::Notebook) -> Option<gtk4::Widget> {
    notebook.nth_page(Some(notebook.current_page()?))
}

/// The session id of the current page, if it is a session tab.
pub(crate) fn current_session_id(notebook: &gtk4::Notebook) -> Option<i64> {
    session_id_of(&current_page(notebook)?)
}

/// The id of the session `page` shows, if it is a session tab (read back
/// by tab persistence).
pub(crate) fn session_id_of(page: &gtk4::Widget) -> Option<i64> {
    Some(existing_state(page)?.session.get()?.id)
}

/// The current page's tab id, if it is a session tab (the `reconnect` and
/// `disconnect` actions' target).
pub(crate) fn current_tab_id(notebook: &gtk4::Notebook) -> Option<i64> {
    tab_id_of(&current_page(notebook)?)
}

/// `page`'s tab id ([`PageState::tab_id`]), if it is a session tab.
fn tab_id_of(page: &gtk4::Widget) -> Option<i64> {
    let state = existing_state(page)?;
    state.session.get()?;
    Some(state.tab_id.get())
}

// ---------------------------------------------------------------------------
// Connect flow
// ---------------------------------------------------------------------------

/// Start (or retry) connecting `page`, invalidating any attempt in flight.
fn start_connect(notebook: &gtk4::Notebook, page: &gtk4::Widget, session: &Rc<SessionConfig>) {
    let gen = bump_gen(page);
    stop_forward_engine(page);
    close_terminal(page);
    // Every caller but the reconnect driver is the user asking for this
    // tab; `redial` clears it again.
    state(page).focus_on_connect.set(true);

    // VNC has no byte stream: its own dial (with the password prompt) and
    // engine, no supervisor or terminal (doc 08 §VNC).
    if session.session_type == SessionType::VNC {
        return vnc_tab::start(notebook, page, session, gen);
    }
    // RDP likewise owns its subprocess dial (doc 08 §RDP).
    if session.session_type == SessionType::RDP {
        return rdp_tab::start(notebook, page, session, gen);
    }

    // Nothing to dial or authenticate: no supervisor, host key or credential
    // machinery (doc 04 §Local and serial transports).
    if matches!(
        session.session_type,
        SessionType::LocalShell | SessionType::Serial
    ) {
        let (notebook, page, session) = (notebook.clone(), page.clone(), session.clone());
        glib::MainContext::default().spawn_local(async move {
            if current_gen(&page) == gen {
                finish_connect(&notebook, &session, &page, None).await;
            }
        });
        return;
    }

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
    let params = ConnectParams {
        host: host.clone(),
        port,
        username,
    };
    // Jump sessions dial through their bastion (doc 11 §Jump hosts): the
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
        // Each hop authenticates as its own session (doc 11).
        let jump_auth = hop_auth(&jump_cfg, store.as_deref());
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

/// The error views for a row missing what its dial needs.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) const NO_HOST: &str = "This session has no host configured.";
pub(crate) const NO_USERNAME: &str = "This session has no username configured.";

/// A row's text field, trimmed; `None` when blank.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn filled(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// How the hop `session` describes authenticates (doc 11): its stored
/// method, its own credential chain, a prompt-backed host-key check under
/// its own policy — else the global one, or strict when the page has no
/// store to read that from — and the challenge dialog.
fn hop_auth(session: &SessionConfig, store: Option<&SessionStore>) -> HopAuth {
    let policy = session
        .host_key_policy
        .clone()
        .unwrap_or_else(|| store.map_or(HostKeyPolicy::Strict, SessionStore::host_key_policy));
    HopAuth {
        method: jump::hop_auth_method(session),
        creds: credential_manager(session.id, &session.name, store),
        verifier: HostKeyVerifier::new(policy, hostkey_dialog::host_key_prompt(), None),
        responder: Some(auth_dialog::challenge_responder(session.name.clone())),
    }
}

/// Whether `session` dials through a bastion (doc 11 §Jump hosts): SSH
/// with a jump host set. VNC, RDP, local and serial return before the dial
/// exists, so this only ever fires for SSH.
fn uses_jump_dial(session: &SessionConfig) -> bool {
    session.session_type == SessionType::SSH && session.jump_session_id.is_some()
}

/// The page's store, kept by [`SessionTab::open`] for jump resolution and
/// the credential settings.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn page_store(page: &gtk4::Widget) -> Option<Rc<SessionStore>> {
    state(page).store.get().cloned()
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

/// The rules the forward engine arms on `finish_connect`: the session's
/// current set, so every redial re-enters with the same rules and a
/// reconnect re-establishes them with no separate hook (doc 11).
fn forward_rules_for(session: &SessionConfig) -> Vec<ForwardRule> {
    session.forward_rules.clone().unwrap_or_default()
}

/// Arm the caller-owned forward engine for a fresh connection (doc 11
/// §Port forwarding): start the session's rules — the engine spawns its
/// listeners and never blocks — and bridge the event stream into the
/// page's live status source for the Tunnels dialog. Replaces any engine
/// still on the page, so a redial never stacks listeners.
fn arm_forward_engine(page: &gtk4::Widget, session: &Rc<SessionConfig>, conn: &Arc<SshConnection>) {
    stop_forward_engine(page);
    let (tx, rx) = mpsc::channel::<ForwardEvent>(FWD_EVENT_BUF);
    let engine = ForwardEngine::new(conn.clone(), tx);
    let bridge = Arc::new(EngineStatusBridge::new(engine.clone()));
    state(page).forward.replace(Some(bridge.clone()));
    engine.start(forward_rules_for(session));
    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            bridge.handle_event(event);
        }
    });
}

/// Stop the page's forward engine, if any: listeners close and terminal
/// `Stopped` snapshots emit deterministically (a dropped engine would
/// abort its tasks anyway). Stopping clears the page key, so a
/// disconnected tab falls back to the dialog's offline state until the
/// next `finish_connect` arms a fresh engine.
fn stop_forward_engine(page: &gtk4::Widget) {
    if let Some(bridge) = state(page).forward.take() {
        bridge.engine().stop();
    }
}

/// Embed the terminal, measure its real grid, wire the transport, and
/// attach the sidebar to the same connection (SSH only: local and serial
/// pass no connection and get no sidebar).
async fn finish_connect(
    notebook: &gtk4::Notebook,
    session: &Rc<SessionConfig>,
    page: &gtk4::Widget,
    conn: Option<Arc<SshConnection>>,
) {
    let gen = current_gen(page);
    let handle = create_terminal();
    let backend = handle.backend.clone();
    let widget = backend.widget();
    set_terminal_area(page, &widget);
    for _ in 0..ALLOC_WAIT_STEPS {
        if widget.width() > 0 && widget.height() > 0 {
            break;
        }
        glib::timeout_future(Duration::from_millis(16)).await;
    }
    let (rows, cols) = match widget.downcast_ref::<vte4::Terminal>() {
        Some(vte) => (
            vte.row_count().max(1) as u32,
            vte.column_count().max(1) as u32,
        ),
        None => (24, 80),
    };
    // The wait yields: a cancel, a retry or a close may have overtaken us.
    if current_gen(page) != gen || notebook.page_num(page).is_none() {
        return;
    }

    let main_ctx = glib::MainContext::default();
    // Macro recording listens between the terminal and the pump (doc 12).
    let (handle, input_tap) = InputTap::wrap(handle, &main_ctx);
    let wired = TerminalTab::new(session, conn.clone(), handle, &main_ctx, rows, cols);
    let (terminal_tab, state_rx) = match wired {
        Ok(wired) => wired,
        Err(err) => return show_error_view(notebook, page, session, &err.to_string()),
    };
    let (host, port) = status_endpoint(session, terminal_tab.description());
    state(page).terminal_tab.replace(Some(terminal_tab));
    SessionTab::set_terminal_for_page(page, backend, input_tap);
    let tab_id = state(page).tab_id.get();
    let cancel = tab_resilience::register_connected_tab(tab_id, redial_fn(notebook, page, tab_id));
    let events = page_events(notebook, page, session, host, port);
    spawn_state_forward(page.clone(), gen, cancel, state_rx, events);

    // SSH tabs arm the caller-owned forward engine on every connect, so a
    // redial re-establishes the same rules on the fresh connection
    // (doc 11 §Port forwarding). Local and serial pass no connection.
    if session.session_type == SessionType::SSH {
        if let Some(conn) = &conn {
            arm_forward_engine(page, session, conn);
        }
    }

    if let (Some(conn), Some(sidebar)) = (conn, SessionTab::sidebar_for_page(page)) {
        if session.jump_session_id.is_some() {
            // Doc 11 v1: no SFTP task runs over a jumped connection; the
            // sidebar shows the limitation instead of attaching.
            sidebar.show_jump_placeholder();
        } else {
            sidebar.follow_new_shell(ShellCwdProbe::new(&conn, session.tmux_remote_name()));
            sidebar.attach_on_connect(conn);
            state(page).sidebar_attached.set(true);
            follow_terminal_directory(page, &widget);
            // Like the terminal's focus: only a connect the user started
            // brings the SFTP panel forward, never a reconnect.
            if sftp_sidebar::open_on_connect() && state(page).focus_on_connect.get() {
                side_dock::reveal_sftp(notebook, page);
            }
        }
    }
    follow_terminal_title(page, &widget, &session.name);
    focus_after_connect(notebook, page);
}

/// A user-started connect takes focus (from the session tree, say); a
/// reconnect only takes it back if nothing holds it — the old terminal was
/// torn down with the connection — so it never pulls focus out of the SFTP
/// sidebar or the session tree while the user is typing there.
fn focus_after_connect(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    let requested = state(page).focus_on_connect.replace(false);
    if requested || focus_unclaimed(page) {
        focus_terminal(notebook, page);
    }
}

/// No widget has focus, or only a container of `page` (GTK may hand focus
/// to an ancestor when the focused terminal is removed).
fn focus_unclaimed(page: &gtk4::Widget) -> bool {
    match page.root().and_then(|root| root.focus()) {
        None => true,
        Some(focus) => focus == *page || page.is_ancestor(&focus),
    }
}

/// Give the page's terminal keyboard focus, so typing reaches the shell
/// instead of whatever opened the tab (the session tree, whose type-ahead
/// search would take the keys). Only on the current page: a background
/// tab's connect or redial must not take focus from the tab in use.
fn focus_terminal(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    if current_page(notebook).as_ref() != Some(page) {
        return;
    }
    let terminal = state(page).terminal.borrow().clone();
    if let Some(backend) = terminal {
        backend.widget().grab_focus();
    }
}

/// What the status bar shows for a tab: `host:port` for SSH, the
/// transport's description (no port, `0`) for local and serial.
fn status_endpoint(session: &SessionConfig, description: &str) -> (String, u16) {
    match session.session_type {
        SessionType::LocalShell | SessionType::Serial => (description.to_string(), 0),
        _ => (
            session.host.clone().unwrap_or_default(),
            session.port.unwrap_or(22),
        ),
    }
}

/// The page's state channel, created with its drain on first use: every
/// attempt's forwarder feeds it, and so does the reconnect driver
/// (`Reconnecting { attempt }`), so it outlives any one attempt.
fn page_events(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    host: String,
    port: u16,
) -> UnboundedSender<TransportEvent> {
    let state = state(page);
    if let Some(events) = state.events.borrow().clone() {
        return events;
    }
    let (events, rx) = mpsc::unbounded_channel();
    // The page holds the only lasting sender: the drain ends with the page.
    state.events.replace(Some(events.clone()));
    let downgraded = events.downgrade();
    spawn_state_drain(notebook, page, session, host, port, downgraded, rx);
    events
}

/// Forward one attempt's state channel to the page's until its terminal
/// closes or a newer attempt takes over. The tab's token firing
/// (Disconnect) ends the attempt.
fn spawn_state_forward(
    page: gtk4::Widget,
    gen: u64,
    cancel: CancellationToken,
    mut rx: UnboundedReceiver<TransportEvent>,
    events: UnboundedSender<TransportEvent>,
) {
    glib::MainContext::default().spawn_local(async move {
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    if current_gen(&page) == gen {
                        bump_gen(&page);
                        close_terminal(&page);
                        publish_page_state(&page, TransportState::Disconnected);
                    }
                    return;
                }
                event = rx.recv() => event,
            };
            match event {
                Some(event) if current_gen(&page) == gen => {
                    if events.send(event).is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    });
}

/// Replay the page's transport events into the registry, its status and
/// views, and the disconnect alert; on `ConnectionLost`, start the reconnect
/// driver per policy (doc 04 §Lifecycle). `SessionEnded` neither alerts nor
/// reconnects.
fn spawn_state_drain(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    host: String,
    port: u16,
    events: WeakUnboundedSender<TransportEvent>,
    mut rx: UnboundedReceiver<TransportEvent>,
) {
    let app = window_of(page).and_then(|window| window.application());
    let tab_id = state(page).tab_id.get();
    let (notebook, session, page) = (notebook.clone(), session.clone(), page.downgrade());
    glib::MainContext::default().spawn_local(async move {
        while let Some(event) = rx.recv().await {
            let Some(page) = page.upgrade() else { break };
            match event {
                TransportEvent::StateChanged(state) => {
                    if state == TransportState::Connected {
                        settle_redial(&page, Ok(()));
                    }
                    tab_resilience::set_tab_state(tab_id, state.clone());
                    publish_status(&page, state, &host, port);
                }
                TransportEvent::Error(message) => {
                    let state = TransportState::Error(message.clone());
                    tab_resilience::set_tab_state(tab_id, state.clone());
                    show_error_view(&notebook, &page, &session, &message);
                    publish_status(&page, state, &host, port);
                }
                TransportEvent::ConnectionLost => {
                    publish_status(&page, TransportState::Disconnected, &host, port);
                    if let Some(app) = &app {
                        tray::send_disconnect_notification(app, &session.name);
                    }
                    let Some(events) = events.upgrade() else {
                        break;
                    };
                    if tab_resilience::on_connection_lost(tab_id, events).is_some() {
                        // Free the dead session during the first backoff.
                        release_connection(&notebook, &page);
                    }
                }
                TransportEvent::SessionEnded => {
                    tab_resilience::set_tab_state(tab_id, TransportState::Disconnected);
                    publish_status(&page, TransportState::Disconnected, &host, port);
                    show_session_ended(&notebook, &page);
                }
                TransportEvent::DataReceived(_) => {}
            }
        }
    });
}

/// The registered tab's connect closure (doc 04: every attempt is a fresh
/// connect through the supervisor, so tmux reattaches). It runs on the
/// driver's tokio task, so it only hops to the main context, where the page
/// is rewired through [`start_connect`] with a new generation, and awaits
/// that attempt's verdict. Weak refs only: the registry must not keep a
/// closed page alive.
fn redial_fn(notebook: &gtk4::Notebook, page: &gtk4::Widget, tab_id: i64) -> ConnectFn {
    // Shared, never cloned off the main thread: cloning a weak ref upgrades it.
    let notebook = Arc::new(glib::SendWeakRef::from(notebook.downgrade()));
    let page = Arc::new(glib::SendWeakRef::from(page.downgrade()));
    Arc::new(move || {
        let (reply, verdict) = oneshot::channel();
        let (for_redial, for_page) = (notebook.clone(), page.clone());
        glib::MainContext::default().invoke(move || {
            if let (Some(notebook), Some(page)) = (for_redial.upgrade(), for_page.upgrade()) {
                redial(&notebook, &page, reply);
            }
        });
        let cancel = tab_resilience::live_tab(tab_id).map(|tab| tab.cancel);
        let page = page.clone();
        Box::pin(async move {
            let Some(cancel) = cancel else {
                return Err(TransportError::Cancelled);
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    glib::MainContext::default().invoke(move || {
                        if let Some(page) = page.upgrade() {
                            abandon_attempt(&page);
                        }
                    });
                    Err(TransportError::Cancelled)
                }
                // A dropped reply means the page went away.
                verdict = verdict => verdict.unwrap_or(Err(TransportError::Cancelled)),
            }
        }) as ConnectFuture
    })
}

/// Main thread: rewire `page` for a driver attempt; `reply` gets the verdict.
fn redial(notebook: &gtk4::Notebook, page: &gtk4::Widget, reply: RedialReply) {
    let state = state(page);
    let Some(session) = state.session.get().cloned() else {
        return;
    };
    if notebook.page_num(page).is_none() {
        return;
    }
    release_connection(notebook, page);
    // `start_connect` takes the next generation first thing.
    state
        .redial
        .replace(Some((state.gen.get().wrapping_add(1), reply)));
    start_connect(notebook, page, &session);
    state.focus_on_connect.set(false);
}

/// Let go of the page's hold on its connection — the transport and the
/// sidebar's SFTP task (or the connection it keeps for its first show) — so
/// the supervisor can evict a dead session and the next attempt dials fresh;
/// a live shared one is simply handed out again.
fn release_connection(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    stop_forward_engine(page);
    close_terminal(page);
    if !state(page).sidebar_attached.replace(false) {
        return;
    }
    let Some(old) = SessionTab::sidebar_for_page(page) else {
        return;
    };
    let sidebar = if old.is_shown() {
        SftpSidebar::new()
    } else {
        SftpSidebar::hidden()
    };
    sidebar.set_following(old.is_following());
    SessionTab::set_sidebar_for_page(page, sidebar);
    side_dock::sidebar_replaced(notebook);
}

/// Disconnect landed while a redial was in flight: drop the attempt.
fn abandon_attempt(page: &gtk4::Widget) {
    bump_gen(page);
    close_terminal(page);
    set_terminal_area(page, &centered_label("Disconnected"));
    publish_page_state(page, TransportState::Disconnected);
}

/// Closing a page by any path tears its registry entry down, so no
/// reconnect loop outlives its tab (doc 04 §Lifecycle). Once per notebook.
fn install_teardown_on_remove(notebook: &gtk4::Notebook) {
    if data::<bool>(notebook, TEARDOWN_HOOK_KEY).is_some() {
        return;
    }
    set_data(notebook, TEARDOWN_HOOK_KEY, true);
    notebook.connect_page_removed(|_, page, _| {
        if let Some(tab_id) = tab_id_of(page) {
            bump_gen(page);
            // Closing by any path stops the engine deterministically
            // (`close_tab` removes the page, so it lands here too).
            stop_forward_engine(page);
            tab_resilience::teardown_tab(tab_id);
        }
    });
}

/// Keep the tab label on the terminal's window title (falling back to the
/// session name), unless the user has renamed the tab.
fn follow_terminal_title(page: &gtk4::Widget, terminal: &gtk4::Widget, session_name: &str) {
    let (Some(label), Some(vte)) = (
        state(page).label.get().cloned(),
        terminal.downcast_ref::<vte4::Terminal>(),
    ) else {
        return;
    };
    let name = session_name.to_string();
    let apply = move |term: &vte4::Terminal| {
        let title = term
            .window_title()
            .map(|t| t.to_string())
            .unwrap_or_default();
        tab_label::apply_terminal_title(&label, &tab_title(&name, &title));
    };
    apply(vte);
    vte.connect_window_title_changed(apply);
}

/// Keep the page's SFTP sidebar on the shell's working directory (doc 09
/// §Following the shell): VTE reports OSC 7 as `current-directory-uri`,
/// and after each command — Enter, then [`PROBE_QUIET`] without output —
/// the sidebar may ask the remote instead. The sidebar is looked up each
/// time, since a redial replaces it.
fn follow_terminal_directory(page: &gtk4::Widget, terminal: &gtk4::Widget) {
    let Some(vte) = terminal.downcast_ref::<vte4::Terminal>() else {
        return;
    };
    let page = page.downgrade();
    let follower = RefCell::new(DirectoryFollower::default());
    let for_report = page.clone();
    vte.connect_current_directory_uri_changed(move |term| {
        let (Some(page), Some(uri)) = (for_report.upgrade(), term.current_directory_uri()) else {
            return;
        };
        let Some(path) = follower.borrow_mut().report(&uri) else {
            return;
        };
        if let Some(sidebar) = SessionTab::sidebar_for_page(&page) {
            sidebar.follow_shell(path);
        }
    });

    let armed = Rc::new(Cell::new(false));
    let for_commit = Rc::clone(&armed);
    vte.connect_commit(move |_, text, _| {
        if text.contains(['\r', '\n']) {
            for_commit.set(true);
        }
    });
    let quiet: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    vte.connect_contents_changed(move |_| {
        if !armed.get() {
            return;
        }
        if let Some(pending) = quiet.take() {
            pending.remove();
        }
        let (page, armed, fired) = (page.clone(), Rc::clone(&armed), Rc::clone(&quiet));
        let source = glib::timeout_add_local_once(PROBE_QUIET, move || {
            // A fired source must not be removed again.
            fired.take();
            armed.set(false);
            if let Some(sidebar) = page
                .upgrade()
                .and_then(|page| SessionTab::sidebar_for_page(&page))
            {
                sidebar.probe_shell();
            }
        });
        *quiet.borrow_mut() = Some(source);
    });
}

/// The terminal's title, or the session name when it is blank.
fn tab_title(session_name: &str, terminal_title: &str) -> String {
    match terminal_title.trim() {
        "" => session_name.to_string(),
        title => title.to_string(),
    }
}

/// What the prompt says when the keyring cannot be reached.
const KEYRING_UNAVAILABLE_HINT: &str = "Keyring unavailable — prompting for credentials.";

/// Keyring first, then Bitwarden when enabled, then a one-field challenge
/// dialog (doc 11 §CredentialManager), for saved session `session_id`. The
/// settings come from `store`; without one (a page opened with no store)
/// Bitwarden stays off.
// used by ui::vnc_tab, ui::rdp_tab and ui::session_tree
pub(crate) fn credential_manager(
    session_id: i64,
    session_name: &str,
    store: Option<&SessionStore>,
) -> CredentialManager {
    let session_id = session_id.to_string();
    let settings = store.map(CredentialSettings::load).unwrap_or_default();
    let keyring = KeyringSource::new(&session_id, session_name);
    let keyring_unreachable = !keyring.available();
    let prompt = prompt_source(session_name, prompt_instructions(keyring_unreachable));
    CredentialManager::for_session(
        &session_id,
        credential_sources(&settings, session_name, Box::new(keyring), Box::new(prompt)),
    )
}

/// The prompt's instructions line: the keyring hint, or nothing.
fn prompt_instructions(keyring_unreachable: bool) -> Option<&'static str> {
    keyring_unreachable.then_some(KEYRING_UNAVAILABLE_HINT)
}

/// The chain in priority order: keyring, Bitwarden when enabled, and the
/// prompt always last.
fn credential_sources(
    settings: &CredentialSettings,
    session_name: &str,
    keyring: Box<dyn CredentialSource>,
    prompt: Box<dyn CredentialSource>,
) -> Vec<Box<dyn CredentialSource>> {
    let mut sources = vec![keyring];
    if settings.bitwarden_enabled {
        sources.push(Box::new(BitwardenSource::new(
            &settings.bitwarden_bin,
            session_name,
        )));
    }
    sources.push(prompt);
    sources
}

/// The challenge dialog with one masked field.
fn prompt_source(session_name: &str, instructions: Option<&'static str>) -> PromptSource {
    let name = session_name.to_string();
    PromptSource::new(Arc::new(move |_session_id, secret_type| {
        let label = match secret_type {
            "key_passphrase" => "Passphrase: ",
            _ => "Password: ",
        };
        let prompts = [ChallengePrompt {
            prompt: label.into(),
            echo: false,
        }];
        let name = name.clone();
        auth_dialog::run_on_gtk_main_thread(move || {
            auth_dialog::run_challenges(&name, instructions, &prompts)
        })
        .flatten()
        .and_then(|answers| answers.into_iter().next())
        .map(|answer| Zeroizing::new(answer.into_bytes()))
    }))
}

#[cfg(test)]
mod credential_chain_tests {
    use super::*;

    fn names(settings: &CredentialSettings) -> Vec<String> {
        let prompt = PromptSource::new(Arc::new(|_, _| None));
        let keyring = KeyringSource::new("1", "prod");
        credential_sources(settings, "prod", Box::new(keyring), Box::new(prompt))
            .iter()
            .map(|source| source.source_name().to_string())
            .collect()
    }

    #[test]
    fn bitwarden_sits_between_keyring_and_prompt_when_enabled() {
        let on = CredentialSettings {
            bitwarden_enabled: true,
            ..CredentialSettings::default()
        };
        assert_eq!(names(&on), ["keyring", "bitwarden", "prompt"]);
    }

    #[test]
    fn bitwarden_is_absent_when_disabled() {
        assert_eq!(names(&CredentialSettings::default()), ["keyring", "prompt"]);
    }

    #[test]
    fn the_manager_reads_the_store_and_ends_with_the_prompt() {
        let store = SessionStore::in_memory().unwrap();
        let debug = |mgr: CredentialManager| format!("{mgr:?}");
        assert!(debug(credential_manager(42, "prod", Some(&store)))
            .contains(r#"["keyring", "prompt"]"#));
        crate::ui::settings::credentials::save_enabled(&store, true).unwrap();
        assert!(debug(credential_manager(42, "prod", Some(&store)))
            .contains(r#"["keyring", "bitwarden", "prompt"]"#));
        assert!(debug(credential_manager(42, "prod", None)).contains(r#"["keyring", "prompt"]"#));
    }

    #[test]
    fn the_hint_shows_only_when_the_keyring_is_unreachable() {
        assert_eq!(prompt_instructions(false), None);
        assert_eq!(prompt_instructions(true), Some(KEYRING_UNAVAILABLE_HINT));
    }
}

// ---------------------------------------------------------------------------
// Page views and bookkeeping
// ---------------------------------------------------------------------------

/// Make `child` the page's only child, filling it.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn set_terminal_area(page: &gtk4::Widget, child: &impl IsA<gtk4::Widget>) {
    let Some(page) = page.downcast_ref::<gtk4::Box>() else {
        return;
    };
    while let Some(old) = page.first_child() {
        page.remove(&old);
    }
    child.set_hexpand(true);
    child.set_vexpand(true);
    page.append(child);
}

/// A centered column: the layout of every status view.
fn status_column() -> gtk4::Box {
    let view = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    view.set_vexpand(true);
    view.set_hexpand(true);
    view.set_valign(gtk4::Align::Center);
    view.set_halign(gtk4::Align::Center);
    view
}

/// `text` over a Cancel button that runs `on_cancel`: a dial in progress.
/// The button lives inside the page, so `on_cancel` must hold the page
/// weakly, or a tab closed while connecting is never freed.
// used by ui::rdp_tab
pub(crate) fn connecting_view(text: &str, on_cancel: impl Fn() + 'static) -> gtk4::Widget {
    let view = status_column();
    view.append(&centered_label(text));
    let cancel = gtk4::Button::with_label("Cancel");
    cancel.set_halign(gtk4::Align::Center);
    cancel.connect_clicked(move |_| on_cancel());
    view.append(&cancel);
    view.upcast()
}

/// A heading over a wrapped, selectable `detail`, then `buttons` in a
/// centered row. Handlers on the buttons must hold the page weakly (see
/// [`connecting_view`]).
// used by ui::rdp_tab
pub(crate) fn notice_view(heading: &str, detail: &str, buttons: &[gtk4::Button]) -> gtk4::Widget {
    let view = status_column();
    set_margins(&view, 24);
    let title = gtk4::Label::new(Some(heading));
    title.add_css_class("heading");
    let text = gtk4::Label::new(Some(detail));
    text.set_wrap(true);
    text.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    text.set_selectable(true);
    text.set_justify(gtk4::Justification::Center);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.set_halign(gtk4::Align::Center);
    for button in buttons {
        row.append(button);
    }
    view.append(&title);
    view.append(&text);
    view.append(&row);
    view.upcast()
}

/// "Connecting to host:port…" with a Cancel that drops the attempt.
// used by ui::vnc_tab
pub(crate) fn show_connecting_view(page: &gtk4::Widget, host: &str, port: u16) {
    let (weak, host_for_cancel) = (page.downgrade(), host.to_string());
    let view = connecting_view(&format!("Connecting to {host}:{port}…"), move || {
        if let Some(page) = weak.upgrade() {
            bump_gen(&page);
            close_terminal(&page);
            show_disconnected(&page, &host_for_cancel, port);
        }
    });
    set_terminal_area(page, &view);
}

/// The attempt ended cleanly (a cancelled prompt, Cancel): "Disconnected".
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn show_disconnected(page: &gtk4::Widget, host: &str, port: u16) {
    set_terminal_area(page, &centered_label("Disconnected"));
    publish_status(page, TransportState::Disconnected, host, port);
}

/// The attempt failed: the error view, and the error on the status bar.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn show_failure(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    message: &str,
    host: &str,
    port: u16,
) {
    show_error_view(notebook, page, session, message);
    publish_status(page, TransportState::Error(message.to_string()), host, port);
}

/// The error message with Retry and Close Tab.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn show_error_view(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    message: &str,
) {
    settle_redial(
        page,
        Err(TransportError::ConnectionFailed(message.to_string())),
    );
    // An error can arrive after the terminal was built.
    close_terminal(page);

    let retry = gtk4::Button::with_label("Retry");
    retry.add_css_class("suggested-action");
    {
        let (notebook, page, session) = (notebook.clone(), page.downgrade(), session.clone());
        retry.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                start_connect(&notebook, &page, &session);
            }
        });
    }
    let close = gtk4::Button::with_label("Close Tab");
    {
        let (notebook, page) = (notebook.clone(), page.downgrade());
        close.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                close_tab(&notebook, &page);
            }
        });
    }
    let view = notice_view("Connection failed", message, &[retry, close]);
    set_terminal_area(page, &view);
}

/// The shell exited on its own: keep the terminal and its scrollback, add
/// the notice, and let `r` reconnect (as Retry does) or `q` close the tab.
fn show_session_ended(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    close_terminal(page);
    let Some(backend) = state(page).terminal.borrow().clone() else {
        return;
    };
    backend.write(session_ended::NOTICE.as_bytes());
    // Weak: the controller lives on the terminal, inside the page.
    let (notebook, page) = (notebook.downgrade(), page.downgrade());
    session_ended::install(&backend.widget(), move |choice| {
        let (Some(notebook), Some(page)) = (notebook.upgrade(), page.upgrade()) else {
            return;
        };
        match choice {
            EndedChoice::Reconnect => {
                if let Some(session) = state(&page).session.get().cloned() {
                    start_connect(&notebook, &page, &session);
                }
            }
            EndedChoice::CloseTab => close_tab(&notebook, &page),
        }
    });
}

/// Remove the page without asking: its session failed or is over.
fn close_tab(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    bump_gen(page);
    if let Some(page_num) = notebook.page_num(page) {
        // Dropping the page cancels its terminal tasks.
        notebook.remove_page(Some(page_num));
    }
}

/// Record `state` on the page and show it if the page is current.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn publish_status(page: &gtk4::Widget, state: TransportState, host: &str, port: u16) {
    self::state(page).status.replace(PageStatus {
        state: state.clone(),
        host: host.to_string(),
        port,
    });
    if let Some(window) = window_of(page).and_then(|w| w.downcast().ok()) {
        route_status(&window, page, state, host, port);
    }
}

/// [`publish_status`] with the endpoint the page last published.
fn publish_page_state(page: &gtk4::Widget, state: TransportState) {
    let PageStatus { host, port, .. } = self::state(page).status.borrow().clone();
    publish_status(page, state, &host, port);
}

/// Tear down the page's terminal, if any, in the background.
fn close_terminal(page: &gtk4::Widget) {
    if let Some(mut tab) = state(page).terminal_tab.take() {
        glib::MainContext::default().spawn_local(async move { tab.close().await });
    }
}

/// Invalidate any attempt in flight; returns the new generation. A redial
/// waiting on an older attempt was superseded (Cancel, Retry, close) and
/// ends the driver's run.
fn bump_gen(page: &gtk4::Widget) -> u64 {
    let state = state(page);
    let gen = state.gen.get().wrapping_add(1);
    state.gen.set(gen);
    let stale = state.redial.borrow_mut().take_if(|(g, _)| *g != gen);
    if let Some((_, reply)) = stale {
        let _ = reply.send(Err(TransportError::Cancelled));
    }
    gen
}

/// Answer a redial waiting on the current attempt, if any.
fn settle_redial(page: &gtk4::Widget, result: Result<(), TransportError>) {
    let state = state(page);
    let gen = state.gen.get();
    let pending = state.redial.borrow_mut().take_if(|(g, _)| *g == gen);
    if let Some((_, reply)) = pending {
        let _ = reply.send(result);
    }
}

// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn current_gen(page: &gtk4::Widget) -> u64 {
    state(page).gen.get()
}

/// The page for `session`: a box holding the session's area, and its
/// [`PageState`]. SSH pages carry their SFTP sidebar for the side dock
/// (hidden while `sftp_on_connect` is off).
fn page_content(session: &Rc<SessionConfig>) -> gtk4::Widget {
    let page = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    page.set_vexpand(true);
    page.set_hexpand(true);
    let page: gtk4::Widget = page.upcast();
    set_terminal_area(
        &page,
        &centered_label(&format!("{}\nnot connected", session.name)),
    );
    let state = state(&page);
    let _ = state.session.set(session.clone());
    state
        .tab_id
        .set(NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed));
    if supports_sftp(&session.session_type) {
        let sidebar = if sftp_sidebar::open_on_connect() {
            SftpSidebar::new()
        } else {
            SftpSidebar::hidden()
        };
        state.sidebar.replace(Some(sidebar));
    }
    page
}

/// The tab label; closing asks first when the transport is live (doc 04
/// §Lifecycle, `confirm_close`).
fn build_tab_label(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &SessionConfig,
) -> gtk4::Widget {
    let notebook = notebook.clone();
    // Weak: the label belongs to the notebook, which owns the page.
    let page = page.downgrade();
    let (label, _) = tab_label::build_tab_label(&session.name, move || {
        if let Some(page) = page.upgrade() {
            request_close_page(&notebook, &page);
        }
    });
    label
}

/// Close `page` as its close button does: a session tab asks first while
/// its transport is live (doc 04 §Lifecycle, `confirm_close`); any other
/// page goes at once. The `close_tab` action's path too.
pub(crate) fn request_close_page(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    let Some(tab_id) = tab_id_of(page) else {
        if let Some(page_num) = notebook.page_num(page) {
            notebook.remove_page(Some(page_num));
        }
        return;
    };
    let (notebook, weak) = (notebook.clone(), page.downgrade());
    let parent = window_of(&notebook);
    crate::ui::tab_resilience::request_close(tab_id, parent.as_ref(), move || {
        if let Some(page_num) = weak.upgrade().and_then(|page| notebook.page_num(&page)) {
            notebook.remove_page(Some(page_num));
        }
    });
}

mod duplicate;
pub(crate) use duplicate::TMUX_NOTICE_SETTING;

#[cfg(test)]
mod tests;

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;
    use crate::hotkeys::engine::focus_within;

    /// Focusing the current tab hands keyboard focus from the session tree
    /// to its terminal; a tab in the background never takes it.
    pub(in crate::ui) fn focusing_a_tab_moves_focus_to_its_terminal() {
        let notebook = gtk4::Notebook::new();
        // Stands in for the session tree, which holds focus after a
        // double-click.
        let tree = gtk4::Entry::new();
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.append(&tree);
        root.append(&notebook);
        let window = gtk4::Window::new();
        window.set_child(Some(&root));

        let tabs: Vec<_> = (1..=2)
            .map(|id| {
                let session = SessionConfig {
                    id,
                    name: format!("local-{id}"),
                    session_type: SessionType::LocalShell,
                    ..Default::default()
                };
                let page = page_content(&Rc::new(session.clone()));
                let backend = create_terminal().backend;
                set_terminal_area(&page, &backend.widget());
                state(&page).terminal.replace(Some(backend.clone()));
                notebook.append_page(&page, None::<&gtk4::Widget>);
                (session, page, backend.widget())
            })
            .collect();
        window.present();
        for _ in 0..10 {
            glib::MainContext::default().iteration(false);
        }

        let [(_, first_page, first_terminal), (_, _, second_terminal)] = &tabs[..] else {
            unreachable!();
        };
        notebook.set_current_page(Some(1));
        tree.grab_focus();
        focus_terminal(&notebook, first_page);
        assert!(focus_within(&tree), "a background tab took focus");

        notebook.set_current_page(Some(0));
        focus_terminal(&notebook, first_page);
        assert!(focus_within(first_terminal), "the opened tab's terminal");
        assert!(!focus_within(second_terminal));
        window.destroy();
    }

    /// A reconnect finishing on the current tab leaves focus where the user
    /// put it; it takes focus only when nothing holds it, while a
    /// user-started connect always does.
    pub(in crate::ui) fn reconnect_does_not_steal_focus() {
        let notebook = gtk4::Notebook::new();
        let tree = gtk4::Entry::new();
        // Stands in for the SFTP sidebar's path entry, in the side dock.
        let sidebar_entry = gtk4::Entry::new();
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.append(&tree);
        root.append(&sidebar_entry);
        root.append(&notebook);
        let window = gtk4::Window::new();
        window.set_child(Some(&root));

        let session = SessionConfig {
            id: 1,
            name: "local-1".into(),
            session_type: SessionType::LocalShell,
            ..Default::default()
        };
        let page = page_content(&Rc::new(session));
        let backend = create_terminal().backend;
        let terminal = backend.widget();
        set_terminal_area(&page, &terminal);
        state(&page).terminal.replace(Some(backend));
        notebook.append_page(&page, None::<&gtk4::Widget>);
        window.present();
        for _ in 0..10 {
            glib::MainContext::default().iteration(false);
        }

        // Reconnect while typing in the sidebar, then in the tree.
        for holder in [
            sidebar_entry.upcast_ref::<gtk4::Widget>(),
            tree.upcast_ref(),
        ] {
            holder.grab_focus();
            state(&page).focus_on_connect.set(false);
            focus_after_connect(&notebook, &page);
            assert!(focus_within(holder), "a reconnect stole focus");
        }

        // Reconnect after the focused terminal went away with the connection.
        gtk4::prelude::GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
        focus_after_connect(&notebook, &page);
        assert!(focus_within(&terminal), "an unclaimed focus is taken back");

        // A user-started connect takes focus from the tree.
        tree.grab_focus();
        state(&page).focus_on_connect.set(true);
        focus_after_connect(&notebook, &page);
        assert!(focus_within(&terminal), "a user-started connect");
        assert!(
            !state(&page).focus_on_connect.get(),
            "the request is one-shot"
        );
        window.destroy();
    }

    /// The shell's OSC 7 report moves the page's sidebar — through the real
    /// VTE signal — while a report from another host (a nested ssh) does not.
    pub(in crate::ui) fn sftp_sidebar_follows_the_shell_directory() {
        let session = SessionConfig {
            id: 1,
            name: "ssh-1".into(),
            session_type: SessionType::SSH,
            ..Default::default()
        };
        let page = page_content(&Rc::new(session));
        let backend = create_terminal().backend;
        let terminal = backend.widget();
        set_terminal_area(&page, &terminal);
        follow_terminal_directory(&page, &terminal);
        let sidebar = SessionTab::sidebar_for_page(&page).expect("an SSH page's sidebar");

        let report = |uri: &str| {
            backend.write(format!("\x1b]7;{uri}\x07").as_bytes());
            // VTE parses fed bytes on a timer, not inline.
            for _ in 0..100 {
                glib::MainContext::default().iteration(false);
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        report("file://box/srv/my%20site");
        assert_eq!(sidebar.path_text(), "/srv/my site");
        report("file://inner/tmp");
        assert_eq!(sidebar.path_text(), "/srv/my site", "another host");
        report("file://box/var/log");
        assert_eq!(sidebar.path_text(), "/var/log");
    }

    /// The connect flow end to end on a local shell: opening the tab wires
    /// a terminal and its input tap, the page reports Connected, and
    /// closing the tab lets the page go.
    pub(in crate::ui) fn a_local_shell_tab_connects_and_closes() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let _entered = runtime.enter();
        let notebook = gtk4::Notebook::new();
        let window = gtk4::Window::new();
        window.set_child(Some(&notebook));
        window.present();
        let session = SessionConfig {
            id: 7_001,
            name: "local".into(),
            session_type: SessionType::LocalShell,
            local_shell: Some("/bin/sh".into()),
            ..Default::default()
        };
        let store = Rc::new(SessionStore::in_memory().expect("in-memory store"));

        let page = SessionTab::open(&notebook, &session, &store);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while SessionTab::page_status(&page).map(|status| status.state)
            != Some(TransportState::Connected)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the local shell never connected"
            );
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(current_session_id(&notebook), Some(7_001));
        assert!(SessionTab::active_terminal(&notebook).is_some());
        assert!(SessionTab::active_input_tap(&notebook).is_some());
        assert!(
            SessionTab::sidebar_for_page(&page).is_none(),
            "only SSH pages carry a sidebar"
        );
        let duplicate = SessionTab::open(&notebook, &session, &store);
        assert_ne!(duplicate, page, "a second open adds a tab");
        assert_eq!(notebook.n_pages(), 2);
        assert_ne!(tab_id_of(&duplicate), tab_id_of(&page));
        close_tab(&notebook, &duplicate);
        drop(duplicate);

        let gone = page.downgrade();
        close_tab(&notebook, &page);
        drop(page);
        for _ in 0..20 {
            glib::MainContext::default().iteration(false);
        }
        assert_eq!(notebook.n_pages(), 0);
        assert!(gone.upgrade().is_none(), "a closed tab's page is freed");

        // A failed connect shows the error view, whose buttons hold the
        // page weakly: closing that tab frees its page too.
        let broken = SessionConfig {
            id: 7_002,
            name: "broken".into(),
            local_shell: Some("/nonexistent/wayterm-shell".into()),
            ..session
        };
        let page = SessionTab::open(&notebook, &broken, &store);
        let shows_error = |page: &gtk4::Widget| {
            page.first_child()
                .and_then(|view| view.first_child())
                .and_downcast::<gtk4::Label>()
                .is_some_and(|heading| heading.text() == "Connection failed")
        };
        while !shows_error(&page) {
            assert!(
                std::time::Instant::now() < deadline,
                "the missing shell never failed"
            );
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..50 {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            matches!(
                SessionTab::page_status(&page).map(|status| status.state),
                Some(TransportState::Error(ref message)) if message.contains("shell not found")
            ),
            "the status ends on the error, not on the transport's Disconnected"
        );
        let gone = page.downgrade();
        close_tab(&notebook, &page);
        drop(page);
        for _ in 0..20 {
            glib::MainContext::default().iteration(false);
        }
        assert!(
            gone.upgrade().is_none(),
            "a tab closed on its error is freed"
        );
        window.destroy();
    }
}
