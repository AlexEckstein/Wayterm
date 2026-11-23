//! The status bar: segments for the connection state, the endpoint, the
//! host's uptime, the time connected and what compression saves, each with a tooltip that
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

/// Whose uptime a tab shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UptimeOf {
    /// An SSH tab's server (doc 04 §Server uptime).
    Server,
    /// The machine a local shell runs on (doc 06 §Local shell).
    ThisComputer,
}

/// How long a host has been up, as read at `at`.
#[derive(Clone, Debug)]
pub struct HostUptime {
    pub of: UptimeOf,
    pub secs: u64,
    pub at: Instant,
    /// Local wall-clock time it booted, e.g. "11/11/26 10:02:11".
    pub boot_clock: String,
}

impl HostUptime {
    pub fn measured(of: UptimeOf, secs: u64) -> Self {
        let boot_clock = glib::DateTime::now_local()
            .and_then(|now| now.add_seconds(-(secs as f64)))
            .and_then(|boot| boot.format("%x %X"))
            .map(String::from)
            .unwrap_or_default();
        Self {
            of,
            secs,
            at: Instant::now(),
            boot_clock,
        }
    }

    pub fn now_secs(&self) -> u64 {
        self.secs + self.at.elapsed().as_secs()
    }
}

/// What the bar reads on every tick while connected.
#[derive(Clone, Debug, Default)]
pub struct LiveStatus {
    /// Doc 04 §Compression savings.
    pub savings: Option<Savings>,
    pub uptime: Option<HostUptime>,
}

/// Reads the current tab's [`LiveStatus`].
pub type LiveSource = Rc<dyn Fn() -> LiveStatus>;

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

/// A host's uptime, with the full duration and the boot time in the
/// tooltip.
pub fn uptime_segment(of: UptimeOf, secs: u64, boot_clock: &str) -> Segment {
    let whose = match of {
        UptimeOf::Server => "Server",
        UptimeOf::ThisComputer => "This computer",
    };
    let mut tooltip = format!("{whose} up {}", describe_duration(secs));
    if !boot_clock.is_empty() {
        tooltip.push_str(&format!("\nBooted {boot_clock}"));
    }
    Segment {
        text: format!("Up {}", format_uptime(secs)),
        tooltip,
    }
}

/// `42m`, `5h 12m`, or `12d 4h` from a day on.
pub fn format_uptime(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// E.g. "12 days, 4 hours, 7 minutes"; zero parts are left out, and under
/// a minute reads "less than a minute".
fn describe_duration(secs: u64) -> String {
    let parts: Vec<String> = [
        (secs / 86_400, "day"),
        ((secs % 86_400) / 3600, "hour"),
        ((secs % 3600) / 60, "minute"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, unit)| format!("{n} {unit}{}", if n == 1 { "" } else { "s" }))
    .collect();
    if parts.is_empty() {
        "less than a minute".to_string()
    } else {
        parts.join(", ")
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
/// optional icon.
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

    fn append_to(&self, container: &gtk4::Box) {
        container.append(&self.separator);
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
        self.area.set_visible(segment.is_some());
    }
}

/// A separator shows only between two shown segments of one side.
fn separate(side: &[&Part]) {
    let mut before = false;
    for part in side {
        let shown = part.area.is_visible();
        part.separator.set_visible(shown && before);
        before |= shown;
    }
}

/// The state (a coloured dot and its text) and the endpoint on the left;
/// the host's uptime, the time connected and the compression saving on
/// the right, ticking every second while connected.
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    state: Part,
    endpoint: Part,
    uptime: Part,
    elapsed: Part,
    savings: Part,
    timer: RefCell<Option<glib::SourceId>>,
    live_source: RefCell<Option<LiveSource>>,
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
        let uptime = Part::new(None, EllipsizeMode::None, false);
        let elapsed = Part::new(
            Some("document-open-recent-symbolic"),
            EllipsizeMode::None,
            true,
        );
        let savings = Part::new(None, EllipsizeMode::None, false);

        let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        container.add_css_class("wayterm-status-bar");
        state.append_to(&container);
        endpoint.append_to(&container);
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        container.append(&spacer);
        uptime.append_to(&container);
        elapsed.append_to(&container);
        savings.append_to(&container);
        container.set_margin_top(2);
        container.set_margin_bottom(2);
        container.set_margin_start(8);
        container.set_margin_end(8);

        let this = Self {
            container,
            dot,
            state,
            endpoint,
            uptime,
            elapsed,
            savings,
            timer: RefCell::default(),
            live_source: RefCell::default(),
        };
        this.set_state(&TransportState::Disconnected, "", 0, None, None);
        this
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    /// Where the next [`StatusBar::set_state`] reads the live figures from.
    pub fn set_live_source(&self, source: Option<LiveSource>) {
        self.live_source.replace(source);
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
        separate(&[&self.state, &self.endpoint]);

        let connected = *state == TransportState::Connected;
        let source = self.live_source.borrow().clone().filter(|_| connected);
        let since = connected.then(|| since.cloned().unwrap_or_else(ConnectedSince::now));
        let (uptime, elapsed, savings) = (
            self.uptime.clone(),
            self.elapsed.clone(),
            self.savings.clone(),
        );
        let refresh = move || {
            let live = source.as_ref().map(|source| source()).unwrap_or_default();
            let up = live
                .uptime
                .map(|up| uptime_segment(up.of, up.now_secs(), &up.boot_clock));
            uptime.show(up.as_ref());
            let segment = since
                .as_ref()
                .map(|since| elapsed_segment(since.at.elapsed().as_secs(), &since.clock));
            elapsed.show(segment.as_ref());
            savings.show(live.savings.map(savings_segment).as_ref());
            separate(&[&uptime, &elapsed, &savings]);
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
