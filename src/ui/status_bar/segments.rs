//! What each status bar segment shows, from the tab's status and its live
//! figures (doc 03 §Status bar). Display-free: [`super::StatusBar`] only
//! renders it.

use std::time::{Duration, Instant};

use gtk4::glib;

use crate::store::model::SessionType;
use crate::transport::bandwidth::Rate;
use crate::transport::disks::{self, Disks, Filesystem, Group, Usage};
use crate::transport::host_stats::INTERVAL;
use crate::transport::load::Memory;
use crate::transport::traffic::Savings;
use crate::transport::types::TransportState;
use crate::ui::format::{format_size, local_clock};

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
        Self {
            at: Instant::now(),
            clock: local_clock(Duration::ZERO, "%X"),
        }
    }
}

/// The machine the host figures are about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Host {
    /// An SSH tab's server (doc 04 §Server figures).
    Server,
    /// The machine a local shell runs on (doc 06 §Local shell).
    ThisComputer,
}

/// A tab's host figures, as its monitor last read them (doc 03 §Status
/// bar). Each waits for its first sample.
#[derive(Clone, Debug)]
pub struct HostFigures {
    pub of: Host,
    pub uptime: Option<Uptime>,
    pub load: Option<Load>,
    /// The default-route interface's traffic.
    pub bandwidth: Option<Rate>,
    pub disks: Option<DiskReading>,
}

impl HostFigures {
    pub fn new(of: Host) -> Self {
        Self {
            of,
            uptime: None,
            load: None,
            bandwidth: None,
            disks: None,
        }
    }
}

/// How long a host has been up, as read at `at`.
#[derive(Clone, Debug)]
pub struct Uptime {
    pub secs: u64,
    pub at: Instant,
    /// Local wall-clock time it booted, e.g. "11/11/26 10:02:11".
    pub boot_clock: String,
}

impl Uptime {
    pub fn measured(secs: u64) -> Self {
        Self {
            secs,
            at: Instant::now(),
            boot_clock: local_clock(Duration::from_secs(secs), "%x %X"),
        }
    }

    pub fn now_secs(&self) -> u64 {
        self.secs + self.at.elapsed().as_secs()
    }
}

/// A host's CPU and memory use. `busy` waits for a second sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Load {
    pub busy: Option<u8>,
    pub cores: u32,
    pub load_average: [String; 3],
    pub memory: Memory,
}

/// A host's real filesystems, and which of them are not up to date.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiskReading {
    pub filesystems: Vec<Filesystem>,
    pub stale: Vec<StaleDisks>,
}

/// A group of filesystems whose `df` has not finished by the next round.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaleDisks {
    pub group: Group,
    /// Local wall-clock time of its last finished read, e.g. "14:02:11".
    pub last_read: Option<String>,
}

impl DiskReading {
    pub fn sampled(disks: Disks) -> Self {
        let stale = disks
            .late
            .iter()
            .map(|late| StaleDisks {
                group: late.group,
                last_read: late.last_read.map(|at| local_clock(at.elapsed(), "%X")),
            })
            .collect();
        Self {
            filesystems: disks.filesystems,
            stale,
        }
    }
}

/// What the bar reads on every tick while connected.
#[derive(Clone, Debug, Default)]
pub struct LiveStatus {
    /// Doc 04 §Compression savings.
    pub savings: Option<Savings>,
    pub host: Option<HostFigures>,
}

/// The tab's status as the bar was last given it.
#[derive(Clone, Debug)]
pub struct TabStatus {
    pub state: TransportState,
    pub host: String,
    pub port: u16,
    pub session: Option<SessionInfo>,
    /// While connected, when the connection came up.
    pub since: Option<ConnectedSince>,
}

/// One of the bar's segments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Item {
    State,
    Endpoint,
    Cpu,
    Memory,
    Disk,
    Bandwidth,
    Uptime,
    Elapsed,
    Savings,
}

impl Item {
    /// The left side's segments, in order.
    pub const LEFT: [Item; 2] = [Item::State, Item::Endpoint];
    /// The right side's segments, in order.
    pub const RIGHT: [Item; 7] = [
        Item::Cpu,
        Item::Memory,
        Item::Disk,
        Item::Bandwidth,
        Item::Uptime,
        Item::Elapsed,
        Item::Savings,
    ];

    /// What it shows, `None` while it has nothing to show.
    pub fn segment(self, tab: &TabStatus, live: &LiveStatus) -> Option<Segment> {
        let host = live.host.as_ref();
        match self {
            Item::State => Some(state_segment(&tab.state, &tab.host, tab.port)),
            Item::Endpoint => endpoint_segment(&tab.host, tab.port, tab.session.as_ref()),
            Item::Cpu => {
                let (of, load) = (host?.of, host?.load.as_ref()?);
                Some(cpu_segment(of, load.busy?, load.cores, &load.load_average))
            }
            Item::Memory => memory_segment(host?.of, &host?.load.as_ref()?.memory),
            Item::Disk => {
                let disks = host?.disks.as_ref()?;
                disk_segment(host?.of, &disks.filesystems, &disks.stale)
            }
            Item::Bandwidth => Some(bandwidth_segment(host?.of, host?.bandwidth.as_ref()?)),
            Item::Uptime => {
                let uptime = host?.uptime.as_ref()?;
                Some(uptime_segment(
                    host?.of,
                    uptime.now_secs(),
                    &uptime.boot_clock,
                ))
            }
            Item::Elapsed => {
                let since = tab.since.as_ref()?;
                Some(elapsed_segment(since.at.elapsed().as_secs(), &since.clock))
            }
            Item::Savings => live.savings.map(savings_segment),
        }
    }
}

