//! The status bar: connection state, host, and time connected (doc 10
//! Phase 2). [`status_view_from`] decides what to show; [`StatusBar`]
//! only renders it. Which tab drives it is the window's business.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;

use crate::transport::types::TransportState;

/// The indicator's colour family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorKind {
    /// Connected.
    Green,
    /// On the way: connecting, verifying, authenticating, reconnecting.
    Yellow,
    /// Disconnected or failed.
    Red,
}

impl ColorKind {
    pub fn hex(self) -> &'static str {
        match self {
            Self::Green => "#4caf50",
            Self::Yellow => "#e0b800",
            Self::Red => "#cc4744",
        }
    }
}

/// What the bar shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusView {
    /// E.g. "Connected to example.com:22".
    pub state: String,
    pub color: ColorKind,
    pub host: Option<String>,
    /// Only while connected.
    pub elapsed_secs: Option<u64>,
}

impl StatusView {
    /// E.g. "Connected to example.com:22 — 12:34".
    pub fn text(&self) -> String {
        match self.elapsed_secs {
            Some(secs) => format!("{} — {}", self.state, format_elapsed(secs)),
            None => self.state.clone(),
        }
    }
}

/// The view for `state`. The elapsed time is kept only while connected, so
/// a stale duration is never shown.
pub fn status_view_from(
    state: &TransportState,
    host: &str,
    port: u16,
    elapsed_secs: Option<u64>,
) -> StatusView {
    let host = (!host.is_empty()).then(|| host.to_string());
    let endpoint = host
        .as_ref()
        .map(|h| {
            if port == 0 {
                h.clone()
            } else {
                format!("{h}:{port}")
            }
        })
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

/// `m:ss`, or `h:mm:ss` from one hour on.
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

#[derive(Default)]
struct TimerState {
    id: Option<glib::SourceId>,
    connected_at: Option<Instant>,
}

/// A coloured dot, the state text, and a per-second timer while connected.
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    label: gtk4::Label,
    timer: Rc<RefCell<TimerState>>,
}

impl StatusBar {
    /// Starts out Disconnected.
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

    pub fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    /// Show `state`; Connected (re)starts the timer, anything else stops it.
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

        timer.borrow_mut().id = Some(source);
    }

    fn stop_timer(&self) {
        if let Some(id) = self.timer.borrow_mut().id.take() {
            id.remove();
        }
        self.timer.borrow_mut().connected_at = None;
    }

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
