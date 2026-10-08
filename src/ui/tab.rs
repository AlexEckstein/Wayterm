//! Session tab — the notebook page for one saved session, and the connect
//! flow that turns an SSH page into a live terminal (task P3-3).
//!
//! Each tab represents an open saved session. The tab label is a small box
//! containing the session name plus a close button; opening the same session
//! twice focuses the existing tab instead of duplicating it.
//!
//! # Page layout (doc 09)
//!
//! An SSH session's page is a [`gtk4::Paned`]: the [`SftpSidebar`] on the
//! left, the terminal on the right, user-resizable — doc 09 §Layout's
//! "sidebar left, terminal right". The paned belongs to the **tab**, not to
//! the window, because doc 09 §Connection Model gives each sidebar the
//! session's own shared `SshConnection`: two SSH tabs browse two servers
//! independently, so a single window-wide sidebar could not represent either.
//! Non-SSH session types get the bare page content instead — doc 09
//! §Availability restricts the sidebar to SSH sessions, and the local/serial
//! transports are Phase 5 (doc 04).
//!
//! # Connect flow (P3-3, doc 04)
//!
//! Opening an SSH tab runs [`start_connect`]:
//!
//! 1. Validate host/username, show the "Connecting …" view, publish
//!    [`TransportState::Connecting`].
//! 2. Ask the window's [`ConnectionSupervisor`] for a shared
//!    [`SshConnection`] — auth (host-key prompt, credential dialogs) happens
//!    there, on a blocking pool thread per `auth_dialog`'s contract.
//! 3. [`finish_connect`] measures the real VTE grid (never 80×24), builds
//!    the [`TerminalTab`] pump/bridge wiring, attaches the SFTP sidebar to
//!    the *same* connection, and wires the terminal title to the tab label.
//! 4. The tab's state-drain future replays `TransportEvent`s: connection
//!    state → status bar (routed through the window's status router so only
//!    the current tab drives it), `Error` → error view with Retry.
//!
//! Every attempt bumps a per-page generation counter; a cancelled or
//! superseded attempt discards its result (the late connection is simply
//! dropped, which the supervisor's release monitor notices).
//!
//! P4-PAR-2 additions (additive only, P3-3 owns the connect flow):
//! [`SessionTab::attach_terminal`] / [`SessionTab::active_terminal`] are the
//! seam the connect flow uses to publish a tab's terminal backend and the
//! window-level terminal actions use to find it;
//! [`SessionTab::apply_terminal_title`] is the doc-02 `TitleChanged` seam.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::store::model::{
    AuthMethod as ModelAuthMethod, HostKeyPolicy, SessionConfig, SessionType,
};
use crate::terminal::{create_terminal, TerminalBackend};
use crate::transport::connection::{AuthMethod as TransportAuthMethod, SshConnection};
#[cfg(feature = "keyring")]
use crate::transport::credentials::KeyringSource;
use crate::transport::credentials::{CredentialManager, CredentialSource, PromptSource};
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use crate::transport::tab::TerminalTab;
use crate::transport::types::{TransportError, TransportEvent, TransportState};
use crate::ui::auth_dialog::{self, ChallengePrompt};
use crate::ui::hostkey_dialog;
use crate::ui::main_window::{route_status, SUPERVISOR_KEY};
use crate::ui::sftp_sidebar::{supports_sftp, SftpSidebar};
use crate::ui::tab_label;
use vte4::prelude::*;
use zeroize::Zeroizing;

/// Key used to stash the session id on the tab content widget for dedup.
///
/// Public because the tab-persistence layer reads the same value back
/// off a page to learn which session a tab belongs to (doc 10 Phase 4,
/// "Remember open tabs on exit, restore on launch").
pub const SESSION_ID_KEY: &str = "wayterm-session-tab-id";

/// Key used to stash the sidebar on an SSH tab's page widget so the
/// `toggle_sftp` action can find it — the same widget-data pattern
/// `session_tree.rs` uses to recover its own widget from a `TreeView`.
const SIDEBAR_KEY: &str = "wayterm-sftp-sidebar";

/// Key for the tab-label name [`gtk4::Label`], updated by the terminal's
/// `window-title-changed` signal (P3-3 deliverable 1f).
const TITLE_LABEL_KEY: &str = "wayterm-tab-title-label";

/// Key for the page's [`PageStatusCell`] — the stashed connection state the
/// `switch_page` handler replays into the status bar.
const PAGE_STATUS_KEY: &str = "wayterm-page-status";

/// Key for the page's terminal-tab slot ([`Rc<RefCell<Option<TerminalTab>>>`]),
/// taken by the close/cancel/error paths for an orderly teardown.
const TERM_TAB_KEY: &str = "wayterm-terminal-tab";

