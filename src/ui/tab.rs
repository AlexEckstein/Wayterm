//! Session tabs and the SSH connect flow (doc 04).
//!
//! An SSH tab's page is a paned: the SFTP sidebar (doc 09) on the left —
//! per tab, since each browses its own connection — and the terminal area
//! on the right. Local and serial tabs get the terminal area alone; other
//! session types get a placeholder page.
//!
//! Connecting: validate the row, show "Connecting…", get a shared
//! connection from the window's supervisor (prompts run on the GTK thread
//! while the dial waits on a blocking-pool thread), then measure the real
//! terminal grid, wire the [`TerminalTab`], and attach the sidebar to the
//! same connection. Every attempt bumps a per-page generation, so a
//! cancelled or superseded attempt discards its result.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc::UnboundedReceiver;
use vte4::prelude::*;
use zeroize::Zeroizing;

use crate::store::model::{AuthMethod, HostKeyPolicy, SessionConfig, SessionType};
use crate::terminal::{create_terminal, TerminalBackend};
use crate::transport::connection::{self, SshConnection};
use crate::transport::credentials::{CredentialManager, KeyringSource, PromptSource};
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use crate::transport::tab::TerminalTab;
use crate::transport::types::{TransportError, TransportEvent, TransportState};
use crate::ui::auth_dialog::{self, ChallengePrompt};
use crate::ui::hostkey_dialog;
use crate::ui::main_window::{route_status, SUPERVISOR_KEY};
use crate::ui::rdp_tab;
use crate::ui::sftp_sidebar::{supports_sftp, SftpSidebar};
use crate::ui::tab_label;
use crate::ui::vnc_tab;
use crate::ui::widgets::{centered_label, data, set_data, set_margins, window_of};

/// The session id on a tab's page (read back by tab persistence).
pub const SESSION_ID_KEY: &str = "wayterm-session-tab-id";
// Page data keys.
const SIDEBAR_KEY: &str = "wayterm-sftp-sidebar";
const TAB_LABEL_KEY: &str = "wayterm-tab-label";
const PAGE_STATUS_KEY: &str = "wayterm-page-status";
const TERM_TAB_KEY: &str = "wayterm-terminal-tab";
const CONNECT_GEN_KEY: &str = "wayterm-connect-gen";
const TERMINAL_BACKEND_KEY: &str = "wayterm-terminal-backend";

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

/// Opening and querying session tabs.
pub struct SessionTab;

