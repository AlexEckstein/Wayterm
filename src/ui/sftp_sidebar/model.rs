//! The display-free half of the SFTP sidebar: listing rows, toolbar
//! rules, transfer status text, and `text/uri-list` parsing. Everything
//! here is plain data, unit-tested without a display.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::sftp::DirEntry;

/// The synthetic "go up" row.
pub const PARENT_ROW: &str = "..";

/// Where the sidebar starts browsing.
pub const START_PATH: &str = ".";

const DIR_ICON: &str = "📁";
const FILE_ICON: &str = "📄";
const PARENT_ICON: &str = "⬆";

/// Inline error shown while no SSH connection backs the sidebar.
pub const NOT_CONNECTED: &str = "SFTP needs a live SSH connection";
/// Doc 11 v1: no SFTP task runs over a jumped connection, so the sidebar
/// shows this instead of attaching.
pub const JUMPED_UNAVAILABLE: &str = "SFTP is not available for jumped sessions yet";
/// Doc 09: a dead SFTP task is observable as an errored reply.
pub const SESSION_ENDED: &str = "SFTP session ended — reconnect?";
/// The command could not be queued at all (task gone, channel closed).
pub const COMMAND_FAILED: &str = "SFTP command could not be queued";

/// Settings row (doc 06): open the sidebar's SFTP channel when an SSH tab
/// connects, and bring the side dock's SFTP panel forward. Off, the
/// sidebar starts hidden and attaches on its first show, so no channel
/// opens for a sidebar nobody looks at.
pub const SFTP_ON_CONNECT_SETTING: &str = "sftp_on_connect";

static OPEN_ON_CONNECT: AtomicBool = AtomicBool::new(true);

/// `sftp_on_connect`, pushed in at startup and on every change; tabs read
/// it when they are built and when they connect.
pub fn install_settings(open_on_connect: bool) {
    OPEN_ON_CONNECT.store(open_on_connect, Ordering::SeqCst);
}

pub fn open_on_connect() -> bool {
    OPEN_ON_CONNECT.load(Ordering::SeqCst)
}

/// Whether a freshly connected sidebar attaches at once or waits for its
/// first show. A sidebar the user already opened attaches regardless.
pub fn attach_on_connect(open_on_connect: bool, shown: bool) -> bool {
    open_on_connect || shown
}

/// One listing row, in store order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowView {
    pub icon: &'static str,
    pub name: String,
    /// Already human-formatted; empty for directories.
    pub size: String,
    pub modified: String,
    pub permissions: String,
    pub is_dir: bool,
    /// True only for the synthetic [`PARENT_ROW`] row.
    pub is_parent: bool,
}

/// The synthetic "go up" row.
pub fn parent_row() -> RowView {
    RowView {
        icon: PARENT_ICON,
        name: PARENT_ROW.to_string(),
        size: String::new(),
        modified: String::new(),
        permissions: String::new(),
        is_dir: true,
        is_parent: true,
    }
}

/// Map one [`DirEntry`] onto a listing row.
pub fn row_from_entry(entry: &DirEntry) -> RowView {
    RowView {
        icon: if entry.is_dir { DIR_ICON } else { FILE_ICON },
        name: entry.name.clone(),
        size: if entry.is_dir {
            String::new()
        } else {
            format_size(entry.size)
        },
        modified: entry.modified.clone(),
        permissions: entry.permissions.clone(),
        is_dir: entry.is_dir,
        is_parent: false,
    }
}

/// The listing for `path`: [`PARENT_ROW`] first (except at `/`), then the
/// entries in the task's order.
pub fn listing_rows(path: &str, entries: &[DirEntry]) -> Vec<RowView> {
    let mut rows = Vec::with_capacity(entries.len() + 1);
    if parent_path(path).is_some() {
        rows.push(parent_row());
    }
    rows.extend(entries.iter().map(row_from_entry));
    rows
}

/// `dir/name`; an absolute `name` or an empty `dir` yields `name`.
pub fn join_path(dir: &str, name: &str) -> String {
    if name.starts_with('/') || dir.is_empty() {
        return name.to_string();
    }
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The [`PARENT_ROW`] target; `None` at the root or for an empty path.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some("/".to_string()),
        Some((head, _)) => Some(head.to_string()),
        None => Some(".".to_string()),
    }
}