/// Key for the page's connect-generation [`Cell<u64>`]; bumped to invalidate
/// an in-flight attempt.
const CONNECT_GEN_KEY: &str = "wayterm-connect-gen";

/// Key used to stash the tab's wired terminal backend (an
/// `Arc<dyn TerminalBackend>`) on the tab content widget.
const TERMINAL_BACKEND_KEY: &str = "wayterm-terminal-backend";

/// Width of the sidebar when the tab is created, in pixels.
const SIDEBAR_WIDTH: i32 = 320;

/// How long [`finish_connect`] waits for the swapped-in terminal widget to
/// be allocated before falling back to the classic 24×80 grid. 125 × 16 ms
/// ≈ 2 s; allocation normally lands within one main-loop iteration.
const ALLOC_WAIT_STEPS: u32 = 125;

/// Fallback grid when the widget cannot be measured as a VTE terminal.
const FALLBACK_GRID: (u32, u32) = (24, 80);

/// Connection state + identity stashed on every SSH page so the window can
/// repaint the status bar when the page becomes current (P3-3 1g).
pub(crate) struct PageStatus {
    pub(crate) state: TransportState,
    pub(crate) host: String,
    pub(crate) port: u16,
}

/// Shared stash handle — main-thread only.
pub(crate) type PageStatusCell = Rc<RefCell<PageStatus>>;

/// Namespace for opening and querying session tabs.
pub struct SessionTab;

impl SessionTab {
    /// Open (or focus if already open) a tab for `session`.
    ///
    /// The tab label shows the session name and a close button. For SSH
    /// sessions the page becomes a live connection: the placeholder label is
    /// replaced by the connect flow (doc 04 / P3-3). Returns the tab content
    /// widget.
    pub fn open(notebook: &gtk4::Notebook, session: &SessionConfig) -> gtk4::Widget {
        // Dedup: if a tab for this session already exists, focus it.
        if let Some((page_num, widget)) = find_tab_by_session_id(notebook, session.id) {
            notebook.set_current_page(Some(page_num));
            return widget;
        }

        let session = Rc::new(session.clone());
        let page = page_content(&session);

        let (tab_label, name_label) = build_tab_label(notebook, &session);
        let page_num = notebook.append_page(&page, Some(&tab_label));
        notebook.set_current_page(Some(page_num));

        // The terminal-title wiring in `finish_connect` updates this label.
        unsafe {
            page.set_data(TITLE_LABEL_KEY, name_label);
        }

        if session.session_type == SessionType::SSH {
            start_connect(notebook, &page, &session);
        }

        page
    }

    /// The SFTP sidebar of the notebook's current page, if any.
    ///
    /// Doc 09's `toggle_sftp` acts on the **active** SSH tab, so this is what
    /// the window's toggle action resolves on every activation and on every
    /// page switch (which is also how the action knows to disable itself on
    /// RDP/VNC/serial/local pages — doc 09 §Availability).
    pub fn active_sidebar(notebook: &gtk4::Notebook) -> Option<SftpSidebar> {
        let page_num = notebook.current_page()?;
        let page = notebook.nth_page(Some(page_num))?;
        Self::sidebar_for_page(&page)
    }

    /// The SFTP sidebar of the page widget `page` belongs to, if it has one.
    pub fn sidebar_for_page(page: &gtk4::Widget) -> Option<SftpSidebar> {
        unsafe {
            page.data::<SftpSidebar>(SIDEBAR_KEY)
                .map(|data| data.as_ref().clone())
        }
    }

    /// The stashed [`PageStatus`] of `page`, if the page ever published one.
    /// Used by the window's `switch_page` handler to repaint the status bar.
    pub(crate) fn page_status(page: &gtk4::Widget) -> Option<PageStatusCell> {
        page_status_cell(page)
    }

    /// Publish `backend` as the terminal of the tab whose content widget is
    /// `content` (the connect flow's side of the seam; P3 calls this once a
    /// session is wired). The window-level terminal actions read it back
    /// through [`SessionTab::active_terminal`].
    // The connect flow (P3) is the caller; it lands in that lane.
    pub fn attach_terminal(content: &gtk4::Widget, backend: Arc<dyn TerminalBackend>) {
        // SAFETY: the key and value type have a 1:1 relationship; the qdata
        // (and its Arc) is dropped with the widget.
        unsafe {
            content.set_data(TERMINAL_BACKEND_KEY, backend);
        }
    }

    /// The terminal backend wired to the notebook's current tab, if the
    /// connect flow has attached one yet. `None` for the Home page and for
    /// placeholder tabs — window-level terminal actions must treat that as
    /// "nothing to act on" (doc 07 context=terminal).
    pub fn active_terminal(notebook: &gtk4::Notebook) -> Option<Arc<dyn TerminalBackend>> {
        let page = notebook.current_page()?;
        let content = notebook.nth_page(Some(page))?;
        // SAFETY: only `attach_terminal` writes this key, with this type.
        unsafe { content.data::<Arc<dyn TerminalBackend>>(TERMINAL_BACKEND_KEY) }
            .map(|ptr| unsafe { (*ptr.as_ptr()).clone() })
    }

