//! The status bar: segments for the connection state, the endpoint, the
//! host's CPU, memory, disks, bandwidth and uptime, the time connected and
//! what compression saves, each with a tooltip that adds detail (doc 03 §Status bar). The
//! `*_segment` functions decide what to show; [`StatusBar`] only renders
//! it. Which tab drives it is the window's business.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;

use crate::store::model::SessionType;
use crate::transport::bandwidth::{INTERVAL, Rate};
use crate::transport::disks::{self, Filesystem, Usage};
use crate::transport::load::{self, Memory};
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
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Segment {
    pub text: String,
    pub tooltip: String,
    /// The text's colour, when it warns.
    pub color: Option<&'static str>,
    /// Pango markup shown instead of `tooltip`, which stays its plain text.
    pub tooltip_markup: Option<String>,
}

/// Amber from 80 % full, red from 90 %.
const FILLING: &str = "#d08700";
const FULL: &str = "#e0443e";

/// The text colour for how full a filesystem is; `None` while it has room.
pub fn fullness_color(percent: u8) -> Option<&'static str> {
    match percent {
        90.. => Some(FULL),
        80.. => Some(FILLING),
        _ => None,
    }
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

/// The machine a host figure (load, disks, bandwidth, uptime) is about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Host {
    /// An SSH tab's server (doc 04 §Server uptime, §Server bandwidth).
    Server,
    /// The machine a local shell runs on (doc 06 §Local shell).
    ThisComputer,
}

/// How long a host has been up, as read at `at`.
#[derive(Clone, Debug)]
pub struct HostUptime {
    pub of: Host,
    pub secs: u64,
    pub at: Instant,
    /// Local wall-clock time it booted, e.g. "11/11/26 10:02:11".
    pub boot_clock: String,
}

impl HostUptime {
    pub fn measured(of: Host, secs: u64) -> Self {
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

/// A host's default-route interface traffic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostRate {
    pub of: Host,
    pub rate: Rate,
}

/// A host's CPU and memory use. `busy` waits for a second sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLoad {
    pub of: Host,
    pub busy: Option<u8>,
    pub cores: u32,
    pub load_average: [String; 3],
    pub memory: Memory,
}

/// A host's real filesystems.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDisks {
    pub of: Host,
    pub filesystems: Vec<Filesystem>,
}

/// What the bar reads on every tick while connected.
#[derive(Clone, Debug, Default)]
pub struct LiveStatus {
    /// Doc 04 §Compression savings.
    pub savings: Option<Savings>,
    pub uptime: Option<HostUptime>,
    pub bandwidth: Option<HostRate>,
    pub load: Option<HostLoad>,
    pub disks: Option<HostDisks>,
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
                    ..Default::default()
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
    (
        color,
        Segment {
            text,
            tooltip,
            ..Default::default()
        },
    )
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
    }
}

fn host_name(of: Host) -> &'static str {
    match of {
        Host::Server => "Server",
        Host::ThisComputer => "This computer",
    }
}

/// How busy a host's CPUs were over the last interval, with the core
/// count and load averages in the tooltip.
pub fn cpu_segment(of: Host, busy: u8, cores: u32, load_average: &[String; 3]) -> Segment {
    let [one, five, fifteen] = load_average;
    Segment {
        text: format!("CPU {busy}%"),
        tooltip: format!(
            "{} CPU {busy}% busy, {cores} {}\n\
             Load average {one}, {five}, {fifteen} (1, 5, 15 min)\n\
             Sampled every {} s",
            host_name(of),
            if cores == 1 { "core" } else { "cores" },
            load::INTERVAL.as_secs(),
        ),
        ..Default::default()
    }
}

