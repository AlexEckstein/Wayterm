//! What the SFTP sidebar shows, as plain data: listing rows, toolbar
//! rules, transfer status text, and the [`SftpView`] the handlers mutate
//! before each render. Unit-tested without a display, like the rest of
//! the display-free half (`settings`, `paths`, `edit_sync`).

use super::paths::{join_path, parent_path};
use crate::sftp::DirEntry;

/// The synthetic "go up" row.
pub const PARENT_ROW: &str = "..";

/// Where the sidebar starts browsing, unless the shell reports its
/// directory first ([`SftpView::start_at`]).
pub const START_PATH: &str = ".";

/// Row icons, bundled with the app (`ui::icons`).
pub const DIR_ICON: &str = "wayterm-folder-symbolic";
pub const FILE_ICON: &str = "wayterm-file-symbolic";
pub const PARENT_ICON: &str = "wayterm-folder-up-symbolic";

/// Inline error shown while no SSH connection backs the sidebar.
pub const NOT_CONNECTED: &str = "SFTP needs a live SSH connection";
/// The same, on a local-shell tab: its file task runs while the shell does.
pub const SHELL_NOT_RUNNING: &str = "Browsing files needs the tab's shell running";
/// No SFTP task runs over a jumped connection (doc 04 §Limitations), so the sidebar
/// shows this instead of attaching.
pub const JUMPED_UNAVAILABLE: &str = "SFTP is not available for jumped sessions yet";
/// Doc 07: a dead SFTP task is observable as an errored reply.
pub const SESSION_ENDED: &str = "SFTP session ended — reconnect?";
/// The command could not be queued at all (task gone, channel closed).
pub const COMMAND_FAILED: &str = "SFTP command could not be queued";

/// One listing row, in store order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowView {
    /// An icon name: [`DIR_ICON`], [`FILE_ICON`] or [`PARENT_ICON`].
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

/// Byte counts, e.g. `1.2 KB / 4.0 MB`.
pub fn progress_text(transferred: u64, total: u64) -> String {
    if total == 0 {
        format!("{} / ?", format_size(transferred))
    } else {
        format!("{} / {}", format_size(transferred), format_size(total))
    }
}

/// Bar text for a transfer that has sent no sample yet.
pub fn started_text(label: &str) -> String {
    format!("{label} — starting…")
}

/// Bar text for one progress sample.
pub fn running_text(label: &str, transferred: u64, total: u64) -> String {
    format!("{label} — {}", progress_text(transferred, total))
}

/// Status line for a finished transfer.
pub fn done_status(label: &str, bytes: u64) -> String {
    format!("{label} — done — {}", format_size(bytes))
}

/// Status line for a failed transfer.
pub fn failed_status(label: &str, err: &str) -> String {
    format!("{label} — failed: {err}")
}

/// Button sensitivity: doc 07's six toolbar buttons plus Cancel.
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
    selected: Option<&RowView>,
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

/// One running transfer, in start order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferState {
    pub id: u64,
    /// Bar and status-line prefix, e.g. `Upload notes.txt`.
    pub label: String,
    /// The last `(transferred, total)` sample, if any came yet.
    sample: Option<(u64, u64)>,
}