    /// Doc 02 `TitleChanged` seam: feed a terminal-provided window title
    /// into the active tab's title state machine. Ignored when the user has
    /// already renamed the tab (see [`tab_label::TabTitle`]). Returns
    /// whether the title took effect.
    ///
    /// Not called yet — the pump drops `TerminalEvent::TitleChanged` today;
    /// the consumer wiring lands with the connect flow.
    pub fn apply_terminal_title(notebook: &gtk4::Notebook, title: &str) -> bool {
        let Some(page) = notebook.current_page() else {
            return false;
        };
        let Some(content) = notebook.nth_page(Some(page)) else {
            return false;
        };
        let Some(label) = notebook.tab_label(&content) else {
            return false;
        };
        tab_label::apply_terminal_title(&label, title)
    }
}

// ---------------------------------------------------------------------------
// Connect flow
// ---------------------------------------------------------------------------

/// Begin (or retry) the SSH connect flow for `page` (P3-3 deliverable 1b).
///
/// Invalidates any in-flight attempt for the page, tears down a previous
/// terminal, shows the connecting view, and spawns the supervisor dial; the
/// continuation awaits the result on the GTK main context and hands success
/// to [`finish_connect`].
fn start_connect(notebook: &gtk4::Notebook, page: &gtk4::Widget, session: &Rc<SessionConfig>) {
    // Invalidate the previous attempt (its continuation will discard its
    // result) and stop a previously wired terminal.
    let gen = bump_gen(page);
    if let Some(tab) = take_terminal_tab(page) {
        glib::MainContext::default().spawn_local(async move {
            let mut tab = tab;
            tab.close().await;
        });
    }

    let Some(host) = session.host.clone().filter(|h| !h.trim().is_empty()) else {
        show_error_view(
            notebook,
            page,
            session,
            "This session has no host configured.",
        );
        return;
    };
    let Some(username) = session.username.clone().filter(|u| !u.trim().is_empty()) else {
        show_error_view(
            notebook,
            page,
            session,
            "This session has no username configured.",
        );
        return;
    };
    let port = session.port.unwrap_or(22);

    // The supervisor and the status router live on the window; without a
    // window (bare-notebook unit contexts) there is nothing to connect to.
    let Some(window) = window_of(page) else {
        return;
    };
    let Some(supervisor) = supervisor_of(&window) else {
        show_error_view(
            notebook,
            page,
            session,
            "Connection supervisor unavailable.",
        );
        return;
    };

    show_connecting_view(page, session, &host, port);
    publish_status(page, TransportState::Connecting, &host, port);

    // Host-key verification and the challenge responder are the GTK dialog
    // closures (doc 11); credentials walk keyring → prompt (doc 11
    // §CredentialManager).
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

    // The dialog closures block for their whole lifetime
    // (`auth_dialog::run_on_gtk_main_thread`), so the dial runs on the
    // blocking pool, not a reactor worker — per that function's doc note.
    let connect_task = tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async move {
            supervisor
                .get_or_connect(&params, &method, &creds, verifier, Some(responder))
                .await
        })
    });

    let notebook = notebook.clone();
    let page = page.clone();
    let session = session.clone();
    glib::MainContext::default().spawn_local(async move {
        let result = connect_task.await;
        // Cancelled or superseded attempt: discard. Dropping a late
        // connection releases it to the supervisor's monitor.
        if current_gen(&page) != gen {
            if let Ok(Ok(conn)) = result {
                drop(conn);
            }
            return;
        }
        match result {
            Ok(Ok(conn)) => finish_connect(&notebook, &session, &page, conn).await,
            Ok(Err(err)) if is_neutral_failure(&err) => {
                // User cancelled a credential/host-key prompt: clean
                // Disconnected, no error screen (doc 11 §Flow, P3-3 3).
                show_disconnected_view(&page);
                publish_status(&page, TransportState::Disconnected, &host, port);
            }
            Ok(Err(err)) => {
                show_error_view(&notebook, &page, &session, &err.to_string());
                publish_status(&page, TransportState::Error(err.to_string()), &host, port);
            }
            Err(join_err) => {
                let message = format!("connect task failed: {join_err}");
                show_error_view(&notebook, &page, &session, &message);
                publish_status(&page, TransportState::Error(message), &host, port);
            }
        }
    });
}

