//! The status bar: segments for the connection state, the endpoint, the
//! time connected and what compression saves, each with a tooltip that
//! adds detail (doc 03 §Status bar). The `*_segment` functions decide what
//! to show; [`StatusBar`] only renders it. Which tab drives it is the
//! window's business.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;

use crate::store::model::SessionType;
use crate::transport::traffic::Savings;
use crate::transport::types::TransportState;
use crate::ui::panels::sftp_sidebar::format_size;

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

/// One part of the bar and what its tooltip adds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    pub text: String,
    pub tooltip: String,
}

/// The session a tab shows, for the endpoint's tooltip.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub kind: SessionType,
    pub user: Option<String>,
    /// The jump host's session name.
    pub jump: Option<String>,
}

/// When the current connection came up.
#[derive(Clone, Debug)]
pub struct ConnectedSince {
    pub at: Instant,
    /// Local wall-clock time, e.g. "14:02:11".
    pub clock: String,
}

impl ConnectedSince {
    pub fn now() -> Self {
        let clock = glib::DateTime::now_local()
            .and_then(|now| now.format("%X"))
            .map(String::from)
            .unwrap_or_default();
        Self {
            at: Instant::now(),
            clock,
        }
    }
}

/// Reads the current tab's compression saving (doc 04 §Compression
/// savings).
pub type SavingsSource = Rc<dyn Fn() -> Option<Savings>>;

/// `host:port`, or the bare description of a tab without a port (a shell
/// path, `device@baud`).
fn endpoint(host: &str, port: u16) -> String {
    if port == 0 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

/// The dot's colour and the state segment. Its tooltip names the
/// endpoint, or repeats the state when there is none.
pub fn state_segment(state: &TransportState, host: &str, port: u16) -> (ColorKind, Segment) {
    let (color, text, phrase) = match state {
        TransportState::Disconnected => (
            ColorKind::Red,
            "Disconnected".to_string(),
            "Not connected to",
        ),
        TransportState::Connecting => (
            ColorKind::Yellow,
            "Connecting…".to_string(),
            "Connecting to",
        ),
        TransportState::VerifyingHostKey => (
            ColorKind::Yellow,
            "Verifying host key…".to_string(),
            "Checking the host key of",
        ),
        TransportState::Authenticating => (
            ColorKind::Yellow,
            "Authenticating…".to_string(),
            "Logging in to",
        ),
        TransportState::Connected => (ColorKind::Green, "Connected".to_string(), "Connected to"),
        TransportState::Reconnecting { attempt } => (
            ColorKind::Yellow,
            format!("Reconnecting (attempt {attempt})…"),
            "Connection lost, reconnecting to",
        ),
        TransportState::Error(msg) => {
            return (
                ColorKind::Red,
                Segment {
                    text: msg.clone(),
                    tooltip: msg.clone(),
                },
            );
        }
    };
    let tooltip = match (host.is_empty(), state) {
        (true, _) => text.clone(),
        (false, TransportState::Reconnecting { attempt }) => {
            format!("{phrase} {}, attempt {attempt}", endpoint(host, port))
        }
        (false, _) => format!("{phrase} {}", endpoint(host, port)),
    };
    (color, Segment { text, tooltip })
}

/// The endpoint and, in its tooltip, the session behind it. `None` until
/// the tab has published an endpoint.
pub fn endpoint_segment(host: &str, port: u16, session: Option<&SessionInfo>) -> Option<Segment> {
    if host.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(session) = session {
        if !session.name.is_empty() {
            lines.push(format!("Session: {}", session.name));
        }
        lines.push(format!("Type: {}", kind_name(&session.kind)));
    }
    match session.map(|s| &s.kind) {
        Some(SessionType::LocalShell) => lines.push(format!("Shell: {host}")),
        Some(SessionType::Serial) => lines.push(format!("Line: {host}")),
        _ => {
            lines.push(format!("Host: {host}"));
            if port != 0 {
                lines.push(format!("Port: {port}"));
            }
            if let Some(session) = session {
                if let Some(user) = &session.user {
                    lines.push(format!("User: {user}"));
                }
                if let Some(jump) = &session.jump {
                    lines.push(format!("Jump host: {jump}"));
                }
            }
        }
    }
    Some(Segment {
        text: endpoint(host, port),
        tooltip: lines.join("\n"),
    })
}

fn kind_name(kind: &SessionType) -> &'static str {
    match kind {
        SessionType::SSH => "SSH",
        SessionType::RDP => "RDP",
        SessionType::VNC => "VNC",
        SessionType::Serial => "Serial",
        SessionType::LocalShell => "Local shell",
    }
}

/// The time connected, with the clock time it started in the tooltip.
pub fn elapsed_segment(secs: u64, since_clock: &str) -> Segment {
    Segment {
        text: format_elapsed(secs),
        tooltip: if since_clock.is_empty() {
            "Time connected".to_string()
        } else {
            format!("Connected since {since_clock}")
        },
    }
}