impl SessionTab {
    /// Open a tab for `session`, or focus the one already open. SSH, local
    /// and serial tabs start connecting. Returns the page widget.
    pub fn open(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
        if let Some((page_num, page)) = find_tab(notebook, session.id) {
            notebook.set_current_page(Some(page_num));
            return page;
        }
        let session = Rc::new(session.clone());
        let page = page_content(&session);
        let tab_label = build_tab_label(notebook, &session);
        let page_num = notebook.append_page(&page, Some(&tab_label));
        notebook.set_current_page(Some(page_num));
        set_data(&page, TAB_LABEL_KEY, tab_label);
        if matches!(
            session.session_type,
            SessionType::SSH
                | SessionType::LocalShell
                | SessionType::Serial
                | SessionType::VNC
                | SessionType::RDP
        ) {
            start_connect(notebook, &page, &session);
        }
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
    close_terminal(page);

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
    let creds = credential_manager(&session.name);
    let method = transport_auth_method(session);
    let params = ConnectParams {
        host: host.clone(),
        port,
        username,
    };
    // The prompt closures block until the user answers, so the dial must
    // not occupy a reactor worker.
    let dial = tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(supervisor.get_or_connect(
            &params,
            &method,
            &creds,
            verifier,
            Some(responder),
        ))
    });

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
            Ok(Err(TransportError::Cancelled | TransportError::Disconnected)) => {
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

/// Embed the terminal, measure its real grid, wire the transport, and
/// attach the sidebar to the same connection (SSH only: local and serial
/// pass no connection and get no sidebar).
async fn finish_connect(
    notebook: &gtk4::Notebook,
    session: &Rc<SessionConfig>,
    page: &gtk4::Widget,
    conn: Option<Arc<SshConnection>>,
) {
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

    let main_ctx = glib::MainContext::default();
    let wired = TerminalTab::new(session, conn.clone(), handle, &main_ctx, rows, cols);
    let (terminal_tab, state_rx) = match wired {
        Ok(wired) => wired,
        Err(err) => return show_error_view(notebook, page, session, &err.to_string()),
    };
    let (host, port) = status_endpoint(session, terminal_tab.description());
    terminal_slot(page).replace(Some(terminal_tab));
    set_data(page, TERMINAL_BACKEND_KEY, backend);
    spawn_state_drain(
        notebook.clone(),
        page.clone(),
        session.clone(),
        host,
        port,
        state_rx,
    );

    if let (Some(conn), Some(sidebar)) = (conn, SessionTab::sidebar_for_page(page)) {
        sidebar.attach(conn);
    }
    follow_terminal_title(page, &widget, &session.name);
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

/// Replay the tab's transport events into its page status and views.
fn spawn_state_drain(
    notebook: gtk4::Notebook,
    page: gtk4::Widget,
    session: Rc<SessionConfig>,
    host: String,
    port: u16,
    mut rx: UnboundedReceiver<TransportEvent>,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Some(event) = rx.recv().await {
            match event {
                TransportEvent::StateChanged(state) => publish_status(&page, state, &host, port),
                TransportEvent::Error(message) => {
                    show_error_view(&notebook, &page, &session, &message);
                    publish_status(&page, TransportState::Error(message), &host, port);
                }
                TransportEvent::ConnectionLost => {
                    publish_status(&page, TransportState::Disconnected, &host, port)
                }
                TransportEvent::DataReceived(_) => {}
            }
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

/// Keyring first, then a one-field challenge dialog (doc 11).
// used by ui::vnc_tab
pub(crate) fn credential_manager(session_name: &str) -> CredentialManager {
    let name = session_name.to_string();
    let prompt = PromptSource::new(Arc::new(move |_session_id, secret_type| {
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
            auth_dialog::run_challenges(&name, None, &prompts)
        })
        .flatten()
        .and_then(|answers| answers.into_iter().next())
        .map(|answer| Zeroizing::new(answer.into_bytes()))
    }));
    CredentialManager::new(vec![Box::new(KeyringSource), Box::new(prompt)])
}

// ---------------------------------------------------------------------------
// Page views and bookkeeping
// ---------------------------------------------------------------------------

// used by ui::vnc_tab
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
// used by ui::vnc_tab
pub(crate) fn show_error_view(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    message: &str,
) {
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
    close.connect_clicked(move |_| {
        bump_gen(&page_to_close);
        if let Some(page_num) = notebook.page_num(&page_to_close) {
            // Dropping the page cancels its terminal tasks.
            notebook.remove_page(Some(page_num));
        }
    });

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    buttons.set_halign(gtk4::Align::Center);
    buttons.append(&retry);
    buttons.append(&close);
    view.append(&heading);
    view.append(&detail);
    view.append(&buttons);
    set_terminal_area(page, &view);
}

/// Record `state` on the page and show it if the page is current.
// used by ui::vnc_tab
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

fn terminal_slot(page: &gtk4::Widget) -> TerminalSlot {
    data(page, TERM_TAB_KEY).unwrap_or_else(|| {
        let slot = TerminalSlot::default();
        set_data(page, TERM_TAB_KEY, slot.clone());
        slot
    })
}

/// Tear down the page's terminal, if any, in the background.
fn close_terminal(page: &gtk4::Widget) {
    if let Some(mut tab) = terminal_slot(page).take() {
        glib::MainContext::default().spawn_local(async move { tab.close().await });
    }
}

/// Invalidate any attempt in flight; returns the new generation.
fn bump_gen(page: &gtk4::Widget) -> u64 {
    let slot = gen_slot(page);
    slot.set(slot.get().wrapping_add(1));
    slot.get()
}

// used by ui::vnc_tab
pub(crate) fn current_gen(page: &gtk4::Widget) -> u64 {
    gen_slot(page).get()
}

fn gen_slot(page: &gtk4::Widget) -> Rc<Cell<u64>> {
    data(page, CONNECT_GEN_KEY).unwrap_or_else(|| {
        let slot = Rc::new(Cell::new(0));
        set_data(page, CONNECT_GEN_KEY, slot.clone());
        slot
    })
}

/// The page: the sidebar and terminal area for SSH, the terminal area alone
/// for local and serial, a placeholder otherwise. Tagged with the session id for dedup and persistence.
fn page_content(session: &SessionConfig) -> gtk4::Widget {
    let placeholder = centered_label(&format!("{}\nnot connected", session.name));
    let page: gtk4::Widget = if supports_sftp(&session.session_type) {
        let sidebar = SftpSidebar::new();
        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(SIDEBAR_WIDTH);
        paned.set_start_child(Some(sidebar.widget()));
        paned.set_end_child(Some(&placeholder));
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        set_data(&paned, SIDEBAR_KEY, sidebar);
        paned.upcast()
    } else if matches!(
        session.session_type,
        SessionType::LocalShell | SessionType::Serial | SessionType::VNC | SessionType::RDP
    ) {
        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_end_child(Some(&placeholder));
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        paned.upcast()
    } else {
        placeholder.upcast()
    };
    set_data(&page, SESSION_ID_KEY, session.id);
    page
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
mod tests {
    use super::*;

    #[test]
    fn the_tab_title_prefers_the_terminal_and_never_goes_blank() {
        assert_eq!(tab_title("db-prod", "root@db-prod:~"), "root@db-prod:~");
        assert_eq!(tab_title("db-prod", ""), "db-prod");
        assert_eq!(tab_title("db-prod", "   \n"), "db-prod");
    }

    #[test]
    fn status_endpoint_uses_the_description_only_without_a_host() {
        let ssh = SessionConfig {
            host: Some("db".into()),
            port: Some(2222),
            ..Default::default()
        };
        assert_eq!(status_endpoint(&ssh, "ignored"), ("db".into(), 2222));
        for session_type in [SessionType::LocalShell, SessionType::Serial] {
            let config = SessionConfig {
                session_type,
                ..Default::default()
            };
            assert_eq!(
                status_endpoint(&config, "/dev/ttyUSB0@115200"),
                ("/dev/ttyUSB0@115200".into(), 0)
            );
        }
    }

    #[test]
    fn auth_methods_map_without_secrets() {
        let mut config = SessionConfig {
            key_path: Some("/k/id_ed25519".into()),
            ..Default::default()
        };
        for (stored, expected) in [
            (None, "Password(\"\")"),
            (Some(AuthMethod::Password), "Password(\"\")"),
            (
                Some(AuthMethod::Key),
                "Key { path: \"/k/id_ed25519\", passphrase: None }",
            ),
            (Some(AuthMethod::Agent), "Agent"),
            (Some(AuthMethod::KeyboardInteractive), "KeyboardInteractive"),
            (Some(AuthMethod::None), "None"),
        ] {
            config.auth_method = stored;
            assert_eq!(format!("{:?}", transport_auth_method(&config)), expected);
        }
    }
}