/// Success path: embed the real terminal, measure it, wire the pump, attach
/// SFTP, and follow the terminal title (P3-3 deliverables 1b–1f).
///
/// Runs as a `spawn_local` future so it can wait for the widget allocation
/// without blocking the main loop — an unallocated VTE reports its default
/// 24×80 (probe-verified), so measuring must wait until `width/height > 0`.
async fn finish_connect(
    notebook: &gtk4::Notebook,
    session: &Rc<SessionConfig>,
    page: &gtk4::Widget,
    conn: Arc<SshConnection>,
) {
    // Swap the real terminal into the paned first, so it can be allocated.
    let handle = create_terminal();
    let widget = handle.backend.widget();
    set_terminal_area(page, &widget);

    let mut waited = 0u32;
    while widget.width() == 0 || widget.height() == 0 {
        if waited >= ALLOC_WAIT_STEPS {
            break;
        }
        waited += 1;
        glib::timeout_future(Duration::from_millis(16)).await;
    }
    let (rows, cols) = measure_grid(&widget);

    let main_ctx = glib::MainContext::default();
    let (terminal_tab, state_rx) = match TerminalTab::new(
        session.as_ref(),
        Some(conn.clone()),
        handle,
        &main_ctx,
        rows,
        cols,
    ) {
        Ok(wired) => wired,
        Err(err) => {
            show_error_view(notebook, page, session, &err.to_string());
            return;
        }
    };
    *ensure_term_tab_slot(page).borrow_mut() = Some(terminal_tab);
    spawn_state_drain(notebook.clone(), page.clone(), session.clone(), state_rx);

    // SFTP sidebar over the same shared connection (doc 09 §Connection
    // Model; P3-2's `NOT_CONNECTED` gate is replaced by this attach).
    if supports_sftp(&session.session_type) {
        if let Some(sidebar) = SessionTab::sidebar_for_page(page) {
            sidebar.attach(conn);
        }
    }

    wire_terminal_title(page, &widget, &session.name);
}

/// The tab's transport-state consumer (doc 04 §Wiring's second channel,
/// P3-3 deliverable 1g).
///
/// State and errors are always stashed in the page's [`PageStatus`]; the
/// status router additionally repaints the bar when this page is current.
fn spawn_state_drain(
    notebook: gtk4::Notebook,
    page: gtk4::Widget,
    session: Rc<SessionConfig>,
    mut rx: UnboundedReceiver<TransportEvent>,
) {
    let host = session.host.clone().unwrap_or_default();
    let port = session.port.unwrap_or(22);
    glib::MainContext::default().spawn_local(async move {
        while let Some(event) = rx.recv().await {
            match event {
                TransportEvent::StateChanged(state) => {
                    publish_status(&page, state, &host, port);
                }
                TransportEvent::Error(message) => {
                    show_error_view(&notebook, &page, &session, &message);
                    publish_status(&page, TransportState::Error(message), &host, port);
                }
                TransportEvent::ConnectionLost => {
                    // Reconnect/backoff is Phase 8; today this is a quiet
                    // return to Disconnected (no error overlay).
                    publish_status(&page, TransportState::Disconnected, &host, port);
                }
                TransportEvent::DataReceived(_) => {}
            }
        }
    });
}

/// Wire the VTE `window-title-changed` signal to the tab label.
///
/// The signal is wired directly on the widget instead of emitting
/// `TerminalEvent::TitleChanged`: the transport pump ignores that variant
/// (doc 04's `TransportEvent` is frozen), so a tab label fed by it could
/// never update. The backend event stays available for future consumers.
fn wire_terminal_title(page: &gtk4::Widget, terminal: &gtk4::Widget, session_name: &str) {
    let Some(name_label) = title_label(page) else {
        return;
    };
    let Some(vte) = terminal.downcast_ref::<vte4::Terminal>() else {
        return;
    };
    let name = session_name.to_string();
    let initial = vte
        .window_title()
        .map(|t| t.to_string())
        .unwrap_or_default();
    name_label.set_text(&tab_title(&name, &initial));
    vte.connect_window_title_changed(move |term| {
        let title = term
            .window_title()
            .map(|t| t.to_string())
            .unwrap_or_default();
        name_label.set_text(&tab_title(&name, &title));
    });
}

/// Map a session name + remote terminal title onto the tab label text.
///
/// Empty/whitespace-only titles fall back to the session name so the label
/// never goes blank (a remote that clears `OSC 0` must not erase the tab).
fn tab_title(session_name: &str, terminal_title: &str) -> String {
    let title = terminal_title.trim();
    if title.is_empty() {
        session_name.to_string()
    } else {
        title.to_string()
    }
}

/// Measure the VTE grid for [`TerminalTab`]'s initial PTY size — the widget's
/// *current* rows/columns, never a hardcoded 80×24 (doc 10, P3-3 1e).
fn measure_grid(widget: &gtk4::Widget) -> (u32, u32) {
    if let Some(vte) = widget.downcast_ref::<vte4::Terminal>() {
        (
            (vte.row_count().max(1)) as u32,
            (vte.column_count().max(1)) as u32,
        )
    } else {
        FALLBACK_GRID
    }
}

