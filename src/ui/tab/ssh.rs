//! SSH tabs (doc 04): validate the row, show "Connecting…", get a shared
//! connection from the window's supervisor — directly or through the jump
//! host (doc 11 §Jump hosts) — while prompts run on the GTK thread and the
//! dial waits on a blocking-pool thread. Once connected, the page arms the
//! forward engine (doc 11 §Port forwarding) and attaches its SFTP sidebar
//! (doc 09) to the same connection.

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
use crate::store::model::{ForwardRule, HostKeyPolicy, SessionConfig};
use crate::store::SessionStore;
use crate::transport::connection::SshConnection;
use crate::transport::forward::{ForwardEngine, ForwardEvent};
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::jump;
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth};
use crate::transport::types::{TransportError, TransportState};
use crate::ui::auth_dialog;
use crate::ui::hostkey_dialog;
use crate::ui::main_window::SUPERVISOR_KEY;
use crate::ui::sftp_sidebar::{self, DirectoryFollower, SftpSidebar};
use crate::ui::side_dock;
use crate::ui::tunnels_dialog::EngineStatusBridge;
use crate::ui::widgets::{data, data_or_default, window_of};

/// Page data key: the page's [`SshPageState`].
const SSH_PAGE_KEY: &str = "wayterm-ssh-page";
/// Engine event channel capacity: status snapshots, never blocking.
const FWD_EVENT_BUF: usize = 64;
/// After Enter, how long the terminal must stay quiet before the SFTP
/// sidebar asks where the shell is (doc 09 §Following the shell).
const PROBE_QUIET: Duration = Duration::from_millis(300);

/// What an SSH page keeps beside its [`PageState`](super::PageState).
/// Main thread only.
#[derive(Default)]
struct SshPageState {
    /// The page's sidebar, and whether it holds the connection.
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

/// `page`'s SFTP sidebar, if it is an SSH page.
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

/// A new page's sidebar, hidden while `sftp_on_connect` is off.
pub(super) fn install_sidebar(page: &gtk4::Widget) {
    let sidebar = if sftp_sidebar::open_on_connect() {
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
/// rules on the fresh connection (doc 11 §Port forwarding), and attach the
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
        // Doc 11 v1: no SFTP task runs over a jumped connection; the
        // sidebar shows the limitation instead of attaching.
        sidebar.show_jump_placeholder();
    } else {
        sidebar.follow_new_shell(ShellCwdProbe::new(&conn, session.tmux_remote_name()));
        sidebar.attach_on_connect(conn);
        ssh_state(page).sidebar_attached.set(true);
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
    ssh_state(page).forward.replace(Some(bridge.clone()));
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
pub(super) fn stop_forward_engine(page: &gtk4::Widget) {
    if let Some(bridge) = existing_ssh_state(page).and_then(|ssh| ssh.forward.take()) {
        bridge.engine().stop();
    }
}

/// Let go of the sidebar's SFTP task (or the connection it keeps for its
/// first show) by swapping in a fresh sidebar that keeps the old one's
/// shown and following state.
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
    let sidebar = if old.is_shown() {
        SftpSidebar::new()
    } else {
        SftpSidebar::hidden()
    };
    sidebar.set_following(old.is_following());
    SessionTab::set_sidebar_for_page(page, sidebar);
    side_dock::sidebar_replaced(notebook);
}

/// Keep the page's SFTP sidebar on the shell's working directory (doc 09
/// §Following the shell): VTE reports OSC 7 as `current-directory-uri`,
/// and after each command — Enter, then [`PROBE_QUIET`] without output —
/// the sidebar may ask the remote instead. The sidebar is looked up each
/// time, since a redial replaces it.
pub(super) fn follow_terminal_directory(page: &gtk4::Widget, terminal: &gtk4::Widget) {
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
        if let Some(sidebar) = sidebar_of(&page) {
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
            if let Some(sidebar) = page.upgrade().and_then(|page| sidebar_of(&page)) {
                sidebar.probe_shell();
            }
        });
        *quiet.borrow_mut() = Some(source);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{ForwardKind, SessionType};

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
