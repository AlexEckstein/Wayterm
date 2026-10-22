//! Session tabs (doc 04): the page core every tab type shares, and the
//! dispatch to each type's connect flow.
//!
//! A tab's page holds the session's area: a terminal for SSH, local and
//! serial (`terminal`), the remote desktop for VNC and RDP (`vnc_tab`,
//! `rdp_tab`). An SSH page also owns an SFTP sidebar (doc 09) — per tab,
//! since each browses its own connection — which the side dock's SFTP panel
//! shows while the tab is current (`side_dock`). State a type needs beyond
//! the core lives in that type's own struct on the page (`ssh`, `terminal`).
//!
//! Connecting: [`start_connect`] bumps a per-page generation, so a
//! cancelled or superseded attempt discards its result, and hands the page
//! to its [`TabType`]'s flow: `ssh` dials through the window's supervisor,
//! `local` starts the terminal directly, and both finish in `terminal`.
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

use gtk4::prelude::*;
use tokio::sync::oneshot;

use crate::macros::InputTap;
use crate::store::model::{SessionConfig, SessionType};
use crate::store::SessionStore;
use crate::terminal::TerminalBackend;
use crate::transport::types::{TransportError, TransportState};
use crate::ui::main_window::route_status;
use crate::ui::rdp_tab;
use crate::ui::sftp_sidebar::SftpSidebar;
use crate::ui::tab_label;
use crate::ui::tab_resilience;
use crate::ui::tunnels_dialog::ForwardStatusSource;
use crate::ui::vnc_tab;
use crate::ui::widgets::{centered_label, data, data_or_default, set_data, window_of};

mod credentials;
mod duplicate;
mod local;
mod ssh;
mod terminal;
mod views;

pub(crate) use credentials::credential_manager;
pub(crate) use duplicate::TMUX_NOTICE_SETTING;
use terminal::close_terminal;
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) use views::{
    connecting_view, notice_view, set_terminal_area, show_connecting_view, show_disconnected,
    show_error_view, show_failure,
};

/// Page data key: the tab's [`PageState`].
const PAGE_KEY: &str = "wayterm-session-page";
// Notebook data key.
const TEARDOWN_HOOK_KEY: &str = "wayterm-teardown-hook";
/// The next page's tab id ([`PageState::tab_id`]).
static NEXT_TAB_ID: AtomicI64 = AtomicI64::new(1);

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

/// Which connect flow a page runs ([`start_connect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabType {
    Ssh,
    /// Local shell and serial: a terminal with nothing to dial (`local`).
    Local,
    Vnc,
    Rdp,
}

impl TabType {
    fn of(session_type: &SessionType) -> Self {
        match session_type {
            SessionType::SSH => Self::Ssh,
            SessionType::LocalShell | SessionType::Serial => Self::Local,
            SessionType::VNC => Self::Vnc,
            SessionType::RDP => Self::Rdp,
        }
    }
}

/// A driver-driven redial waiting for its attempt's verdict.
type RedialReply = oneshot::Sender<Result<(), TransportError>>;

/// What every session tab keeps about itself, held once on its page widget
/// ([`state`]). Main thread only.
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
        ssh::sidebar_of(page)
    }

    /// Give `page` its sidebar; the side dock hosts it while the page is
    /// current.
    pub(in crate::ui) fn set_sidebar_for_page(page: &gtk4::Widget, sidebar: SftpSidebar) {
        ssh::set_sidebar(page, sidebar);
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
        let bridge = ssh::forward_source(&current_page(notebook)?)?;
        Some(bridge)
    }

    /// The current tab's terminal, once connected (the terminal actions'
    /// target).
    pub fn active_terminal(notebook: &gtk4::Notebook) -> Option<Arc<dyn TerminalBackend>> {
        terminal::existing_terminal_state(&current_page(notebook)?)?
            .terminal
            .borrow()
            .clone()
    }

    /// The current tab's input tap, once connected (the macro recorder's
    /// target, doc 12).
    pub fn active_input_tap(notebook: &gtk4::Notebook) -> Option<InputTap> {
        terminal::existing_terminal_state(&current_page(notebook)?)?
            .input_tap
            .borrow()
            .clone()
    }

    /// `page`'s connected terminal and the session's `color_scheme`
    /// override, if it has a terminal (the appearance push's targets).
    pub(in crate::ui) fn terminal_and_scheme(
        page: &gtk4::Widget,
    ) -> Option<(Arc<dyn TerminalBackend>, Option<String>)> {
        let terminal = terminal::existing_terminal_state(page)?
            .terminal
            .borrow()
            .clone()?;
        let scheme =
            existing_state(page).and_then(|state| state.session.get()?.color_scheme.clone());
        Some((terminal, scheme))
    }

    /// Give `page` its connected terminal and the tap on its input.
    pub(in crate::ui) fn set_terminal_for_page(
        page: &gtk4::Widget,
        backend: Arc<dyn TerminalBackend>,
        tap: InputTap,
    ) {
        let state = terminal::terminal_state(page);
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
    ssh::stop_forward_engine(page);
    close_terminal(page);
    // Every caller but the reconnect driver is the user asking for this
    // tab; `redial` clears it again.
    state(page).focus_on_connect.set(true);

    match TabType::of(&session.session_type) {
        TabType::Ssh => ssh::start(notebook, page, session, gen),
        TabType::Local => local::start(notebook, page, session, gen),
        // VNC has no byte stream: its own dial (with the password prompt)
        // and engine, no supervisor or terminal (doc 08 §VNC).
        TabType::Vnc => vnc_tab::start(notebook, page, session, gen),
        // RDP likewise owns its subprocess dial (doc 08 §RDP).
        TabType::Rdp => rdp_tab::start(notebook, page, session, gen),
    }
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

/// The page's store, kept by [`SessionTab::open`] for jump resolution and
/// the credential settings.
// used by ui::vnc_tab and ui::rdp_tab
pub(crate) fn page_store(page: &gtk4::Widget) -> Option<Rc<SessionStore>> {
    state(page).store.get().cloned()
}

// ---------------------------------------------------------------------------
// Page bookkeeping
// ---------------------------------------------------------------------------

/// Closing a page by any path tears its registry entry down, so no
/// reconnect loop outlives its tab (doc 04 §Lifecycle), and the tab left
/// current gets the keyboard focus the closed one held. Once per notebook.
fn install_teardown_on_remove(notebook: &gtk4::Notebook) {
    if data::<bool>(notebook, TEARDOWN_HOOK_KEY).is_some() {
        return;
    }
    set_data(notebook, TEARDOWN_HOOK_KEY, true);
    notebook.connect_page_removed(|notebook, page, _| {
        if let Some(tab_id) = tab_id_of(page) {
            bump_gen(page);
            // Closing by any path stops the engine deterministically
            // (`close_tab` removes the page, so it lands here too).
            ssh::stop_forward_engine(page);
            tab_resilience::teardown_tab(tab_id);
        }
        terminal::focus_after_close(notebook);
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
    if session.session_type.supports_sftp() {
        ssh::install_sidebar(&page);
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
    let store = page_store(page);
    crate::ui::tab_resilience::request_close(tab_id, parent.as_ref(), store, move || {
        if let Some(page_num) = weak.upgrade().and_then(|page| notebook.page_num(&page)) {
            notebook.remove_page(Some(page_num));
        }
    });
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) mod gtk_tests;