/// Whether a connect failure is a neutral outcome — the user cancelled a
/// credential/host-key prompt, or the transport is already gone. Those show
/// the Disconnected view, never the error screen (doc 11 §Flow, P3-3 3).
fn is_neutral_failure(err: &TransportError) -> bool {
    matches!(
        err,
        TransportError::Cancelled | TransportError::Disconnected
    )
}

/// Map the stored auth method onto the transport's auth chain input.
///
/// The password payload is never stored in the session config (doc 06 /
/// doc 11): the chain looks the real password up through the
/// [`CredentialManager`] at connect time, so `Password` carries an empty
/// placeholder. Key passphrases likewise come from the credential lookup —
/// `key_path` is the only key material on the row.
fn transport_auth_method(session: &SessionConfig) -> TransportAuthMethod {
    match session
        .auth_method
        .clone()
        .unwrap_or(ModelAuthMethod::Password)
    {
        ModelAuthMethod::Password => TransportAuthMethod::Password(String::new()),
        ModelAuthMethod::Key => TransportAuthMethod::Key {
            path: session.key_path.clone().unwrap_or_default(),
            passphrase: None,
        },
        ModelAuthMethod::Agent => TransportAuthMethod::Agent,
        ModelAuthMethod::KeyboardInteractive => TransportAuthMethod::KeyboardInteractive,
        ModelAuthMethod::None => TransportAuthMethod::None,
    }
}

/// The app's credential stack for one connect: keyring first, then an
/// interactive prompt that opens the GTK challenge dialog (doc 11
/// §CredentialManager lookup order).
fn credential_manager(session_name: &str) -> CredentialManager {
    let prompt = prompt_source(session_name.to_string());
    #[cfg(feature = "keyring")]
    let sources: Vec<Box<dyn CredentialSource>> =
        vec![Box::new(KeyringSource::new()), Box::new(prompt)];
    #[cfg(not(feature = "keyring"))]
    let sources: Vec<Box<dyn CredentialSource>> = vec![Box::new(prompt)];
    CredentialManager::new(sources)
}

/// Interactive credential source: one non-echo entry, shown on the GTK main
/// thread through the same adapter the challenge responder uses. Returning
/// `None` on cancel maps to `TransportError::Cancelled` in `auth_config`.
fn prompt_source(session_name: String) -> PromptSource {
    PromptSource::new(Arc::new(move |_session_id, secret_type| {
        let prompt_text = match secret_type {
            "key_passphrase" => "Passphrase: ",
            _ => "Password: ",
        };
        let prompts = [ChallengePrompt {
            prompt: prompt_text.to_string(),
            echo: false,
        }];
        let name = session_name.clone();
        auth_dialog::run_on_gtk_main_thread(move || match auth_dialog::top_level_parent() {
            Some(parent) => auth_dialog::run_challenges(&parent, &name, None, &prompts),
            None => auth_dialog::run_challenges_orphan(&name, None, &prompts),
        })
        .flatten()
        .and_then(|answers| answers.into_iter().next())
        .map(|answer| Zeroizing::new(answer.into_bytes()))
    }))
}

// ---------------------------------------------------------------------------
// Page views
// ---------------------------------------------------------------------------

/// Swap the paned's terminal area (its end child) to `child`.
fn set_terminal_area(page: &gtk4::Widget, child: &impl IsA<gtk4::Widget>) {
    if let Some(paned) = page.downcast_ref::<gtk4::Paned>() {
        paned.set_end_child(Some(child));
    }
}

fn status_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_vexpand(true);
    label.set_hexpand(true);
    label.set_valign(gtk4::Align::Center);
    label.set_halign(gtk4::Align::Center);
    label.set_justify(gtk4::Justification::Center);
    label
}

/// The connecting view: status text plus a Cancel button that invalidates
/// the in-flight attempt.
fn show_connecting_view(page: &gtk4::Widget, session: &Rc<SessionConfig>, host: &str, port: u16) {
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    box_.set_vexpand(true);
    box_.set_hexpand(true);
    box_.set_valign(gtk4::Align::Center);
    box_.set_halign(gtk4::Align::Center);
    box_.append(&status_label(&format!("Connecting to {host}:{port}…")));

    let cancel = gtk4::Button::with_label("Cancel");
    let page_for_cancel = page.clone();
    let session = session.clone();
    cancel.connect_clicked(move |_| cancel_connect(&page_for_cancel, &session));
    box_.append(&cancel);

    set_terminal_area(page, &box_);
}

