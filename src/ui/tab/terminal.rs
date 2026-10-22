//! Terminal pages — SSH, local and serial — once their transport is ready:
//! measure the real grid, wire the [`TerminalTab`], and keep the page's
//! state channel, which feeds the registry, the status bar and the
//! disconnect alert and starts the reconnect driver on `ConnectionLost`
//! (doc 03 §Lifecycle).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender, WeakUnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use vte4::prelude::*;

use super::views::{set_terminal_area, show_error_view};
use super::{
    bump_gen, close_tab, current_gen, current_page, publish_page_state, publish_status,
    settle_redial, ssh, start_connect, state, RedialReply, SessionTab, TabType,
};
use crate::macros::InputTap;
use crate::store::model::SessionConfig;
use crate::terminal::{create_terminal, TerminalBackend};
use crate::transport::connection::SshConnection;
use crate::transport::reconnect::{ConnectFn, ConnectFuture};
use crate::transport::types::{TransportError, TransportEvent, TransportState};
use crate::ui::appearance;
use crate::ui::session_ended::{self, EndedChoice};
use crate::ui::tab_label;
use crate::ui::tab_resilience;
use crate::ui::terminal_tab::TerminalTab;
use crate::ui::tray;
use crate::ui::widgets::{centered_label, data, data_or_default, window_of};

/// Page data key: the page's [`TerminalPageState`].
const TERMINAL_PAGE_KEY: &str = "wayterm-terminal-page";

/// An unallocated VTE reports 24×80, so measuring waits (≈2 s at most)
/// for the widget to get a size.
const ALLOC_WAIT_STEPS: u32 = 125;

/// What a terminal page keeps about its terminal, on the page widget
/// beside its [`PageState`](super::PageState). Main thread only.
#[derive(Default)]
pub(super) struct TerminalPageState {
    /// The page's state channel, once its drain runs ([`page_events`]).
    events: RefCell<Option<UnboundedSender<TransportEvent>>>,
    /// The live transport wiring.
    terminal_tab: RefCell<Option<TerminalTab>>,
    /// The connected terminal and the tap on its input (doc 12).
    pub(super) terminal: RefCell<Option<Arc<dyn TerminalBackend>>>,
    pub(super) input_tap: RefCell<Option<InputTap>>,
}

/// `page`'s terminal state, created on first use.
pub(super) fn terminal_state(page: &gtk4::Widget) -> Rc<TerminalPageState> {
    data_or_default(page, TERMINAL_PAGE_KEY)
}

/// `page`'s terminal state, if it has any (not a VNC or RDP page, say).
pub(super) fn existing_terminal_state(page: &gtk4::Widget) -> Option<Rc<TerminalPageState>> {
    data(page, TERMINAL_PAGE_KEY)
}

/// Embed the terminal, measure its real grid, wire the transport, and hand
/// an SSH connection to the page's SSH half (local and serial pass none).
pub(super) async fn finish_connect(
    notebook: &gtk4::Notebook,
    session: &Rc<SessionConfig>,
    page: &gtk4::Widget,
    conn: Option<Arc<SshConnection>>,
) {
    let gen = current_gen(page);
    let handle = create_terminal();
    let backend = handle.backend.clone();
    appearance::apply_to(&*backend, session.color_scheme.as_deref());
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
    terminal_state(page)
        .terminal_tab
        .replace(Some(terminal_tab));
    SessionTab::set_terminal_for_page(page, backend, input_tap);
    let tab_id = state(page).tab_id.get();
    let cancel = tab_resilience::register_connected_tab(tab_id, redial_fn(notebook, page, tab_id));
    let events = page_events(notebook, page, session, host, port);
    spawn_state_forward(page.clone(), gen, cancel, state_rx, events);

    // Only the SSH dial passes a connection: its forward engine and
    // sidebar.
    if let Some(conn) = conn {
        ssh::on_connect(notebook, page, session, conn, &widget);
    }
    follow_terminal_title(page, &widget, &session.name);
    focus_after_connect(notebook, page);
}

/// A user-started connect takes focus (from the session tree, say); a
/// reconnect only takes it back if nothing holds it — the old terminal was
/// torn down with the connection — so it never pulls focus out of the SFTP
/// sidebar or the session tree while the user is typing there.
pub(super) fn focus_after_connect(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
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

/// After a page closes, the current page's terminal takes focus if the
/// closed page took it along: GTK leaves it on the notebook itself, where
/// typing goes nowhere. Focus elsewhere (the tree, a sidebar entry) stays.
pub(super) fn focus_after_close(notebook: &gtk4::Notebook) {
    let unclaimed = match notebook.root().and_then(|root| root.focus()) {
        None => true,
        Some(focus) => focus == *notebook.upcast_ref::<gtk4::Widget>(),
    };
    if let (true, Some(page)) = (unclaimed, current_page(notebook)) {
        focus_terminal(notebook, &page);
    }
}

/// Give the page's terminal keyboard focus, so typing reaches the shell
/// instead of whatever opened the tab (the session tree, whose type-ahead
/// search would take the keys). Only on the current page: a background
/// tab's connect or redial must not take focus from the tab in use.
pub(super) fn focus_terminal(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    if current_page(notebook).as_ref() != Some(page) {
        return;
    }
    let terminal = existing_terminal_state(page).and_then(|t| t.terminal.borrow().clone());
    if let Some(backend) = terminal {
        backend.widget().grab_focus();
    }
}

/// What the status bar shows for a tab: `host:port` for SSH, the
/// transport's description (no port, `0`) for local and serial.
fn status_endpoint(session: &SessionConfig, description: &str) -> (String, u16) {
    match TabType::of(&session.session_type) {
        TabType::Local => (description.to_string(), 0),
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
    let state = terminal_state(page);
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
/// driver per policy (doc 03 §Lifecycle). `SessionEnded` neither alerts nor
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

/// The registered tab's connect closure (doc 03: every attempt is a fresh
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
    ssh::stop_forward_engine(page);
    close_terminal(page);
    ssh::release_sidebar(notebook, page);
}

/// Disconnect landed while a redial was in flight: drop the attempt.
fn abandon_attempt(page: &gtk4::Widget) {
    bump_gen(page);
    close_terminal(page);
    set_terminal_area(page, &centered_label("Disconnected"));
    publish_page_state(page, TransportState::Disconnected);
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

/// The terminal's title, or the session name when it is blank.
fn tab_title(session_name: &str, terminal_title: &str) -> String {
    match terminal_title.trim() {
        "" => session_name.to_string(),
        title => title.to_string(),
    }
}

/// The shell exited on its own: keep the terminal and its scrollback, add
/// the notice, and let `r` reconnect (as Retry does) or `q` close the tab.
fn show_session_ended(notebook: &gtk4::Notebook, page: &gtk4::Widget) {
    close_terminal(page);
    let Some(backend) = existing_terminal_state(page).and_then(|t| t.terminal.borrow().clone())
    else {
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

/// Tear down the page's terminal, if any, in the background.
pub(super) fn close_terminal(page: &gtk4::Widget) {
    let tab = existing_terminal_state(page).and_then(|t| t.terminal_tab.take());
    if let Some(mut tab) = tab {
        glib::MainContext::default().spawn_local(async move { tab.close().await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::SessionType;

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
}
