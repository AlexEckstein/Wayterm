//! Session tabs and the SSH connect flow (doc 04).
//!
//! An SSH tab's page is a paned: the SFTP sidebar (doc 09) on the left —
//! per tab, since each browses its own connection — and the terminal area
//! on the right. Every other type gets the area alone: a terminal for
//! local and serial, the remote desktop for VNC and RDP (`vnc_tab`,
//! `rdp_tab`).
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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender, WeakUnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use vte4::prelude::*;
use zeroize::Zeroizing;

use crate::store::model::{AuthMethod, ForwardRule, HostKeyPolicy, SessionConfig, SessionType};
use crate::store::SessionStore;
use crate::terminal::{create_terminal, TerminalBackend};
use crate::transport::connection::{self, SshConnection};
use crate::transport::credentials::{
    BitwardenSource, CredentialManager, CredentialSource, KeyringSource, PromptSource,
};
use crate::transport::forward::{ForwardEngine, ForwardEvent};
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::jump::{self, HopInputs};
use crate::transport::reconnect::{ConnectFn, ConnectFuture};
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use crate::transport::tab::TerminalTab;
use crate::transport::types::{TransportError, TransportEvent, TransportState};
use crate::ui::auth_dialog::{self, ChallengePrompt};
use crate::ui::hostkey_dialog;
use crate::ui::main_window::{route_status, SUPERVISOR_KEY};
use crate::ui::rdp_tab;
use crate::ui::session_ended::{self, EndedChoice};
use crate::ui::settings::credentials::CredentialSettings;
use crate::ui::sftp_sidebar::{self, supports_sftp, SftpSidebar};
use crate::ui::tab_label;
use crate::ui::tab_resilience;
use crate::ui::tray;
use crate::ui::tunnels_dialog::{EngineStatusBridge, ForwardStatusSource};
use crate::ui::vnc_tab;
use crate::ui::widgets::{centered_label, data, data_or_default, set_data, set_margins, window_of};

/// The session id on a tab's page (read back by tab persistence).
pub const SESSION_ID_KEY: &str = "wayterm-session-tab-id";
// Page data keys.
const SIDEBAR_KEY: &str = "wayterm-sftp-sidebar";
const TAB_LABEL_KEY: &str = "wayterm-tab-label";
const PAGE_STATUS_KEY: &str = "wayterm-page-status";
const TERM_TAB_KEY: &str = "wayterm-terminal-tab";
const CONNECT_GEN_KEY: &str = "wayterm-connect-gen";
const TERMINAL_BACKEND_KEY: &str = "wayterm-terminal-backend";
const SESSION_KEY: &str = "wayterm-session-config";
const PAGE_EVENTS_KEY: &str = "wayterm-page-events";
const REDIAL_KEY: &str = "wayterm-redial-reply";
const SIDEBAR_ATTACHED_KEY: &str = "wayterm-sftp-sidebar-attached";
const FOCUS_ON_CONNECT_KEY: &str = "wayterm-focus-on-connect";
/// The store the page resolves its jump host through (set by `open`).
const STORE_KEY: &str = "wayterm-session-store";
/// The page's live forward status source, when its engine is armed.
const FORWARD_KEY: &str = "wayterm-forward-bridge";
/// Engine event channel capacity: status snapshots, never blocking.
const FWD_EVENT_BUF: usize = 64;
// Notebook data key.
const TEARDOWN_HOOK_KEY: &str = "wayterm-teardown-hook";

const SIDEBAR_WIDTH: i32 = 320;
/// An unallocated VTE reports 24×80, so measuring waits (≈2 s at most)
/// for the widget to get a size.
const ALLOC_WAIT_STEPS: u32 = 125;

/// The last state a page published, replayed into the status bar when the
/// page becomes current.
pub(crate) struct PageStatus {
    pub(crate) state: TransportState,
    pub(crate) host: String,
    pub(crate) port: u16,
}

