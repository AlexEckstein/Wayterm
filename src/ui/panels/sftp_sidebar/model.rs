//! The display-free half of the SFTP sidebar: listing rows, the visible
//! columns, toolbar rules, transfer status text, `text/uri-list` parsing,
//! following the shell's directory, and keeping a file opened in an editor
//! in step with the server. Everything here is plain data, unit-tested
//! without a display.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::SystemTime;

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

/// Settings row (doc 13 §Settings rows): open the sidebar's SFTP channel when an SSH tab
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

/// Settings row (doc 13 §Settings rows): the listing's optional columns
/// that are shown, as a comma-separated list of [`Column::id`]s. Missing
/// means all of them.
pub const SFTP_COLUMNS_SETTING: &str = "sftp_columns";

/// A listing column the user can hide; Name always shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Column {
    Size,
    Modified,
    Permissions,
}

impl Column {
    /// Listing order, after Name.
    pub const ALL: [Column; 3] = [Column::Size, Column::Modified, Column::Permissions];

    /// The column's name in [`SFTP_COLUMNS_SETTING`] and in the header
    /// menu's actions.
    pub fn id(self) -> &'static str {
        match self {
            Column::Size => "size",
            Column::Modified => "modified",
            Column::Permissions => "permissions",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Column::Size => "Size",
            Column::Modified => "Modified",
            Column::Permissions => "Permissions",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which optional columns the listing shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Columns(u8);

impl Columns {
    pub const ALL: Self = Self(0b111);
    pub const NONE: Self = Self(0);

    /// The stored row; a missing one shows every column, and names it does
    /// not know are skipped.
    pub fn from_setting(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::ALL;
        };
        let mut columns = Self::NONE;
        for id in value.split(',').map(str::trim) {
            if let Some(column) = Column::ALL.into_iter().find(|column| column.id() == id) {
                columns = columns.with(column, true);
            }
        }
        columns
    }

    pub fn to_setting(self) -> String {
        let shown: Vec<&str> = Column::ALL
            .into_iter()
            .filter(|column| self.shows(*column))
            .map(Column::id)
            .collect();
        shown.join(",")
    }

    pub fn shows(self, column: Column) -> bool {
        self.0 & column.bit() != 0
    }

    pub fn with(self, column: Column, shown: bool) -> Self {
        if shown {
            Self(self.0 | column.bit())
        } else {
            Self(self.0 & !column.bit())
        }
    }
}

static COLUMNS: AtomicU8 = AtomicU8::new(Columns::ALL.0);

type SaveColumns = Box<dyn Fn(Columns)>;

thread_local! {
    static SAVE_COLUMNS: RefCell<Option<SaveColumns>> = const { RefCell::new(None) };
}

/// `sftp_columns`, pushed in at startup; every sidebar shows these.
pub fn install_columns(columns: Columns) {
    COLUMNS.store(columns.0, Ordering::SeqCst);
}

pub fn columns() -> Columns {
    Columns(COLUMNS.load(Ordering::SeqCst))
}

/// How a choice made in a column header's menu is stored.
pub fn save_columns_with(save: impl Fn(Columns) + 'static) {
    SAVE_COLUMNS.with(|slot| *slot.borrow_mut() = Some(Box::new(save)));
}

/// The user picked `columns` in a header menu: every sidebar shows them
/// from now on, and they are stored.
pub fn choose_columns(columns: Columns) {
    install_columns(columns);
    SAVE_COLUMNS.with(|slot| {
        if let Some(save) = slot.borrow().as_ref() {
            save(columns);
        }
    });
}

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

/// Where a local-shell tab's sidebar goes for `typed` (doc 07 §Local
/// tabs): `~` is `home`, a relative path starts at `current`, and `.` and
/// `..` steps are taken by name, as a shell's `cd` takes them.
pub fn local_target(current: &str, typed: &str, home: &str) -> String {
    let base = if current.starts_with('/') {
        current
    } else {
        home
    };
    let joined = match typed.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{home}{rest}"),
        _ => join_path(base, typed),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => _ = parts.pop(),
            name => parts.push(name),
        }
    }
    format!("/{}", parts.join("/"))
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
    let (host, path) = split_file_uri(uri)?;
    (host.is_empty() || host.eq_ignore_ascii_case("localhost")).then_some(path)
}