/// The last path component.
pub fn file_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Human-readable byte count, e.g. `1023 B`, `1.2 KB`, `4.0 MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Bar fraction in `0.0..=1.0`; an unknown (zero) total shows empty.
pub fn progress_fraction(transferred: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (transferred as f64 / total as f64).clamp(0.0, 1.0)
}

/// Progress bar text, e.g. `1.2 KB / 4.0 MB`.
pub fn progress_text(transferred: u64, total: u64) -> String {
    if total == 0 {
        format!("{} / ?", format_size(transferred))
    } else {
        format!("{} / {}", format_size(transferred), format_size(total))
    }
}

/// Status line for a transfer that is about to start.
pub fn started_status(label: &str) -> String {
    format!("{label} — starting…")
}

/// Status line for one progress sample.
pub fn running_status(label: &str, transferred: u64, total: u64) -> String {
    format!("{label} — {}", progress_text(transferred, total))
}

/// Status line for a finished transfer.
pub fn done_status(label: &str, bytes: u64) -> String {
    format!("{label} — done — {bytes} bytes")
}

/// Status line for a failed transfer.
pub fn failed_status(label: &str, err: &str) -> String {
    format!("{label} — failed: {err}")
}

/// What the toolbar rules need to know about the selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    pub name: String,
    pub is_dir: bool,
    pub is_parent: bool,
}

/// Button sensitivity: doc 09's six toolbar buttons plus Cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolbarState {
    pub upload: bool,
    pub download: bool,
    pub delete: bool,
    pub rename: bool,
    pub refresh: bool,
    pub mkdir: bool,
    pub cancel: bool,
}

impl ToolbarState {
    /// Nothing works without a live client.
    pub const DISABLED: Self = Self {
        upload: false,
        download: false,
        delete: false,
        rename: false,
        refresh: false,
        mkdir: false,
        cancel: false,
    };
}

/// Nothing works without a client; Download needs a selected file (no
/// recursive transfers); Delete and Rename need any row but `..`; Cancel
/// needs a running transfer.
pub fn toolbar_state(
    selected: Option<&Selection>,
    connected: bool,
    transferring: bool,
) -> ToolbarState {
    if !connected {
        return ToolbarState::DISABLED;
    }
    let picked = selected.filter(|selection| !selection.is_parent);
    ToolbarState {
        upload: true,
        download: picked.is_some_and(|selection| !selection.is_dir),
        delete: picked.is_some(),
        rename: picked.is_some(),
        refresh: true,
        mkdir: true,
        cancel: transferring,
    }
}

/// The [`Selection`] a listing row describes.
pub fn selection_of(row: Option<&RowView>) -> Option<Selection> {
    row.map(|row| Selection {
        name: row.name.clone(),
        is_dir: row.is_dir,
        is_parent: row.is_parent,
    })
}

/// Local paths from a `text/uri-list` drop: comments and blanks skipped,
/// only `file://` URIs with an empty or `localhost` host, percent-decoded.
pub fn parse_uri_list(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(path) = uri_to_path(line) {
            paths.push(path);
        }
    }
    paths
}

/// One `file://` URI to a local path, or `None` when it is not one.
fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let host = &rest[..slash];
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let path = percent_decode(&rest[slash..]);
    (!path.is_empty()).then_some(path)
}

/// Decode `%XX` escapes in a URI path; invalid escapes stay verbatim.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// One running transfer, in start order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferState {
    pub id: u64,
    /// Status-line prefix, e.g. `Upload notes.txt`.
    pub label: String,
}

/// Everything the sidebar shows; handlers mutate it, then render.
#[derive(Clone, Debug, PartialEq)]
pub struct SftpView {
    /// Directory the current listing belongs to ("" before the first one).
    pub path: String,
    /// Text the path bar shows — the requested path, see [`START_PATH`].
    pub path_text: String,
    /// Listing rows in display order.
    pub rows: Vec<RowView>,
    /// Inline error text, or `None`; cleared by the next successful listing.
    pub error: Option<String>,
    /// Progress bar fraction in `0.0..=1.0`.
    pub fraction: f64,
    /// Progress bar text.
    pub progress_text: String,
    /// Transfer status line.
    pub status: String,
    /// Whether the sidebar has been shown — in view in the side dock, or
    /// built while `sftp_on_connect` is on. A hidden sidebar keeps its
    /// connection for the first show.
    pub shown: bool,
    /// Whether a live SFTP client is attached.
    pub connected: bool,
    /// Running transfers, oldest first; the bar shows whichever updated last.
    pub transfers: Vec<TransferState>,
    /// Derived button sensitivity.
    pub toolbar: ToolbarState,
    /// The selected row, so toolbar rules can be recomputed.
    selected: Option<RowView>,
}

