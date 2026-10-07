//! Status bar — connection state, host and connection duration
//! (doc 10 Phase 2 checklist item "Status bar: connection state, host,
//! duration"; state values from doc 04's `TransportState`).
//!
//! Split into a display-free data layer ([`StatusView`],
//! [`status_view_from`], [`format_elapsed`]) and a thin GTK layer
//! ([`StatusBar`]), the same split used by `session_tree` and
//! `session_dialog`.  All mapping and formatting logic lives in the data
//! layer and is exercised by the tests in [`tests`] without a display;
//! the GTK layer only renders a [`StatusView`].
//!
//! **Scope:** this task builds and displays the bar only.  Resetting to
//! `Disconnected` on tab switch/close, and pushing real transport state
//! from the tabs, is the caller's job (P2-7b) — this module exposes
//! nothing but [`StatusBar::set_state`].

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;

use crate::transport::types::TransportState;

/// Colour family of the state indicator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorKind {
    /// Connected and healthy.
    Green,
    /// Transient — connecting, verifying, authenticating, reconnecting.
    Yellow,
    /// Terminal failure or no connection.
    Red,
}

impl ColorKind {
    /// Hex foreground colour of the state indicator dot.
    pub fn hex(self) -> &'static str {
        match self {
            Self::Green => "#4caf50",
            Self::Yellow => "#e0b800",
            Self::Red => "#cc4744",
        }
    }
}

/// Everything the status bar displays, computed without touching GTK.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusView {
    /// Human-readable state text, e.g. "Connected to example.com:22".
    pub state: String,
    /// Colour family of the indicator.
    pub color: ColorKind,
    /// Host shown next to the state; `None` when there is no host yet.
    pub host: Option<String>,
    /// Elapsed connection time in seconds, only set while connected.
    pub elapsed_secs: Option<u64>,
}

impl StatusView {
    /// The full line of text the bar shows, e.g.
    /// "Connected to example.com:22 — 12:34".
    pub fn text(&self) -> String {
        match self.elapsed_secs {
            Some(secs) => format!("{} — {}", self.state, format_elapsed(secs)),
            None => self.state.clone(),
        }
    }
}

/// Build the display-free view for a transport state.
///
/// `host` is reported as-is and appears in the state text whenever it is
/// non-empty (doc 01 shows the host in the status bar).  `elapsed_secs`
/// only applies to [`TransportState::Connected`]; it is dropped for every
/// other state so a stale duration can never be displayed.
pub fn status_view_from(
    state: &TransportState,
    host: &str,
    port: u16,
    elapsed_secs: Option<u64>,
) -> StatusView {
    let host = (!host.is_empty()).then(|| host.to_string());
    let endpoint = host
        .as_ref()
        .map(|h| format!("{h}:{port}"))
        .unwrap_or_default();

    let (text, color) = match state {
        TransportState::Disconnected => ("Disconnected".to_string(), ColorKind::Red),
        TransportState::Connecting => ("Connecting…".to_string(), ColorKind::Yellow),
        TransportState::VerifyingHostKey => ("Verifying host key…".to_string(), ColorKind::Yellow),
        TransportState::Authenticating => ("Authenticating…".to_string(), ColorKind::Yellow),
        TransportState::Connected => {
            let text = if endpoint.is_empty() {
                "Connected".to_string()
            } else {
                format!("Connected to {endpoint}")
            };
            (text, ColorKind::Green)
        }
        TransportState::Reconnecting { attempt } => (
            format!("Reconnecting (attempt {attempt})…"),
            ColorKind::Yellow,
        ),
        TransportState::Error(msg) => (msg.clone(), ColorKind::Red),
    };

    StatusView {
        state: text,
        color,
        host,
        elapsed_secs: match state {
            TransportState::Connected => elapsed_secs,
            _ => None,
        },
    }
}