/// Cancel an in-flight connect: invalidate the generation, drop any
/// terminal, show the neutral Disconnected view (P3-3 3).
fn cancel_connect(page: &gtk4::Widget, session: &Rc<SessionConfig>) {
    let _ = bump_gen(page);
    if let Some(tab) = take_terminal_tab(page) {
        glib::MainContext::default().spawn_local(async move {
            let mut tab = tab;
            tab.close().await;
        });
    }
    show_disconnected_view(page);
    let host = session.host.clone().unwrap_or_default();
    let port = session.port.unwrap_or(22);
    publish_status(page, TransportState::Disconnected, &host, port);
}

/// Neutral post-cancel / post-disconnect view: no error, no Retry (a fresh
/// connect comes from opening or retrying the session).
fn show_disconnected_view(page: &gtk4::Widget) {
    set_terminal_area(page, &status_label("Disconnected"));
}

/// Error view: message + Retry (re-runs [`start_connect`]) + Close Tab
/// (P3-3 deliverable 3).
fn show_error_view(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    message: &str,
) {
    // An error can arrive after the terminal was built (pump connect
    // failure): tear it down first so the overlay replaces a live tab.
    if let Some(tab) = take_terminal_tab(page) {
        glib::MainContext::default().spawn_local(async move {
            let mut tab = tab;
            tab.close().await;
        });
    }

    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    box_.set_vexpand(true);
    box_.set_hexpand(true);
    box_.set_valign(gtk4::Align::Center);
    box_.set_halign(gtk4::Align::Center);
    box_.set_margin_top(24);
    box_.set_margin_bottom(24);
    box_.set_margin_start(24);
    box_.set_margin_end(24);

    let heading = gtk4::Label::new(Some("Connection failed"));
    heading.add_css_class("heading");
    box_.append(&heading);

    let detail = gtk4::Label::new(Some(message));
    detail.set_wrap(true);
    detail.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    detail.set_selectable(true);
    detail.set_justify(gtk4::Justification::Center);
    box_.append(&detail);

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    buttons.set_halign(gtk4::Align::Center);

    let retry = gtk4::Button::with_label("Retry");
    retry.add_css_class("suggested-action");
    let notebook_for_retry = notebook.clone();
    let page_for_retry = page.clone();
    let session_for_retry = session.clone();
    retry.connect_clicked(move |_| {
        start_connect(&notebook_for_retry, &page_for_retry, &session_for_retry);
    });
    buttons.append(&retry);

    let close = gtk4::Button::with_label("Close Tab");
    let notebook_for_close = notebook.clone();
    let session_id = session.id;
    close.connect_clicked(move |_| request_close_tab(&notebook_for_close, session_id));
    buttons.append(&close);

    box_.append(&buttons);
    set_terminal_area(page, &box_);
}

// ---------------------------------------------------------------------------
// Page bookkeeping
// ---------------------------------------------------------------------------

/// Window of the page's toplevel, if it has one.
fn window_of(page: &gtk4::Widget) -> Option<gtk4::ApplicationWindow> {
    page.root()?.downcast::<gtk4::ApplicationWindow>().ok()
}

/// The window-wide connection supervisor (doc 04 §Connection Supervisor).
fn supervisor_of(window: &gtk4::ApplicationWindow) -> Option<Arc<ConnectionSupervisor>> {
    unsafe {
        window
            .data::<Arc<ConnectionSupervisor>>(SUPERVISOR_KEY)
            .map(|data| data.as_ref().clone())
    }
}

/// Stash `state` on the page and route it to the status bar if the page is
/// current (routing is a no-op without a window).
fn publish_status(page: &gtk4::Widget, state: TransportState, host: &str, port: u16) {
    let cell = ensure_page_status(page, host, port);
    {
        let mut status = cell.borrow_mut();
        status.state = state.clone();
        status.host = host.to_string();
        status.port = port;
    }
    if let Some(window) = window_of(page) {
        route_status(&window, page, state, host, port);
    }
}

fn page_status_cell(page: &gtk4::Widget) -> Option<PageStatusCell> {
    unsafe {
        page.data::<PageStatusCell>(PAGE_STATUS_KEY)
            .map(|data| data.as_ref().clone())
    }
}

/// The tab-label name label stashed by [`SessionTab::open`].
fn title_label(page: &gtk4::Widget) -> Option<gtk4::Label> {
    unsafe {
        page.data::<gtk4::Label>(TITLE_LABEL_KEY)
            .map(|data| data.as_ref().clone())
    }
}

fn ensure_page_status(page: &gtk4::Widget, host: &str, port: u16) -> PageStatusCell {
    if let Some(cell) = page_status_cell(page) {
        return cell;
    }
    let cell = Rc::new(RefCell::new(PageStatus {
        state: TransportState::Disconnected,
        host: host.to_string(),
        port,
    }));
    unsafe {
        page.set_data(PAGE_STATUS_KEY, cell.clone());
    }
    cell
}