/// A `file://` URI's host and percent-decoded path.
fn split_file_uri(uri: &str) -> Option<(&str, String)> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let path = percent_decode(&rest[slash..]);
    (!path.is_empty()).then(|| (&rest[..slash], path))
}

/// Reads the shell's working directory from the terminal's OSC 7 reports
/// (`file://host/path`, doc 07 §Following the shell).
///
/// The first report pins the host: a nested `ssh` inside the session
/// reports another one, and its paths mean nothing to this session's SFTP
/// channel. A local shell's follower knows its host from the start
/// ([`local`](Self::local)): its own shell may never report, and a nested
/// `ssh` must not be taken for it. Whether the directory changed is
/// [`SftpView::shell_moved`]'s call, since probes find it too.
#[derive(Clone, Debug, Default)]
pub struct DirectoryFollower {
    host: Option<String>,
    /// This machine's name, for a local shell.
    local: Option<String>,
}

impl DirectoryFollower {
    /// For a shell on this machine, named `hostname`: only its own reports
    /// count — no host, `localhost`, or `hostname`, where a name with no
    /// domain matches the same name with one.
    pub fn local(hostname: &str) -> Self {
        Self {
            host: None,
            local: Some(hostname.to_string()),
        }
    }

    /// The reported directory, or `None` when the report is not a
    /// `file://` URI for this session's host.
    pub fn report(&mut self, uri: &str) -> Option<String> {
        let (host, path) = split_file_uri(uri)?;
        if let Some(own) = &self.local {
            return is_this_machine(host, own).then_some(path);
        }
        match &self.host {
            Some(pinned) if !pinned.eq_ignore_ascii_case(host) => return None,
            Some(_) => {}
            None => self.host = Some(host.to_string()),
        }
        Some(path)
    }
}

fn is_this_machine(host: &str, own: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let (host, own) = (host.to_ascii_lowercase(), own.to_ascii_lowercase());
    match (host.split_once('.'), own.split_once('.')) {
        (Some((short, _)), None) => short == own,
        (None, Some((short, _))) => host == short,
        _ => host == own,
    }
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

/// Where the local copies of files opened in an editor live:
/// `$XDG_CACHE_HOME/wayterm/sftp-edit` (doc 07 §Opening files). `None`
/// without a cache directory of the user's own: a shared temporary
/// directory is no place for a remote file's contents.
pub fn edit_root() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(cache.join("wayterm").join("sftp-edit"))
}

/// A new directory under `root`, private to its owner, for one opened
/// file. Each file gets its own, so it keeps its name and whatever an
/// editor writes beside it is never mistaken for another opened file.
/// A `root` that is not this user's own private directory is refused,
/// whoever made it.
pub fn create_edit_dir(root: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let mut private = std::fs::DirBuilder::new();
    private.mode(0o700).recursive(true).create(root)?;
    let meta = std::fs::symlink_metadata(root)?;
    // SAFETY: `geteuid` only reads the process's credentials.
    let own = meta.uid() == unsafe { libc::geteuid() };
    if !meta.is_dir() || !own || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of yours", root.display()),
        ));
    }
    private.recursive(false);
    let pid = std::process::id();
    for n in 0u32.. {
        let dir = root.join(format!("{pid}-{n}"));
        match private.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    unreachable!("the counter never ends")
}

/// Whether `name` can name a local copy: one path component, and not a
/// directory reference.
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// What tells one saved version of a local file from the next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl FileStamp {
    /// `None` while there is no regular file at `path`, as in the middle
    /// of an editor's save.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_file())?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// Move a finished download from `staged` over the local copy; the
/// stamp the copy then has, or `None` when it could not be moved.
pub fn place_download(staged: &Path, local: &Path) -> Option<FileStamp> {
    let stamp = FileStamp::of(staged)?;
    std::fs::rename(staged, local).ok()?;
    Some(stamp)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Running {
    Fetch,
    Upload,
}

/// What opening an already opened file does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reopen {
    /// A transfer of the file runs: its end shows or stores it.
    Busy,
    /// The local copy holds a version the server lacks: send that, and
    /// show it, instead of downloading over it.
    Unsent,
    /// Download the server's version again.
    Fetch,
}