pub(crate) type PageStatusCell = Rc<RefCell<PageStatus>>;
type TerminalSlot = Rc<RefCell<Option<TerminalTab>>>;
/// A driver-driven redial waiting for its attempt's verdict, tagged with
/// that attempt's generation.
type RedialReply = oneshot::Sender<Result<(), TransportError>>;
type RedialSlot = Rc<RefCell<Option<(u64, RedialReply)>>>;

/// Opening and querying session tabs.
pub struct SessionTab;

impl SessionTab {
    /// Open a tab for `session`, or focus the one already open; a new tab
    /// starts connecting. Returns the page widget. `store` is kept on the
    /// page so the connect flow can resolve the session's jump host at dial
    /// time (doc 11 §Jump hosts).
    pub fn open(
        notebook: &gtk4::Notebook,
        session: &SessionConfig,
        store: &Rc<SessionStore>,
    ) -> gtk4::Widget {
        if let Some((page_num, page)) = find_tab(notebook, session.id) {
            notebook.set_current_page(Some(page_num));
            focus_terminal(notebook, &page);
            return page;
        }
        let session = Rc::new(session.clone());
        let page = page_content(&session);
        let tab_label = build_tab_label(notebook, &session);
        let page_num = notebook.append_page(&page, Some(&tab_label));
        notebook.set_current_page(Some(page_num));
        set_data(&page, TAB_LABEL_KEY, tab_label);
        set_data(&page, SESSION_KEY, session.clone());
        set_data(&page, STORE_KEY, store.clone());
        install_teardown_on_remove(notebook);
        start_connect(notebook, &page, &session);
        page
    }

    /// The current page's SFTP sidebar (what `toggle_sftp` acts on).
    pub fn active_sidebar(notebook: &gtk4::Notebook) -> Option<SftpSidebar> {
        Self::sidebar_for_page(&current_page(notebook)?)
    }

    pub fn sidebar_for_page(page: &gtk4::Widget) -> Option<SftpSidebar> {
        data(page, SIDEBAR_KEY)
    }

    pub(crate) fn page_status(page: &gtk4::Widget) -> Option<PageStatusCell> {
        data(page, PAGE_STATUS_KEY)
    }

    /// The current tab's live forward status source, while its engine is
    /// armed. Tabs without one (offline, non-SSH, never connected) get the
    /// Tunnels dialog's offline fallback instead.
    pub(crate) fn current_forward_source(
        notebook: &gtk4::Notebook,
    ) -> Option<Arc<dyn ForwardStatusSource>> {
        let page = current_page(notebook)?;
        data::<Option<Arc<EngineStatusBridge>>>(&page, FORWARD_KEY)?
            .map(|bridge| bridge as Arc<dyn ForwardStatusSource>)
    }

    /// The current tab's terminal, once connected (the terminal actions'
    /// target).
    pub fn active_terminal(notebook: &gtk4::Notebook) -> Option<Arc<dyn TerminalBackend>> {
        data(&current_page(notebook)?, TERMINAL_BACKEND_KEY)
    }
}

fn current_page(notebook: &gtk4::Notebook) -> Option<gtk4::Widget> {
    notebook.nth_page(Some(notebook.current_page()?))
}

/// The session id of the current page, if it is a session tab.
pub(crate) fn current_session_id(notebook: &gtk4::Notebook) -> Option<i64> {
    data(&current_page(notebook)?, SESSION_ID_KEY)
}