fn term_tab_slot(page: &gtk4::Widget) -> Option<Rc<RefCell<Option<TerminalTab>>>> {
    unsafe {
        page.data::<Rc<RefCell<Option<TerminalTab>>>>(TERM_TAB_KEY)
            .map(|data| data.as_ref().clone())
    }
}

fn ensure_term_tab_slot(page: &gtk4::Widget) -> Rc<RefCell<Option<TerminalTab>>> {
    if let Some(slot) = term_tab_slot(page) {
        return slot;
    }
    let slot = Rc::new(RefCell::new(None));
    unsafe {
        page.set_data(TERM_TAB_KEY, slot.clone());
    }
    slot
}

/// Take the page's live terminal tab (removing it from the slot) so the
/// caller can `close().await` it.
fn take_terminal_tab(page: &gtk4::Widget) -> Option<TerminalTab> {
    term_tab_slot(page)?.borrow_mut().take()
}

fn gen_slot(page: &gtk4::Widget) -> Rc<Cell<u64>> {
    unsafe {
        if let Some(data) = page.data::<Rc<Cell<u64>>>(CONNECT_GEN_KEY) {
            return data.as_ref().clone();
        }
        let slot = Rc::new(Cell::new(0u64));
        page.set_data(CONNECT_GEN_KEY, slot.clone());
        slot
    }
}

/// Invalidate any in-flight attempt and return the new generation.
fn bump_gen(page: &gtk4::Widget) -> u64 {
    let slot = gen_slot(page);
    let next = slot.get().wrapping_add(1);
    slot.set(next);
    next
}

fn current_gen(page: &gtk4::Widget) -> u64 {
    gen_slot(page).get()
}

/// Close a session tab: invalidate its connect, tear the terminal down
/// asynchronously (doc 04 §Lifecycle), then remove the page. Removing the
/// page drops its widget data — the sidebar's `Rc` and every stash — so the
/// SFTP client releases its `SshConnection` reference (P3-3 2).
fn request_close_tab(notebook: &gtk4::Notebook, session_id: i64) {
    let Some((_, page)) = find_tab_by_session_id(notebook, session_id) else {
        return;
    };
    let _ = bump_gen(&page);

    let notebook = notebook.clone();
    glib::MainContext::default().spawn_local(async move {
        if let Some(tab) = take_terminal_tab(&page) {
            let mut tab = tab;
            tab.close().await;
        }
        // Re-resolve: pages may have shifted while the terminal closed.
        if let Some((page_num, _)) = find_tab_by_session_id(&notebook, session_id) {
            notebook.remove_page(Some(page_num));
        }
    });
}

// ---------------------------------------------------------------------------
// Page construction
// ---------------------------------------------------------------------------

/// Build a tab's page: the terminal side, plus the SFTP sidebar and its paned
/// for SSH sessions.
fn page_content(session: &SessionConfig) -> gtk4::Widget {
    let placeholder = gtk4::Label::new(Some(&format!(
        "{}\nterminal lands in Phase 2 — doc 04",
        session.name
    )));
    placeholder.set_vexpand(true);
    placeholder.set_hexpand(true);
    placeholder.set_valign(gtk4::Align::Center);
    placeholder.set_halign(gtk4::Align::Center);
    placeholder.set_justify(gtk4::Justification::Center);

    let page: gtk4::Widget = if supports_sftp(&session.session_type) {
        let sidebar = SftpSidebar::new();
        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(SIDEBAR_WIDTH);
        paned.set_start_child(Some(sidebar.widget()));
        paned.set_end_child(Some(&placeholder));
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        // Stash the sidebar on the page so `toggle_sftp` (and the connect
        // flow, once it has a connection) can reach it later.
        unsafe {
            paned
                .upcast_ref::<gtk4::Widget>()
                .set_data(SIDEBAR_KEY, sidebar);
        }
        paned.upcast()
    } else {
        placeholder.upcast()
    };

    // Tag the content widget with the session id so we can find it again for
    // dedup and close bookkeeping. This must be the page widget — for SSH
    // tabs that is the paned, not the placeholder inside it.
    unsafe {
        page.set_data(SESSION_ID_KEY, session.id);
    }
    page
}

/// Build the tab label widget: session name + close button.
fn build_tab_label(
    notebook: &gtk4::Notebook,
    session: &SessionConfig,
) -> (gtk4::Widget, gtk4::Label) {
    let notebook_for_close = notebook.clone();
    let session_id = session.id;
    // P4-2's tab_label module owns construction + the rename gesture +
    // the TabTitle state machine; P3-3's caller keeps its (Widget, Label)
    // contract for the terminal-title wiring.
    // Doc 04 §Lifecycle: "Confirm-before-close if processes running
    // (setting `confirm_close`)". The decision and the dialog live in
    // `tab_resilience`; the close button (built by tab_label) routes
    // through it — the close bookkeeping moves into tab_resilience and
    // the confirm dialog fires before the page is removed.
    tab_label::build_tab_label(&session.name, move || {
        let parent = gtk_window_for(&notebook_for_close.clone().upcast());
        let notebook_for_close2 = notebook_for_close.clone();
        crate::ui::tab_resilience::request_close(session_id, parent.as_ref(), move || {
            if let Some((page_num, _)) = find_tab_by_session_id(&notebook_for_close2, session_id) {
                notebook_for_close2.remove_page(Some(page_num));
            }
        });
    })
}