/// What the progress bar shows while a transfer runs.
#[derive(Clone, Debug, PartialEq)]
pub struct BarState {
    pub fraction: f64,
    pub text: String,
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
    /// The last transfer result, "" for none: a success until it expires,
    /// a failure until the next transfer starts.
    pub status: String,
    /// The transfer whose success `status` shows, so it can expire.
    done_by: Option<u64>,
    /// Whether the sidebar has been shown — in view in the side dock, or
    /// built while `sftp_on_connect` is on. A hidden sidebar keeps its
    /// connection for the first show.
    pub shown: bool,
    /// Whether a live SFTP client is attached.
    pub connected: bool,
    /// Where the first listing starts: [`START_PATH`], or the shell's
    /// directory when it was reported first.
    start: String,
    /// "Follow terminal directory": the listing moves when the shell
    /// changes directory.
    pub following: bool,
    /// The shell's directory as last seen, reported (OSC 7) or probed.
    shell_dir: Option<String>,
    /// The shell reports its directory itself, so probing stops.
    shell_reports: bool,
    /// Running transfers, oldest first.
    pub transfers: Vec<TransferState>,
    /// The transfer that sent the last sample; the bar shows it.
    bar_follows: Option<u64>,
    /// The selected listing row, if any.
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
            status: String::new(),
            done_by: None,
            shown: true,
            connected: false,
            start: START_PATH.to_string(),
            following: true,
            shell_dir: None,
            shell_reports: false,
            transfers: Vec::new(),
            bar_follows: None,
            selected: None,
        }
    }

    /// A client was attached: the toolbar comes alive.
    pub fn attach(&mut self) {
        self.connected = true;
        self.error = None;
    }

    /// A jumped connection (doc 04 §Limitations): no SFTP task is ever started over
    /// it — the view keeps the limitation as its inline error and the
    /// toolbar stays disabled.
    pub fn mark_jump_unsupported(&mut self) {
        self.connected = false;
        self.error = Some(JUMPED_UNAVAILABLE.to_string());
    }

    /// Directory the sidebar browses — the start path until the first
    /// listing.
    pub fn current_path(&self) -> &str {
        if self.path.is_empty() {
            &self.start
        } else {
            &self.path
        }
    }

    /// Start browsing at `path` instead of [`START_PATH`]: the shell
    /// reported its directory before a client was attached. Ignored once
    /// a listing exists.
    pub fn start_at(&mut self, path: &str) {
        if self.path.is_empty() {
            self.start = path.to_string();
            self.path_text = path.to_string();
        }
    }

    /// The shell is in `path`: `reported` by the shell itself (OSC 7) or
    /// found by a probe. `Some(path)` when the listing should go there:
    /// following is on and the shell moved, so browsing elsewhere sticks
    /// until the shell changes directory.
    pub fn shell_moved(&mut self, path: &str, reported: bool) -> Option<String> {
        self.shell_reports |= reported;
        if self.shell_dir.as_deref() == Some(path) {
            return None;
        }
        self.shell_dir = Some(path.to_string());
        self.following.then(|| path.to_string())
    }

    /// Tick or untick "Follow terminal directory". Ticking it returns the
    /// shell's last directory when the listing is elsewhere.
    pub fn set_following(&mut self, on: bool) -> Option<String> {
        self.following = on;
        let dir = self.shell_dir.as_deref().filter(|_| on)?;
        (dir != self.current_path()).then(|| dir.to_string())
    }

    /// Whether to ask the remote where the shell is: following is on and
    /// the shell does not report it.
    pub fn wants_probe(&self) -> bool {
        self.following && !self.shell_reports
    }

    /// The tab connected again: a new shell, whose directory is unknown.
    pub fn new_shell(&mut self) {
        self.shell_dir = None;
        self.shell_reports = false;
    }

    /// Show a successful listing of `path` and make it the current directory.
    pub fn set_listing(&mut self, path: &str, entries: &[DirEntry]) {
        self.rows = listing_rows(path, entries);
        self.path = path.to_string();
        self.path_text = path.to_string();
        self.error = None;
    }

    /// Show an inline error without touching the listing or the path bar
    /// (a failed `Cd` leaves both where they were).
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
    }

    /// Remote path of the selected row (never `..`).
    pub fn selected_path(&self) -> Option<String> {
        let row = self.selected.as_ref()?;
        if row.is_parent {
            return None;
        }
        Some(join_path(self.current_path(), &row.name))
    }

    /// A transfer started: the bar shows it, and the last result goes.
    pub fn begin_transfer(&mut self, id: u64, label: String) {
        self.transfers.push(TransferState {
            id,
            label,
            sample: None,
        });
        self.bar_follows = Some(id);
        self.set_status(String::new(), None);
    }

    /// One progress sample from a running transfer.
    pub fn on_progress(&mut self, id: u64, transferred: u64, total: u64) {
        let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == id) else {
            return;
        };
        transfer.sample = Some((transferred, total));
        self.bar_follows = Some(id);
    }

    /// The transfer ended and its result goes to the status line. A
    /// success does not hide another transfer's failure.
    pub fn end_transfer(&mut self, id: u64, result: &Result<u64, String>) {
        let Some(index) = self.transfers.iter().position(|t| t.id == id) else {
            return;
        };
        let label = self.transfers.remove(index).label;
        if self.bar_follows == Some(id) {
            self.bar_follows = None;
        }
        match result {
            Ok(bytes) if !self.shows_failure() => {
                self.set_status(done_status(&label, *bytes), Some(id));
            }
            Ok(_) => {}
            Err(err) => self.set_status(failed_status(&label, err), None),
        }
    }

    /// Drop the success line transfer `id` left, if it still shows.
    pub fn expire_status(&mut self, id: u64) {
        if self.done_by == Some(id) {
            self.set_status(String::new(), None);
        }
    }

    /// The progress bar, while a transfer runs: the one that sent the last
    /// sample, or else the newest.
    pub fn bar(&self) -> Option<BarState> {
        let followed = self
            .bar_follows
            .and_then(|id| self.transfers.iter().find(|t| t.id == id));
        let transfer = followed.or(self.transfers.last())?;
        Some(match transfer.sample {
            Some((transferred, total)) => BarState {
                fraction: progress_fraction(transferred, total),
                text: running_text(&transfer.label, transferred, total),
            },
            None => BarState {
                fraction: 0.0,
                text: started_text(&transfer.label),
            },
        })
    }

    fn set_status(&mut self, status: String, done_by: Option<u64>) {
        self.status = status;
        self.done_by = done_by;
    }

    fn shows_failure(&self) -> bool {
        !self.status.is_empty() && self.done_by.is_none()
    }

    /// Cancel targets the most recently started transfer.
    pub fn cancel_target(&self) -> Option<u64> {
        self.transfers.last().map(|transfer| transfer.id)
    }

    /// Mark the sidebar shown; `true` on its first show.
    pub fn show(&mut self) -> bool {
        !std::mem::replace(&mut self.shown, true)
    }

    /// The toolbar's button sensitivity for the current state.
    pub fn toolbar(&self) -> ToolbarState {
        toolbar_state(
            self.selected.as_ref(),
            self.connected,
            !self.transfers.is_empty(),
        )
    }
}
