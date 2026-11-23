//! The status bar: connection state, host, time connected and what
//! compression saves (doc 03 §Status bar). [`status_view_from`] decides
//! what to show; [`StatusBar`] only renders it. Which tab drives it is the
//! window's business.

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
    /// What compression saves on incoming traffic, once measured.
    pub savings_percent: Option<u8>,
}

impl StatusView {
    /// E.g. "Connected to example.com:22 — 12:34 — 53% saved by
    /// compression".
    pub fn text(&self) -> String {
        let mut text = match self.elapsed_secs {
            Some(secs) => format!("{} — {}", self.state, format_elapsed(secs)),
            None => self.state.clone(),
        };
        if let Some(percent) = self.savings_percent {
            text.push_str(&format!(" — {percent}% saved by compression"));
        }
        text
    }

    pub fn with_savings(self, savings_percent: Option<u8>) -> Self {
        Self {
            savings_percent,
            ..self
        }
    }
}

/// Reads the current tab's compression saving (doc 04 §Compression
/// savings).
pub type SavingsSource = Rc<dyn Fn() -> Option<u8>>;

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
        savings_percent: None,
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

/// A coloured dot, the state text, and a per-second timer while connected.
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    label: gtk4::Label,
    /// The tick updating the time connected and the saving, while connected.
    timer: RefCell<Option<glib::SourceId>>,
    savings: RefCell<Option<SavingsSource>>,
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
            timer: RefCell::default(),
            savings: RefCell::default(),
        };
        this.set_state(&TransportState::Disconnected, "", 0);
        this
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    /// Where the next [`StatusBar::set_state`] reads the saving from.
    pub fn set_savings_source(&self, source: Option<SavingsSource>) {
        self.savings.replace(source);
    }

    /// Show `state`; Connected (re)starts the timer, anything else stops it.
    pub fn set_state(&self, state: &TransportState, host: &str, port: u16) {
        self.stop_timer();
        let connected = *state == TransportState::Connected;
        let savings = self.savings.borrow().clone().filter(|_| connected);
        let read = move || savings.as_ref().and_then(|source| source());
        let view = status_view_from(state, host, port, connected.then_some(0));
        self.render(&view.with_savings(read()));
        if connected {
            let (label, host, since) = (self.label.clone(), host.to_string(), Instant::now());
            let tick = glib::timeout_add_local(Duration::from_secs(1), move || {
                let elapsed = Some(since.elapsed().as_secs());
                let view = status_view_from(&TransportState::Connected, &host, port, elapsed);
                label.set_text(&view.with_savings(read()).text());
                glib::ControlFlow::Continue
            });
            self.timer.replace(Some(tick));
        }
    }

    fn stop_timer(&self) {
        if let Some(tick) = self.timer.take() {
            tick.remove();
        }
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