fn find_tab(notebook: &gtk4::Notebook, session_id: i64) -> Option<(u32, gtk4::Widget)> {
    (0..notebook.n_pages()).find_map(|page_num| {
        let page = notebook.nth_page(Some(page_num))?;
        (data::<i64>(&page, SESSION_ID_KEY) == Some(session_id)).then_some((page_num, page))
    })
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
    focus_on_connect(page).set(true);

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

    let filled = |value: &Option<String>| value.clone().filter(|v| !v.trim().is_empty());
    let Some(host) = filled(&session.host) else {
        return show_error_view(
            notebook,
            page,
            session,
            "This session has no host configured.",
        );
    };
    let Some(username) = filled(&session.username) else {
        return show_error_view(
            notebook,
            page,
            session,
            "This session has no username configured.",
        );
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

    let policy = session
        .host_key_policy
        .clone()
        .unwrap_or(HostKeyPolicy::Strict);
    let verifier = HostKeyVerifier::new(policy, hostkey_dialog::host_key_prompt(), None);
    let responder = auth_dialog::challenge_responder(session.name.clone());
    let store = page_store(page);
    let creds = credential_manager(session.id, &session.name, store.as_deref());
    let method = transport_auth_method(session);
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
        // Each hop authenticates with its own session's method and
        // credentials (doc 11): fresh managers, verifiers and responders.
        let jump_policy = jump_cfg
            .host_key_policy
            .clone()
            .unwrap_or(HostKeyPolicy::Strict);
        let jump_verifier =
            HostKeyVerifier::new(jump_policy, hostkey_dialog::host_key_prompt(), None);
        let jump_responder = auth_dialog::challenge_responder(jump_cfg.name.clone());
        let jump_creds = credential_manager(jump_cfg.id, &jump_cfg.name, store.as_deref());
        // Plain data: the blocking pool owns this copy (`Rc` is not `Send`).
        let target: SessionConfig = (**session).clone();
        tokio::task::spawn_blocking(move || {
            let jump_hop = HopInputs {
                creds: &jump_creds,
                verifier: jump_verifier,
                responder: Some(jump_responder),
            };
            let target_hop = HopInputs {
                creds: &creds,
                verifier,
                responder: Some(responder),
            };
            tokio::runtime::Handle::current().block_on(jump::dial_with_jump(
                &supervisor,
                &jump_cfg,
                jump_hop,
                &target,
                target_hop,
            ))
        })
    } else {
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(supervisor.get_or_connect(
                &params,
                &method,
                &creds,
                verifier,
                Some(responder),
            ))
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
                set_terminal_area(&page, &centered_label("Disconnected"));
                return publish_status(&page, TransportState::Disconnected, &host, port);
            }
            Ok(Err(err)) => err.to_string(),
            Err(join_err) => format!("connect task failed: {join_err}"),
        };
        show_error_view(&notebook, &page, &session, &error);
        publish_status(&page, TransportState::Error(error), &host, port);
    });
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
    data(page, STORE_KEY)
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
    show_error_view(notebook, page, session, &message);
    publish_status(page, TransportState::Error(message), host, port);
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
/// §Port forwarding): start the session's rules on the runtime — never
/// awaited on the GTK thread — and bridge the event stream into the
/// page's live status source for the Tunnels dialog. Replaces any engine
/// still on the page, so a redial never stacks listeners.
fn arm_forward_engine(page: &gtk4::Widget, session: &Rc<SessionConfig>, conn: &Arc<SshConnection>) {
    stop_forward_engine(page);
    let (tx, rx) = mpsc::channel::<ForwardEvent>(FWD_EVENT_BUF);
    let engine = ForwardEngine::new(conn.clone(), tx);
    let bridge = Arc::new(EngineStatusBridge::new(engine.clone()));
    set_data(page, FORWARD_KEY, Some(bridge.clone()));
    let rules = forward_rules_for(session);
    tokio::spawn(async move {
        engine.start(rules).await;
    });
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
    let current: Option<Option<Arc<EngineStatusBridge>>> = data(page, FORWARD_KEY);
    let Some(Some(bridge)) = current else {
        return;
    };
    let engine = bridge.engine();
    tokio::spawn(async move {
        engine.stop().await;
    });
    set_data(page, FORWARD_KEY, None::<Arc<EngineStatusBridge>>);
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
    let wired = TerminalTab::new(session, conn.clone(), handle, &main_ctx, rows, cols);
    let (terminal_tab, state_rx) = match wired {
        Ok(wired) => wired,
        Err(err) => return show_error_view(notebook, page, session, &err.to_string()),
    };
    let (host, port) = status_endpoint(session, terminal_tab.description());
    terminal_slot(page).replace(Some(terminal_tab));
    set_data(page, TERMINAL_BACKEND_KEY, backend);
    let cancel =
        tab_resilience::register_connected_tab(session.id, redial_fn(notebook, page, session.id));
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
            sidebar.attach_on_connect(conn);
            sidebar_attached(page).set(true);
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
    let requested = focus_on_connect(page).replace(false);
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
    if let Some(backend) = data::<Arc<dyn TerminalBackend>>(page, TERMINAL_BACKEND_KEY) {
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
    if let Some(events) = data(page, PAGE_EVENTS_KEY) {
        return events;
    }
    let (events, rx) = mpsc::unbounded_channel();
    // The page holds the only lasting sender: the drain ends with the page.
    set_data(page, PAGE_EVENTS_KEY, events.clone());
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
    let alerts = window_of(page)
        .and_then(|window| window.application())
        .map(|app| tray::attach_disconnect_alerts(&app, session.name.clone()));
    let (notebook, session, page) = (notebook.clone(), session.clone(), page.downgrade());
    glib::MainContext::default().spawn_local(async move {
        while let Some(event) = rx.recv().await {
            let Some(page) = page.upgrade() else { break };
            match event {
                TransportEvent::StateChanged(state) => {
                    if state == TransportState::Connected {
                        settle_redial(&page, Ok(()));
                    }
                    tab_resilience::set_tab_state(session.id, state.clone());
                    publish_status(&page, state, &host, port);
                }
                TransportEvent::Error(message) => {
                    let state = TransportState::Error(message.clone());
                    tab_resilience::set_tab_state(session.id, state.clone());
                    show_error_view(&notebook, &page, &session, &message);
                    publish_status(&page, state, &host, port);
                }
                TransportEvent::ConnectionLost => {
                    publish_status(&page, TransportState::Disconnected, &host, port);
                    if let Some(alerts) = &alerts {
                        let _ = alerts.send(TransportEvent::ConnectionLost);
                    }
                    let Some(events) = events.upgrade() else {
                        break;
                    };
                    if tab_resilience::on_connection_lost(session.id, events).is_some() {
                        // Free the dead session during the first backoff.
                        release_connection(&page);
                    }
                }
                TransportEvent::SessionEnded => {
                    tab_resilience::set_tab_state(session.id, TransportState::Disconnected);
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
fn redial_fn(notebook: &gtk4::Notebook, page: &gtk4::Widget, session_id: i64) -> ConnectFn {
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
        let cancel = tab_resilience::live_tab(session_id).map(|tab| tab.cancel);
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
    let Some(session) = data::<Rc<SessionConfig>>(page, SESSION_KEY) else {
        return;
    };
    if notebook.page_num(page).is_none() {
        return;
    }
    release_connection(page);
    // `start_connect` takes the next generation first thing.
    redial_slot(page).replace(Some((current_gen(page).wrapping_add(1), reply)));
    start_connect(notebook, page, &session);
    focus_on_connect(page).set(false);
}

/// Let go of the page's hold on its connection — the transport and the
/// sidebar's SFTP task (or the connection it keeps for its first show) — so
/// the supervisor can evict a dead session and the next attempt dials fresh;
/// a live shared one is simply handed out again.
fn release_connection(page: &gtk4::Widget) {
    stop_forward_engine(page);
    close_terminal(page);
    if !sidebar_attached(page).replace(false) {
        return;
    }
    let (Some(paned), Some(old)) = (
        page.downcast_ref::<gtk4::Paned>(),
        SessionTab::sidebar_for_page(page),
    ) else {
        return;
    };
    let sidebar = SftpSidebar::new();
    if !old.widget().is_visible() {
        sidebar.toggle_visible();
    }
    let position = paned.position();
    paned.set_start_child(Some(sidebar.widget()));
    paned.set_position(position);
    set_data(page, SIDEBAR_KEY, sidebar);
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
        if let Some(session_id) = data::<i64>(page, SESSION_ID_KEY) {
            bump_gen(page);
            // Closing by any path stops the engine deterministically
            // (`close_tab` removes the page, so it lands here too).
            stop_forward_engine(page);
            tab_resilience::teardown_tab(session_id);
        }
    });
}

/// Keep the tab label on the terminal's window title (falling back to the
/// session name), unless the user has renamed the tab.
fn follow_terminal_title(page: &gtk4::Widget, terminal: &gtk4::Widget, session_name: &str) {
    let (Some(label), Some(vte)) = (
        data::<gtk4::Widget>(page, TAB_LABEL_KEY),
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

/// The terminal's title, or the session name when it is blank.
fn tab_title(session_name: &str, terminal_title: &str) -> String {
    match terminal_title.trim() {
        "" => session_name.to_string(),
        title => title.to_string(),
    }
}

/// Map the stored method onto the auth chain's input. Secrets are never on
/// the row: the chain asks the credential manager at connect time.
fn transport_auth_method(session: &SessionConfig) -> connection::AuthMethod {
    match session.auth_method.clone().unwrap_or(AuthMethod::Password) {
        AuthMethod::Password => connection::AuthMethod::Password(String::new()),
        AuthMethod::Key => connection::AuthMethod::Key {
            path: session.key_path.clone().unwrap_or_default(),
            passphrase: None,
        },
        AuthMethod::Agent => connection::AuthMethod::Agent,
        AuthMethod::KeyboardInteractive => connection::AuthMethod::KeyboardInteractive,
        AuthMethod::None => connection::AuthMethod::None,
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

// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn set_terminal_area(page: &gtk4::Widget, child: &impl IsA<gtk4::Widget>) {
    if let Some(paned) = page.downcast_ref::<gtk4::Paned>() {
        paned.set_end_child(Some(child));
    }
}

/// A centered column for the status views.
fn view_box() -> gtk4::Box {
    let view = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    view.set_vexpand(true);
    view.set_hexpand(true);
    view.set_valign(gtk4::Align::Center);
    view.set_halign(gtk4::Align::Center);
    view
}

// used by ui::vnc_tab
pub(crate) fn show_connecting_view(page: &gtk4::Widget, host: &str, port: u16) {
    let view = view_box();
    view.append(&centered_label(&format!("Connecting to {host}:{port}…")));
    let cancel = gtk4::Button::with_label("Cancel");
    let (page_for_cancel, host) = (page.clone(), host.to_string());
    cancel.connect_clicked(move |_| {
        bump_gen(&page_for_cancel);
        close_terminal(&page_for_cancel);
        set_terminal_area(&page_for_cancel, &centered_label("Disconnected"));
        publish_status(&page_for_cancel, TransportState::Disconnected, &host, port);
    });
    view.append(&cancel);
    set_terminal_area(page, &view);
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

    let view = view_box();
    set_margins(&view, 24);
    let heading = gtk4::Label::new(Some("Connection failed"));
    heading.add_css_class("heading");
    let detail = gtk4::Label::new(Some(message));
    detail.set_wrap(true);
    detail.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    detail.set_selectable(true);
    detail.set_justify(gtk4::Justification::Center);

    let retry = gtk4::Button::with_label("Retry");
    retry.add_css_class("suggested-action");
    let (n, p, s) = (notebook.clone(), page.clone(), session.clone());
    retry.connect_clicked(move |_| start_connect(&n, &p, &s));
    let close = gtk4::Button::with_label("Close Tab");
    let (notebook, page_to_close) = (notebook.clone(), page.clone());
    close.connect_clicked(move |_| close_tab(&notebook, &page_to_close));

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    buttons.set_halign(gtk4::Align::Center);
    buttons.append(&retry);
    buttons.append(&close);
    view.append(&heading);
    view.append(&detail);
    view.append(&buttons);
    set_terminal_area(page, &view);
}

/// The shell exited on its own: keep the terminal and its scrollback, add
/// the notice, and let `r` reconnect (as Retry does) or `q` close the tab.
fn show_session_ended(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    close_terminal(page);
    let Some(backend) = data::<Arc<dyn TerminalBackend>>(page, TERMINAL_BACKEND_KEY) else {
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
                if let Some(session) = data::<Rc<SessionConfig>>(&page, SESSION_KEY) {
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
    let status = SessionTab::page_status(page).unwrap_or_else(|| {
        let cell = Rc::new(RefCell::new(PageStatus {
            state: TransportState::Disconnected,
            host: String::new(),
            port,
        }));
        set_data(page, PAGE_STATUS_KEY, cell.clone());
        cell
    });
    *status.borrow_mut() = PageStatus {
        state: state.clone(),
        host: host.to_string(),
        port,
    };
    if let Some(window) = window_of(page).and_then(|w| w.downcast().ok()) {
        route_status(&window, page, state, host, port);
    }
}

/// [`publish_status`] with the endpoint the page last published.
fn publish_page_state(page: &gtk4::Widget, state: TransportState) {
    let (host, port) = SessionTab::page_status(page)
        .map(|status| {
            let status = status.borrow();
            (status.host.clone(), status.port)
        })
        .unwrap_or_default();
    publish_status(page, state, &host, port);
}

fn terminal_slot(page: &gtk4::Widget) -> TerminalSlot {
    data_or_default(page, TERM_TAB_KEY)
}

/// Tear down the page's terminal, if any, in the background.
fn close_terminal(page: &gtk4::Widget) {
    if let Some(mut tab) = terminal_slot(page).take() {
        glib::MainContext::default().spawn_local(async move { tab.close().await });
    }
}

/// Invalidate any attempt in flight; returns the new generation. A redial
/// waiting on an older attempt was superseded (Cancel, Retry, close) and
/// ends the driver's run.
fn bump_gen(page: &gtk4::Widget) -> u64 {
    let slot = gen_slot(page);
    slot.set(slot.get().wrapping_add(1));
    let gen = slot.get();
    let stale = redial_slot(page).borrow_mut().take_if(|(g, _)| *g != gen);
    if let Some((_, reply)) = stale {
        let _ = reply.send(Err(TransportError::Cancelled));
    }
    gen
}

/// Answer a redial waiting on the current attempt, if any.
fn settle_redial(page: &gtk4::Widget, result: Result<(), TransportError>) {
    let gen = current_gen(page);
    let pending = redial_slot(page).borrow_mut().take_if(|(g, _)| *g == gen);
    if let Some((_, reply)) = pending {
        let _ = reply.send(result);
    }
}

fn redial_slot(page: &gtk4::Widget) -> RedialSlot {
    data_or_default(page, REDIAL_KEY)
}

fn sidebar_attached(page: &gtk4::Widget) -> Rc<Cell<bool>> {
    data_or_default(page, SIDEBAR_ATTACHED_KEY)
}

fn focus_on_connect(page: &gtk4::Widget) -> Rc<Cell<bool>> {
    data_or_default(page, FOCUS_ON_CONNECT_KEY)
}

// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn current_gen(page: &gtk4::Widget) -> u64 {
    gen_slot(page).get()
}

fn gen_slot(page: &gtk4::Widget) -> Rc<Cell<u64>> {
    data_or_default(page, CONNECT_GEN_KEY)
}

/// The page: a paned whose end child is the session's area, with the SFTP
/// sidebar at the start for SSH (hidden while `sftp_on_connect` is off).
/// Tagged with the session id for dedup and persistence.
fn page_content(session: &SessionConfig) -> gtk4::Widget {
    let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
    paned.set_end_child(Some(&centered_label(&format!(
        "{}\nnot connected",
        session.name
    ))));
    paned.set_vexpand(true);
    paned.set_hexpand(true);
    if supports_sftp(&session.session_type) {
        let sidebar = SftpSidebar::new();
        if !sftp_sidebar::open_on_connect() {
            sidebar.toggle_visible();
        }
        paned.set_position(SIDEBAR_WIDTH);
        paned.set_start_child(Some(sidebar.widget()));
        set_data(&paned, SIDEBAR_KEY, sidebar);
    }
    set_data(&paned, SESSION_ID_KEY, session.id);
    paned.upcast()
}

/// The tab label; closing asks first when the transport is live (doc 04
/// §Lifecycle, `confirm_close`).
fn build_tab_label(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
    let notebook = notebook.clone();
    let session_id = session.id;
    let (label, _) = tab_label::build_tab_label(&session.name, move || {
        let notebook = notebook.clone();
        let parent = window_of(&notebook);
        crate::ui::tab_resilience::request_close(session_id, parent.as_ref(), move || {
            if let Some((page_num, _)) = find_tab(&notebook, session_id) {
                notebook.remove_page(Some(page_num));
            }
        });
    });
    label
}

#[cfg(test)]
mod tests;

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;
    use crate::hotkeys::engine::focus_within;

    /// Opening a session's tab hands keyboard focus from the session tree
    /// to that tab's terminal; a tab in the background never takes it.
    pub(in crate::ui) fn opening_a_tab_focuses_its_terminal() {
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
                let page = page_content(&session);
                let backend = create_terminal().backend;
                set_terminal_area(&page, &backend.widget());
                set_data(&page, TERMINAL_BACKEND_KEY, backend.clone());
                notebook.append_page(&page, None::<&gtk4::Widget>);
                (session, page, backend.widget())
            })
            .collect();
        window.present();
        for _ in 0..10 {
            glib::MainContext::default().iteration(false);
        }

        let [(first, first_page, first_terminal), (_, _, second_terminal)] = &tabs[..] else {
            unreachable!();
        };
        notebook.set_current_page(Some(1));
        tree.grab_focus();
        focus_terminal(&notebook, first_page);
        assert!(focus_within(&tree), "a background tab took focus");

        SessionTab::open(
            &notebook,
            first,
            &Rc::new(SessionStore::in_memory().expect("in-memory store")),
        );
        assert_eq!(notebook.current_page(), Some(0));
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
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.append(&tree);
        root.append(&notebook);
        let window = gtk4::Window::new();
        window.set_child(Some(&root));

        let session = SessionConfig {
            id: 1,
            name: "local-1".into(),
            session_type: SessionType::LocalShell,
            ..Default::default()
        };
        let page = page_content(&session);
        // Stands in for the SFTP sidebar's path entry, inside the page.
        let sidebar_entry = gtk4::Entry::new();
        page.downcast_ref::<gtk4::Paned>()
            .expect("page is a paned")
            .set_start_child(Some(&sidebar_entry));
        let backend = create_terminal().backend;
        let terminal = backend.widget();
        set_terminal_area(&page, &terminal);
        set_data(&page, TERMINAL_BACKEND_KEY, backend);
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
            focus_on_connect(&page).set(false);
            focus_after_connect(&notebook, &page);
            assert!(focus_within(holder), "a reconnect stole focus");
        }

        // Reconnect after the focused terminal went away with the connection.
        gtk4::prelude::GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
        focus_after_connect(&notebook, &page);
        assert!(focus_within(&terminal), "an unclaimed focus is taken back");

        // A user-started connect takes focus from the tree.
        tree.grab_focus();
        focus_on_connect(&page).set(true);
        focus_after_connect(&notebook, &page);
        assert!(focus_within(&terminal), "a user-started connect");
        assert!(!focus_on_connect(&page).get(), "the request is one-shot");
        window.destroy();
    }
}