/// How much of a host's memory is in use, with what is available and the
/// swap in the tooltip. `None` without a total.
pub fn memory_segment(of: Host, memory: &Memory) -> Option<Segment> {
    if memory.total == 0 {
        return None;
    }
    let used = memory.used();
    let percent = (used * 100 + memory.total / 2) / memory.total;
    let swap = if memory.swap_total == 0 {
        "No swap".to_string()
    } else {
        format!(
            "Swap: {} used of {}",
            format_size(memory.swap_total.saturating_sub(memory.swap_free)),
            format_size(memory.swap_total)
        )
    };
    Some(Segment {
        text: format!("RAM {percent}%"),
        tooltip: format!(
            "{} memory: {} used of {} ({percent}%)\n{} available\n{swap}",
            host_name(of),
            format_size(used),
            format_size(memory.total),
            format_size(memory.available),
        ),
        ..Default::default()
    })
}

/// A mount point column at most this wide; longer ones lose their middle.
const MOUNT_WIDTH: usize = 24;

fn shorten(mount: &str) -> String {
    let chars: Vec<char> = mount.chars().collect();
    if chars.len() <= MOUNT_WIDTH {
        return mount.to_string();
    }
    let keep = MOUNT_WIDTH - 1;
    let head: String = chars[..keep / 2].iter().collect();
    let tail: String = chars[chars.len() - (keep - keep / 2)..].iter().collect();
    format!("{head}…{tail}")
}

/// The total across a host's filesystems, coloured by the fullest one, so
/// a filling filesystem warns even when the total has room. The tooltip
/// has a line per filesystem, each coloured by its own fullness, and the
/// total. `None` without filesystems.
pub fn disk_segment(of: Host, filesystems: &[Filesystem]) -> Option<Segment> {
    let fullest = filesystems.iter().map(|f| f.usage.percent()).max()?;
    let total = filesystems
        .iter()
        .fold(Usage::default(), |sum, f| sum + f.usage);
    let mut rows: Vec<(String, Usage)> = filesystems
        .iter()
        .map(|f| (shorten(&f.mount), f.usage))
        .collect();
    if rows.len() > 1 {
        rows.push(("Total".to_string(), total));
    }
    let width = rows
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    let line = |name: &str, usage: &Usage| {
        format!(
            "{name:<width$}  {:>4}  {} used, {} free of {}",
            format!("{}%", usage.percent()),
            format_size(usage.used),
            format_size(usage.available),
            format_size(usage.size),
        )
    };
    let heading = format!("{} filesystems", host_name(of));
    let footer = format!("Sampled every {} s", disks::INTERVAL.as_secs());
    let mut tooltip = vec![heading.clone()];
    let mut markup = vec![glib::markup_escape_text(&heading).to_string()];
    for (name, usage) in &rows {
        let text = line(name, usage);
        let escaped = glib::markup_escape_text(&text);
        markup.push(match fullness_color(usage.percent()) {
            Some(color) => format!("<tt><span foreground=\"{color}\">{escaped}</span></tt>"),
            None => format!("<tt>{escaped}</tt>"),
        });
        tooltip.push(text);
    }
    tooltip.push(footer.clone());
    markup.push(glib::markup_escape_text(&footer).to_string());
    Some(Segment {
        text: format!(
            "Disk {} / {}",
            format_size(total.used),
            format_size(total.size)
        ),
        tooltip: tooltip.join("\n"),
        color: fullness_color(fullest),
        tooltip_markup: Some(markup.join("\n")),
    })
}

/// A host's interface traffic: received and sent per second.
pub fn bandwidth_segment(of: Host, rate: &Rate) -> Segment {
    let (down, up) = (format_rate(rate.rx_per_sec), format_rate(rate.tx_per_sec));
    Segment {
        text: format!("↓ {down} ↑ {up}"),
        tooltip: format!(
            "{} network, {} (the default route)\nReceiving {down}, sending {up}\n\
             All traffic on the interface, sampled every {} s",
            host_name(of),
            rate.interface,
            INTERVAL.as_secs(),
        ),
        ..Default::default()
    }
}