/// Format an elapsed duration as `mm:ss`, or `h:mm:ss` from one hour on.
pub fn format_elapsed(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Live state of the elapsed-connection timer.
#[derive(Default)]
struct TimerState {
    /// Source id of the running glib timeout, if any.
    id: Option<glib::SourceId>,
    /// Instant the current connection started counting from.
    connected_at: Option<Instant>,
}

/// GTK status bar for the bottom of the main window.
///
/// Shows a coloured state indicator, the state text (including host and
/// port while connected) and — once connected — a per-second elapsed
/// connection timer.  Callers drive it exclusively through
/// [`set_state`](StatusBar::set_state).
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    label: gtk4::Label,
    timer: Rc<RefCell<TimerState>>,
}

impl StatusBar {
    /// Build a status bar showing the initial `Disconnected` state with no
    /// host; the first [`set_state`](Self::set_state) call replaces it.
    pub fn new() -> Self {
        let dot = gtk4::Label::new(None);
        dot.set_use_markup(true);
        dot.set_valign(gtk4::Align::Center);

        let label = gtk4::Label::new(Some("Disconnected"));
        label.set_hexpand(true);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        container.append(&dot);
        container.append(&label);
        container.set_margin_top(2);
        container.set_margin_bottom(2);
        container.set_margin_start(8);
        container.set_margin_end(8);

        let this = Self {
            container,
            dot,
            label,
            timer: Rc::new(RefCell::new(TimerState::default())),
        };
        this.render(&status_view_from(
            &TransportState::Disconnected,
            "",
            0,
            None,
        ));
        this
    }

    /// Return the status bar's root widget for insertion into the layout.
    pub fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    /// Show `state` for the connection to `host:port`.
    ///
    /// Entering [`TransportState::Connected`] (re)starts the per-second
    /// elapsed timer; every other state stops it, so no glib source
    /// survives a state change.  The bar is a passive display: deciding
    /// *which* state to show is the caller's business.
    pub fn set_state(&self, state: &TransportState, host: &str, port: u16) {
        match state {
            TransportState::Connected => {
                self.stop_timer();
                self.start_timer(host, port);
            }
            _ => {
                self.stop_timer();
                self.render(&status_view_from(state, host, port, None));
            }
        }
    }

    /// Restart the elapsed timer for a fresh connection to `host:port`.
    fn start_timer(&self, host: &str, port: u16) {
        let timer = Rc::clone(&self.timer);
        *timer.borrow_mut() = TimerState {
            id: None,
            connected_at: Some(Instant::now()),
        };

        self.render(&status_view_from(
            &TransportState::Connected,
            host,
            port,
            Some(0),
        ));

        let label = self.label.clone();
        let host = host.to_string();
        let timer_for_tick = Rc::clone(&timer);
        let source = glib::timeout_add_local(Duration::from_secs(1), move || {
            let elapsed = timer_for_tick
                .borrow()
                .connected_at
                .map(|start| start.elapsed().as_secs());
            let view = status_view_from(&TransportState::Connected, &host, port, elapsed);
            label.set_text(&view.text());
            glib::ControlFlow::Continue
        });

        let mut state = timer.borrow_mut();
        // A state change between scheduling and storing the id would have
        // cleared the cell, but set_state runs on this same main thread, so
        // the slot is always empty here.
        debug_assert!(state.id.is_none());
        state.id = Some(source);
    }

    /// Stop the elapsed timer, if one is running.
    fn stop_timer(&self) {
        if let Some(id) = self.timer.borrow_mut().id.take() {
            id.remove();
        }
        self.timer.borrow_mut().connected_at = None;
    }

    /// Render a view into the widgets.
    fn render(&self, view: &StatusView) {
        self.dot.set_markup(&format!(
            "<span foreground=\"{}\">●</span>",
            view.color.hex()
        ));
        self.label.set_text(&view.text());
    }
}

impl Drop for StatusBar {
    fn drop(&mut self) {
        self.stop_timer();
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