pub fn savings_segment(savings: Savings) -> Segment {
    let Savings {
        percent,
        wire,
        uncompressed,
    } = savings;
    Segment {
        text: format!("Compression saves {percent}%"),
        tooltip: format!(
            "{} received, about {} uncompressed\n\
             Counts the terminal, SFTP and forwards on this connection",
            format_size(wire),
            format_size(uncompressed),
        ),
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

/// A segment's widgets: the separator before it and its label, with an
/// optional icon, shown and hidden together.
#[derive(Clone)]
struct Part {
    separator: gtk4::Separator,
    area: gtk4::Box,
    label: gtk4::Label,
    /// Tabular digits, so a ticking figure keeps its width.
    numeric: bool,
}

impl Part {
    fn new(icon: Option<&str>, ellipsize: EllipsizeMode, numeric: bool) -> Self {
        let label = gtk4::Label::new(None);
        label.set_ellipsize(ellipsize);
        label.set_xalign(0.0);
        let area = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        if let Some(icon) = icon {
            area.append(
                &gtk4::Image::builder()
                    .icon_name(icon)
                    .accessible_role(gtk4::AccessibleRole::Presentation)
                    .build(),
            );
        }
        area.append(&label);
        Self {
            separator: gtk4::Separator::new(gtk4::Orientation::Vertical),
            area,
            label,
            numeric,
        }
    }

    fn append_to(&self, container: &gtk4::Box, separated: bool) {
        if separated {
            container.append(&self.separator);
        }
        container.append(&self.area);
    }

    fn show(&self, segment: Option<&Segment>) {
        if let Some(segment) = segment {
            if self.numeric {
                self.label.set_markup(&format!(
                    "<span font_features=\"tnum\">{}</span>",
                    glib::markup_escape_text(&segment.text)
                ));
            } else {
                self.label.set_text(&segment.text);
            }
            self.area.set_tooltip_text(Some(&segment.tooltip));
        }
        self.separator.set_visible(segment.is_some());
        self.area.set_visible(segment.is_some());
    }
}

/// The state (a coloured dot and its text) and the endpoint on the left;
/// the time connected and the compression saving on the right, ticking
/// every second while connected.
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    state: Part,
    endpoint: Part,
    elapsed: Part,
    savings: Part,
    timer: RefCell<Option<glib::SourceId>>,
    savings_source: RefCell<Option<SavingsSource>>,
}

impl StatusBar {
    /// Starts out Disconnected.
    pub fn new() -> Self {
        let dot = gtk4::Label::builder()
            .use_markup(true)
            .valign(gtk4::Align::Center)
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .build();
        let state = Part::new(None, EllipsizeMode::End, false);
        state.area.prepend(&dot);
        let endpoint = Part::new(None, EllipsizeMode::Middle, false);
        let elapsed = Part::new(
            Some("document-open-recent-symbolic"),
            EllipsizeMode::None,
            true,
        );
        let savings = Part::new(None, EllipsizeMode::None, false);

        let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        container.add_css_class("wayterm-status-bar");
        state.append_to(&container, false);
        endpoint.append_to(&container, true);
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        container.append(&spacer);
        elapsed.append_to(&container, false);
        savings.append_to(&container, true);
        container.set_margin_top(2);
        container.set_margin_bottom(2);
        container.set_margin_start(8);
        container.set_margin_end(8);

        let this = Self {
            container,
            dot,
            state,
            endpoint,
            elapsed,
            savings,
            timer: RefCell::default(),
            savings_source: RefCell::default(),
        };
        this.set_state(&TransportState::Disconnected, "", 0, None, None);
        this
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    /// Where the next [`StatusBar::set_state`] reads the saving from.
    pub fn set_savings_source(&self, source: Option<SavingsSource>) {
        self.savings_source.replace(source);
    }

    /// Show `state`. Connected shows the time since `since` and ticks it;
    /// anything else stops the tick.
    pub fn set_state(
        &self,
        state: &TransportState,
        host: &str,
        port: u16,
        since: Option<&ConnectedSince>,
        session: Option<&SessionInfo>,
    ) {
        self.stop_timer();
        let (color, segment) = state_segment(state, host, port);
        self.dot
            .set_markup(&format!("<span foreground=\"{}\">●</span>", color.hex()));
        self.state.show(Some(&segment));
        self.endpoint
            .show(endpoint_segment(host, port, session).as_ref());

        let connected = *state == TransportState::Connected;
        let source = self.savings_source.borrow().clone().filter(|_| connected);
        let since = connected.then(|| since.cloned().unwrap_or_else(ConnectedSince::now));
        let (elapsed, savings) = (self.elapsed.clone(), self.savings.clone());
        let refresh = move || {
            let segment = since
                .as_ref()
                .map(|since| elapsed_segment(since.at.elapsed().as_secs(), &since.clock));
            elapsed.show(segment.as_ref());
            let saved = source.as_ref().and_then(|source| source());
            savings.show(saved.map(savings_segment).as_ref());
        };
        refresh();
        if connected {
            let tick = glib::timeout_add_local(Duration::from_secs(1), move || {
                refresh();
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

#[cfg(test)]
pub(in crate::ui) mod gtk_tests;