/// The session id of the notebook's current page, if it is a session tab.
///
/// The reconnect / disconnect actions (doc 07) resolve their target tab
/// through this; a non-session page (the Home tab) yields `None` and the
/// actions no-op.
pub(crate) fn current_session_id(notebook: &gtk4::Notebook) -> Option<i64> {
    let page_num = notebook.current_page()?;
    let child = notebook.nth_page(Some(page_num))?;
    #[allow(deprecated)]
    let id: Option<i64> = unsafe { child.data::<i64>(SESSION_ID_KEY).map(|d| *d.as_ptr()) };
    id
}

/// Walk up from a widget to the [`gtk4::Window`] that owns it, so a
/// dialog opened from a tab's close button gets the right transient
/// parent.
#[allow(deprecated)]
fn gtk_window_for(widget: &gtk4::Widget) -> Option<gtk4::Window> {
    let mut current = widget.clone();
    loop {
        if let Ok(window) = current.clone().downcast::<gtk4::Window>() {
            return Some(window);
        }
        current = current.parent()?;
    }
}

/// Find the notebook page whose content widget carries `session_id`.
fn find_tab_by_session_id(
    notebook: &gtk4::Notebook,
    session_id: i64,
) -> Option<(u32, gtk4::Widget)> {
    let n_pages = notebook.n_pages();
    for page_num in 0..n_pages {
        let child = notebook.nth_page(Some(page_num))?;
        let id: Option<i64> = unsafe { child.data::<i64>(SESSION_ID_KEY).map(|d| *d.as_ptr()) };
        if id == Some(session_id) {
            return Some((page_num, child));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tab label prefers the remote terminal title and never goes blank.
    #[test]
    fn tab_title_prefers_remote_title() {
        assert_eq!(tab_title("db-prod", "root@db-prod:~"), "root@db-prod:~");
    }

    #[test]
    fn tab_title_falls_back_to_session_name() {
        assert_eq!(tab_title("db-prod", ""), "db-prod");
        assert_eq!(tab_title("db-prod", "   \n"), "db-prod");
    }

    /// Stored auth method → transport chain input (P3-3 deliverable 4:
    /// error-to-UI-state mapping lives in `is_neutral_failure`, the auth
    /// mapping here keeps passwords/passphrases out of the session row).
    #[test]
    fn auth_method_maps_per_session_config() {
        let base = SessionConfig {
            key_path: Some("/k/id_ed25519".into()),
            ..Default::default()
        };

        let mut config = base.clone();
        config.auth_method = Some(ModelAuthMethod::Password);
        let mapped = transport_auth_method(&config);
        assert!(
            matches!(mapped, TransportAuthMethod::Password(ref p) if p.is_empty()),
            "passwords must not be stored on the row"
        );

        config.auth_method = Some(ModelAuthMethod::Key);
        assert!(matches!(
            transport_auth_method(&config),
            TransportAuthMethod::Key { ref path, passphrase: None } if path == "/k/id_ed25519"
        ));

        config.auth_method = Some(ModelAuthMethod::Agent);
        assert!(matches!(
            transport_auth_method(&config),
            TransportAuthMethod::Agent
        ));

        config.auth_method = Some(ModelAuthMethod::KeyboardInteractive);
        assert!(matches!(
            transport_auth_method(&config),
            TransportAuthMethod::KeyboardInteractive
        ));

        config.auth_method = Some(ModelAuthMethod::None);
        assert!(matches!(
            transport_auth_method(&config),
            TransportAuthMethod::None
        ));

        // Missing auth method falls back to the model default (password).
        config.auth_method = None;
        assert!(matches!(
            transport_auth_method(&config),
            TransportAuthMethod::Password(_)
        ));
    }

    /// Cancellation and disconnect are neutral outcomes; everything else is
    /// an error screen (P3-3 deliverable 3).
    #[test]
    fn neutral_failures_show_disconnected_not_error() {
        assert!(is_neutral_failure(&TransportError::Cancelled));
        assert!(is_neutral_failure(&TransportError::Disconnected));
        assert!(!is_neutral_failure(&TransportError::AuthFailed(
            "nope".into()
        )));
        assert!(!is_neutral_failure(&TransportError::ConnectionFailed(
            "refused".into()
        )));
        assert!(!is_neutral_failure(&TransportError::HostKeyRejected(
            "h".into()
        )));
    }
}