/// Keeps the local copy of a file opened in an editor in step with the
/// server (doc 07 §Opening files): each version the editor saves is
/// uploaded once, one transfer at a time, and a download never replaces a
/// version the server lacks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditSync {
    /// The local version the server has, or is being sent.
    synced: Option<FileStamp>,
    running: Option<Running>,
    /// The copy changed during a transfer: look again when it ends.
    changed_meanwhile: bool,
}

impl EditSync {
    /// A file whose first download is about to start.
    pub fn fetching() -> Self {
        Self {
            running: Some(Running::Fetch),
            ..Self::default()
        }
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    /// Whether the copy, now at `local`, holds nothing the server lacks:
    /// it is as last downloaded or uploaded, or not there at all.
    pub fn is_sent(&self, local: Option<FileStamp>) -> bool {
        local.is_none() || local == self.synced
    }

    /// The file is opened again while its copy is at `local`.
    /// [`Reopen::Fetch`] starts a download, to be reported with
    /// [`fetch_ended`](Self::fetch_ended).
    pub fn reopen(&mut self, local: Option<FileStamp>) -> Reopen {
        if self.is_busy() {
            Reopen::Busy
        } else if !self.is_sent(local) {
            Reopen::Unsent
        } else {
            self.running = Some(Running::Fetch);
            Reopen::Fetch
        }
    }

    /// The download ended. `placed` is the copy's stamp if the download
    /// replaced it; a failed download, or one the editor overtook with a
    /// save, leaves the copy as it was. `true` when the copy changed
    /// meanwhile and must be looked at.
    pub fn fetch_ended(&mut self, placed: Option<FileStamp>) -> bool {
        self.running = None;
        if placed.is_some() {
            self.synced = placed;
        }
        std::mem::take(&mut self.changed_meanwhile)
    }

    /// The local copy may have changed and is now at `stamp`. `true` when
    /// it must be uploaded: the caller starts the upload, then reports
    /// [`uploaded`](Self::uploaded).
    pub fn local_changed(&mut self, stamp: FileStamp) -> bool {
        if self.is_busy() {
            self.changed_meanwhile = true;
            return false;
        }
        if self.synced == Some(stamp) {
            return false;
        }
        self.synced = Some(stamp);
        self.running = Some(Running::Upload);
        true
    }

    /// The upload ended. `true` when the file must be looked at again: it
    /// changed while the upload ran. A failed upload leaves the server
    /// behind, so the next change, or look, uploads again.
    pub fn uploaded(&mut self, ok: bool) -> bool {
        self.running = None;
        if !ok {
            self.synced = None;
        }
        std::mem::take(&mut self.changed_meanwhile)
    }

    /// The session ended under whatever transfer ran. An upload cut off
    /// may not have reached the server, so the copy counts as unsent.
    pub fn interrupted(&mut self) {
        if self.running.take() == Some(Running::Upload) {
            self.synced = None;
        }
        self.changed_meanwhile = false;
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
    /// Running transfers, oldest first; the bar shows whichever updated last.
    pub transfers: Vec<TransferState>,
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
            fraction: 0.0,
            progress_text: String::new(),
            status: String::new(),
            shown: true,
            connected: false,
            start: START_PATH.to_string(),
            following: true,
            shell_dir: None,
            shell_reports: false,
            transfers: Vec::new(),
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

    /// A transfer started; it owns the progress display until it ends.
    pub fn begin_transfer(&mut self, id: u64, label: String) {
        self.status = started_status(&label);
        self.transfers.push(TransferState { id, label });
        self.fraction = 0.0;
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

    /// The toolbar's button sensitivity for the current state.
    pub fn toolbar(&self) -> ToolbarState {
        toolbar_state(
            self.selected.as_ref(),
            self.connected,
            !self.transfers.is_empty(),
        )
    }
}