/// `host:port`, or the bare description of a tab without a port (a shell
/// path, `device@baud`).
fn endpoint(host: &str, port: u16) -> String {
    if port == 0 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

/// The colour of the dot before the state.
pub fn state_color(state: &TransportState) -> ColorKind {
    match state {
        TransportState::Connected => ColorKind::Green,
        TransportState::Disconnected | TransportState::Error(_) => ColorKind::Red,
        TransportState::Connecting
        | TransportState::VerifyingHostKey
        | TransportState::Authenticating
        | TransportState::Reconnecting { .. } => ColorKind::Yellow,
    }
}

/// The state, with the endpoint in its tooltip, or the state again when
/// there is none.
pub fn state_segment(state: &TransportState, host: &str, port: u16) -> Segment {
    let (text, phrase) = match state {
        TransportState::Disconnected => ("Disconnected".to_string(), "Not connected to"),
        TransportState::Connecting => ("Connecting…".to_string(), "Connecting to"),
        TransportState::VerifyingHostKey => (
            "Verifying host key…".to_string(),
            "Checking the host key of",
        ),
        TransportState::Authenticating => ("Authenticating…".to_string(), "Logging in to"),
        TransportState::Connected => ("Connected".to_string(), "Connected to"),
        TransportState::Reconnecting { attempt } => (
            format!("Reconnecting (attempt {attempt})…"),
            "Connection lost, reconnecting to",
        ),
        TransportState::Error(msg) => {
            return Segment {
                text: msg.clone(),
                tooltip: msg.clone(),
                ..Default::default()
            };
        }
    };
    let tooltip = match (host.is_empty(), state) {
        (true, _) => text.clone(),
        (false, TransportState::Reconnecting { attempt }) => {
            format!("{phrase} {}, attempt {attempt}", endpoint(host, port))
        }
        (false, _) => format!("{phrase} {}", endpoint(host, port)),
    };
    Segment {
        text,
        tooltip,
        ..Default::default()
    }
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
            INTERVAL.as_secs(),
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

/// The total across a host's filesystems (a ZFS pool once), coloured by
/// the fullest one, so a filling filesystem warns even when the total has
/// room. The tooltip has a line per filesystem, each coloured by its own
/// fullness, and the total, then a line for each `stale` group. `None`
/// without filesystems.
pub fn disk_segment(of: Host, filesystems: &[Filesystem], stale: &[StaleDisks]) -> Option<Segment> {
    let fullest = filesystems.iter().map(|f| f.percent(filesystems)).max()?;
    let total = disks::total_usage(filesystems);
    let describe = |usage: &Usage| {
        format!(
            "{} used, {} free of {}",
            format_size(usage.used),
            format_size(usage.available),
            format_size(usage.size),
        )
    };
    // A ZFS dataset has no size of its own: it shows its used space and
    // its pool's free space.
    let mut rows: Vec<(String, u8, String)> = filesystems
        .iter()
        .map(|f| {
            let detail = match &f.pool {
                Some(pool) => format!(
                    "{} used, {} free in pool {pool}",
                    format_size(f.usage.used),
                    format_size(disks::pool_usage(filesystems, pool).available),
                ),
                None => describe(&f.usage),
            };
            (shorten(&f.mount), f.percent(filesystems), detail)
        })
        .collect();
    if rows.len() > 1 {
        rows.push(("Total".to_string(), total.percent(), describe(&total)));
    }
    let width = rows
        .iter()
        .map(|(name, _, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    let heading = format!("{} filesystems", host_name(of));
    let footer = format!("Sampled every {} s", disks::INTERVAL.as_secs());
    let mut tooltip = vec![heading.clone()];
    let mut markup = vec![glib::markup_escape_text(&heading).to_string()];
    for (name, percent, detail) in &rows {
        let text = format!("{name:<width$}  {:>4}  {detail}", format!("{percent}%"));
        let escaped = glib::markup_escape_text(&text);
        markup.push(match fullness_color(*percent) {
            Some(color) => format!("<tt><span foreground=\"{color}\">{escaped}</span></tt>"),
            None => format!("<tt>{escaped}</tt>"),
        });
        tooltip.push(text);
    }
    for stale in stale {
        let group = match stale.group {
            Group::Local => "Local",
            Group::Network => "Network",
        };
        let line = match &stale.last_read {
            Some(clock) => format!("{group} filesystems not updated since {clock}"),
            None => format!("{group} filesystems not read yet"),
        } + ": df has not finished (a hung mount?)";
        markup.push(glib::markup_escape_text(&line).to_string());
        tooltip.push(line);
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

#[cfg(test)]
mod tests;