impl SftpView {
    /// A sidebar with no client yet: empty listing, everything disabled.
    pub fn detached() -> Self {
        Self {
            path: String::new(),
            path_text: START_PATH.to_string(),
            rows: Vec::new(),
            error: None,
            fraction: 0.0,
            progress_text: String::new(),
            status: String::new(),
            shown: true,
            connected: false,
            transfers: Vec::new(),
            toolbar: ToolbarState::DISABLED,
            selected: None,
        }
    }

    /// A client was attached: the toolbar comes alive.
    pub fn attach(&mut self) {
        self.connected = true;
        self.error = None;
        self.recompute_toolbar();
    }

    /// A jumped connection (doc 11 v1): no SFTP task is ever started over
    /// it — the view keeps the limitation as its inline error and the
    /// toolbar stays disabled.
    pub fn mark_jump_unsupported(&mut self) {
        self.connected = false;
        self.error = Some(JUMPED_UNAVAILABLE.to_string());
        self.recompute_toolbar();
    }

    /// Directory the sidebar browses — [`START_PATH`] until the first listing.
    pub fn current_path(&self) -> &str {
        if self.path.is_empty() {
            START_PATH
        } else {
            &self.path
        }
    }

    /// Show a successful listing of `path` and make it the current directory.
    pub fn set_listing(&mut self, path: &str, entries: &[DirEntry]) {
        self.rows = listing_rows(path, entries);
        self.path = path.to_string();
        self.path_text = path.to_string();
        self.error = None;
        self.recompute_toolbar();
    }

    /// A failed `Cd` leaves the listing and path bar where they were.
    pub fn set_listing_failed(&mut self, err: &str) {
        self.set_error(err);
    }

    /// Show an inline error without touching the listing.
    pub fn set_error(&mut self, err: &str) {
        self.error = Some(err.to_string());
    }

    /// Drop the inline error.
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Record the selected listing row (or the lack of one).
    pub fn select(&mut self, row: Option<&RowView>) {
        self.selected = row.cloned();
        self.recompute_toolbar();
    }

    /// Remote path of the selected row (never `..`).
    pub fn selected_path(&self) -> Option<String> {
        let row = self.selected.as_ref()?;
        if row.is_parent {
            return None;
        }
        Some(join_path(self.current_path(), &row.name))
    }

    /// A transfer started; it owns the progress display until it ends.
    pub fn begin_transfer(&mut self, id: u64, label: String) {
        self.status = started_status(&label);
        self.transfers.push(TransferState { id, label });
        self.fraction = 0.0;
        self.recompute_toolbar();
    }

    /// One progress sample from a running transfer.
    pub fn on_progress(&mut self, id: u64, transferred: u64, total: u64) {
        let Some(label) = self.transfer_label(id) else {
            return;
        };
        self.fraction = progress_fraction(transferred, total);
        self.progress_text = progress_text(transferred, total);
        self.status = running_status(&label, transferred, total);
    }

    /// The transfer ended: `Ok(bytes)` completes the bar at 100%, `Err` puts
    /// the failure in the status line.
    pub fn end_transfer(&mut self, id: u64, result: &Result<u64, String>) {
        let Some(index) = self.transfers.iter().position(|t| t.id == id) else {
            return;
        };
        let label = self.transfers[index].label.clone();
        self.transfers.remove(index);
        match result {
            Ok(bytes) => {
                self.fraction = 1.0;
                self.progress_text = progress_text(*bytes, *bytes);
                self.status = done_status(&label, *bytes);
            }
            Err(err) => {
                self.status = failed_status(&label, err);
            }
        }
        self.recompute_toolbar();
    }

    /// Cancel targets the most recently started transfer.
    pub fn cancel_target(&self) -> Option<u64> {
        self.transfers.last().map(|transfer| transfer.id)
    }

    /// Mark the sidebar shown; `true` on its first show.
    pub fn show(&mut self) -> bool {
        !std::mem::replace(&mut self.shown, true)
    }

    fn transfer_label(&self, id: u64) -> Option<String> {
        self.transfers
            .iter()
            .find(|transfer| transfer.id == id)
            .map(|transfer| transfer.label.clone())
    }

    fn recompute_toolbar(&mut self) {
        let selection = selection_of(self.selected.as_ref());
        self.toolbar = toolbar_state(
            selection.as_ref(),
            self.connected,
            !self.transfers.is_empty(),
        );
    }
}