/// Bits per second in the largest unit (steps of 1000) that keeps the
/// figure under 1000, as network rates are given: `800 bit/s`,
/// `16.4 kbit/s`, `250 Mbit/s`.
pub fn format_rate(bytes_per_sec: u64) -> String {
    const UNITS: [&str; 5] = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s", "Tbit/s"];
    let bits = bytes_per_sec.saturating_mul(8);
    if bits < 1000 {
        return format!("{bits} {}", UNITS[0]);
    }
    let mut value = bits as f64;
    let mut unit = 0;
    // 999.5 and up would round to "1000", so it takes the next unit.
    while value >= 999.5 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let decimals = if value < 99.95 { 1 } else { 0 };
    format!("{value:.decimals$} {}", UNITS[unit])
}

/// A host's uptime, with the full duration and the boot time in the
/// tooltip.
pub fn uptime_segment(of: Host, secs: u64, boot_clock: &str) -> Segment {
    let mut tooltip = format!("{} up {}", host_name(of), describe_duration(secs));
    if !boot_clock.is_empty() {
        tooltip.push_str(&format!("\nBooted {boot_clock}"));
    }
    Segment {
        text: format!("Up {}", format_uptime(secs)),
        tooltip,
        ..Default::default()
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
            let mut attributes = String::new();
            if self.numeric {
                attributes.push_str(" font_features=\"tnum\"");
            }
            if let Some(color) = segment.color {
                attributes.push_str(&format!(" foreground=\"{color}\""));
            }
            if attributes.is_empty() {
                self.label.set_text(&segment.text);
            } else {
                self.label.set_markup(&format!(
                    "<span{attributes}>{}</span>",
                    glib::markup_escape_text(&segment.text)
                ));
            }
            match &segment.tooltip_markup {
                Some(markup) => self.area.set_tooltip_markup(Some(markup)),
                None => self.area.set_tooltip_text(Some(&segment.tooltip)),
            }
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
/// the host's CPU, memory, disks, bandwidth and uptime, the time connected
/// and the compression saving on the right, ticking every second while
/// connected.
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    state: Part,
    endpoint: Part,
    cpu: Part,
    memory: Part,
    disk: Part,
    bandwidth: Part,
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
        let cpu = Part::new(None, EllipsizeMode::None, true);
        let memory = Part::new(None, EllipsizeMode::None, true);
        let disk = Part::new(None, EllipsizeMode::None, true);
        let bandwidth = Part::new(None, EllipsizeMode::None, true);
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
        cpu.append_to(&container);
        memory.append_to(&container);
        disk.append_to(&container);
        bandwidth.append_to(&container);
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
            cpu,
            memory,
            disk,
            bandwidth,
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
        let (cpu, memory, disk) = (self.cpu.clone(), self.memory.clone(), self.disk.clone());
        let (bandwidth, uptime, elapsed, savings) = (
            self.bandwidth.clone(),
            self.uptime.clone(),
            self.elapsed.clone(),
            self.savings.clone(),
        );
        let refresh = move || {
            let live = source.as_ref().map(|source| source()).unwrap_or_default();
            let load = live.load.as_ref();
            let busy = load.and_then(|load| {
                Some(cpu_segment(
                    load.of,
                    load.busy?,
                    load.cores,
                    &load.load_average,
                ))
            });
            cpu.show(busy.as_ref());
            let used = load.and_then(|load| memory_segment(load.of, &load.memory));
            memory.show(used.as_ref());
            let full = live
                .disks
                .as_ref()
                .and_then(|disks| disk_segment(disks.of, &disks.filesystems));
            disk.show(full.as_ref());
            let rate = live
                .bandwidth
                .map(|host| bandwidth_segment(host.of, &host.rate));
            bandwidth.show(rate.as_ref());
            let up = live
                .uptime
                .map(|up| uptime_segment(up.of, up.now_secs(), &up.boot_clock));
            uptime.show(up.as_ref());
            let segment = since
                .as_ref()
                .map(|since| elapsed_segment(since.at.elapsed().as_secs(), &since.clock));
            elapsed.show(segment.as_ref());
            savings.show(live.savings.map(savings_segment).as_ref());
            separate(&[
                &cpu, &memory, &disk, &bandwidth, &uptime, &elapsed, &savings,
            ]);
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
