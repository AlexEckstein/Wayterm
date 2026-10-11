//! The SFTP sidebar — the GTK half of doc 09. It holds an [`SftpClient`],
//! sends commands, and applies the replies from `spawn_local` futures, so
//! no GTK callback ever awaits.
//!
//! [`SftpView`] and the helpers above it are display-free (the listing,
//! toolbar rules, `text/uri-list` parsing); [`SftpSidebar`] only renders.
//! Drops are accepted as `GdkFileList` and as `text/uri-list` through two
//! drop targets, because gtk4-rs does not bind the multi-type API. The
//! path bar shows the requested path (the protocol has no `getcwd`), and
//! browsing starts at `.`, the SFTP home directory.

// GtkTreeView/TreeStore and FileChooserNative are deprecated since GTK
// 4.10; migrating is UI polish, not a contract change.
#![allow(deprecated)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use crate::backpressure::PendingQueue;
use crate::sftp::{DirEntry, Progress, SftpClient, SftpCommand};
use crate::store::model::SessionType;
use crate::transport::connection::SshConnection;
use crate::ui::widgets::{confirm, prompt_text, set_margins, window_of};

use gdk4 as gdk;
use gio;

// ---------------------------------------------------------------------------
// Model layer (display-free)
// ---------------------------------------------------------------------------

/// Column index of the row icon in the listing's `GtkTreeStore`.
pub const COL_ICON: u32 = 0;
/// Column index of the entry name in the listing's `GtkTreeStore`.
pub const COL_NAME: u32 = 1;
/// Column index of the human-readable size in the listing's `GtkTreeStore`.
pub const COL_SIZE: u32 = 2;
/// Column index of the modification timestamp in the listing's `GtkTreeStore`.
pub const COL_MODIFIED: u32 = 3;
/// Column index of the permission bits in the listing's `GtkTreeStore`.
pub const COL_PERMS: u32 = 4;
/// Hidden column: the row's index into [`SftpView::rows`].
pub const COL_ROW: u32 = 5;

/// The synthetic "go up" row.
pub const PARENT_ROW: &str = "..";

/// Where the sidebar starts browsing.
pub const START_PATH: &str = ".";

const DIR_ICON: &str = "📁";
const FILE_ICON: &str = "📄";
const PARENT_ICON: &str = "⬆";

/// Inline error shown while no SSH connection backs the sidebar.
pub const NOT_CONNECTED: &str = "SFTP needs a live SSH connection";
/// Doc 09: a dead SFTP task is observable as an errored reply.
pub const SESSION_ENDED: &str = "SFTP session ended — reconnect?";
/// The command could not be queued at all (task gone, channel closed).
pub const COMMAND_FAILED: &str = "SFTP command could not be queued";

/// Settings row (doc 06): show the sidebar and open its SFTP channel when
/// an SSH tab connects. Off, the sidebar starts hidden and attaches on its
/// first show, so no channel opens for a sidebar nobody looks at.
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
pub fn attach_on_connect(open_on_connect: bool, visible: bool) -> bool {
    open_on_connect || visible
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
    /// Whether the sidebar part of the paned is shown.
    pub visible: bool,
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
            visible: true,
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

    /// Show or hide the sidebar; returns the new visibility.
    pub fn toggle_visible(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
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

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// The toolbar buttons plus Cancel.
struct Buttons {
    upload: gtk4::Button,
    download: gtk4::Button,
    delete: gtk4::Button,
    rename: gtk4::Button,
    refresh: gtk4::Button,
    mkdir: gtk4::Button,
    cancel: gtk4::Button,
}

/// Widgets plus state, shared by the callbacks and reply futures.
struct SidebarInner {
    container: gtk4::Box,
    path_entry: gtk4::Entry,
    tree_view: gtk4::TreeView,
    model: gtk4::TreeStore,
    error_label: gtk4::Label,
    progress_bar: gtk4::ProgressBar,
    status_label: gtk4::Label,
    buttons: Buttons,
    state: RefCell<SftpView>,
    /// The live client, once [`SftpSidebar::attach`] has been called.
    client: RefCell<Option<SftpClient>>,
    /// A connection waiting for the first show (`sftp_on_connect` off).
    deferred: RefCell<Option<Arc<SshConnection>>>,
    /// Commands waiting for room in the bounded channel.
    pending: RefCell<PendingQueue<SftpCommand>>,
    /// The idle flush of `pending`, while scheduled.
    flush_source: RefCell<Option<glib::SourceId>>,
    /// Keeps an open file chooser alive until its response arrives.
    chooser: RefCell<Option<gtk4::FileChooserNative>>,
}

/// The SFTP sidebar (doc 09). Clones share everything; dropping the last
/// one ends the SFTP task, never the shared SSH connection.
#[derive(Clone)]
pub struct SftpSidebar {
    inner: Rc<SidebarInner>,
}

impl SftpSidebar {
    /// A sidebar with no client yet: empty and disabled until
    /// [`attach`](Self::attach).
    pub fn new() -> Self {
        Self {
            inner: build_sidebar(),
        }
    }

    /// Start an SFTP task over the shared connection and list
    /// [`START_PATH`]. Re-attaching replaces (and ends) the previous task.
    pub fn attach(&self, connection: Arc<SshConnection>) {
        self.inner.deferred.borrow_mut().take();
        *self.inner.client.borrow_mut() = Some(SftpClient::new(connection));
        self.inner.state.borrow_mut().attach();
        self.inner.render();
        let start = self.inner.state.borrow().current_path().to_string();
        self.inner.cd(start);
    }

    /// The root widget (the start child of the tab's paned).
    pub fn widget(&self) -> &gtk4::Widget {
        self.inner.container.upcast_ref()
    }

    /// Attach the newly connected tab's sidebar now, or keep the
    /// connection for its first show (see [`attach_on_connect`]).
    pub fn attach_on_connect(&self, connection: Arc<SshConnection>) {
        let visible = self.inner.state.borrow().visible;
        if attach_on_connect(open_on_connect(), visible) {
            self.attach(connection);
        } else {
            *self.inner.deferred.borrow_mut() = Some(connection);
        }
    }

    /// Show or hide the sidebar (`toggle_sftp`); returns the new state.
    /// The first show attaches a deferred connection.
    pub fn toggle_visible(&self) -> bool {
        let visible = self.inner.toggle_visible();
        if visible {
            let deferred = self.inner.deferred.borrow_mut().take();
            if let Some(connection) = deferred {
                self.attach(connection);
            }
        }
        visible
    }
}

impl Default for SftpSidebar {
    fn default() -> Self {
        Self::new()
    }
}

/// A button carrying a symbolic icon and a tooltip naming the operation.
fn icon_button(icon_name: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

/// Append one `GtkTreeViewColumn` bound to a store column.
fn add_column(tree_view: &gtk4::TreeView, title: &str, column: u32, expand: bool) {
    let view_column = gtk4::TreeViewColumn::builder()
        .title(title)
        .expand(expand)
        .build();
    let cell = gtk4::CellRendererText::new();
    view_column.pack_start(&cell, true);
    // The store columns are 0..=5, so the narrowing is lossless.
    view_column.add_attribute(&cell, "text", column as i32);
    tree_view.append_column(&view_column);
}

/// Local paths of a chooser's selection (entries without one are skipped).
fn chooser_paths(chooser: &gtk4::FileChooserNative) -> Vec<String> {
    let model = chooser.files();
    (0..model.n_items())
        .filter_map(|index| model.item(index))
        .filter_map(|object| object.downcast::<gio::File>().ok())
        .filter_map(|file| file.path())
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// Only SSH sessions get a sidebar (doc 09 §Availability).
pub fn supports_sftp(session_type: &SessionType) -> bool {
    matches!(session_type, SessionType::SSH)
}

/// Build and wire the widgets.
fn build_sidebar() -> Rc<SidebarInner> {
    // 1. Path bar (doc 09 §Layout): Enter navigates.
    let path_entry = gtk4::Entry::new();
    path_entry.set_text(START_PATH);
    path_entry.set_placeholder_text(Some("remote path"));
    path_entry.set_hexpand(true);

    // 2. Toolbar — exactly doc 09's six buttons.
    let buttons = Buttons {
        upload: icon_button("document-send-symbolic", "Upload"),
        download: icon_button("document-receive-symbolic", "Download"),
        delete: icon_button("user-trash-symbolic", "Delete"),
        rename: icon_button("document-edit-symbolic", "Rename"),
        refresh: icon_button("view-refresh-symbolic", "Refresh"),
        mkdir: icon_button("folder-new-symbolic", "New folder"),
        cancel: gtk4::Button::with_label("Cancel"),
    };
    buttons
        .cancel
        .set_tooltip_text(Some("Cancel the running transfer"));
    buttons.cancel.set_sensitive(false);

    let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    for button in [
        &buttons.upload,
        &buttons.download,
        &buttons.delete,
        &buttons.rename,
        &buttons.refresh,
        &buttons.mkdir,
    ] {
        toolbar.append(button);
    }

    // 3. Listing: GtkTreeView + GtkTreeStore, one column per doc 09.
    let model = gtk4::TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::I64,
    ]);
    let tree_view = gtk4::TreeView::builder()
        .model(&model)
        .headers_visible(true)
        .build();
    add_column(&tree_view, "Name", COL_NAME, true);
    add_column(&tree_view, "Size", COL_SIZE, false);
    add_column(&tree_view, "Modified", COL_MODIFIED, false);
    add_column(&tree_view, "Perms", COL_PERMS, false);
    add_column(&tree_view, "", COL_ICON, false);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&tree_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    // 4. Inline error line, hidden while there is no error.
    let error_label = gtk4::Label::new(None);
    error_label.set_xalign(0.0);
    error_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    error_label.add_css_class("dim-label");
    error_label.set_visible(false);

    // 5. Progress bar with Cancel, then the transfer status line.
    let progress_bar = gtk4::ProgressBar::new();
    progress_bar.set_show_text(true);
    progress_bar.set_hexpand(true);
    let progress_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    progress_row.append(&progress_bar);
    progress_row.append(&buttons.cancel);

    let status_label = gtk4::Label::new(None);
    status_label.set_xalign(0.0);
    status_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    set_margins(&container, 4);
    container.append(&path_entry);
    container.append(&toolbar);
    container.append(&error_label);
    container.append(&scrolled);
    container.append(&progress_row);
    container.append(&status_label);

    let inner = Rc::new(SidebarInner {
        container,
        path_entry,
        tree_view,
        model,
        error_label,
        progress_bar,
        status_label,
        buttons,
        state: RefCell::new(SftpView::detached()),
        client: RefCell::new(None),
        deferred: RefCell::new(None),
        pending: RefCell::default(),
        flush_source: RefCell::new(None),
        chooser: RefCell::new(None),
    });
    inner.connect_signals();
    inner.connect_drag_drop();
    inner.render();
    inner
}

impl SidebarInner {
    /// Callbacks only queue work or update the model.
    fn connect_signals(self: &Rc<Self>) {
        // Enter in the path bar navigates to what is typed.
        let for_entry = Rc::clone(self);
        self.path_entry
            .connect_activate(move |entry| for_entry.cd(entry.text().to_string()));

        // Double-click a directory navigates into it; `..` goes up.
        let for_row = Rc::clone(self);
        self.tree_view
            .connect_row_activated(move |_tree_view, path, _column| {
                let index =
                    usize::try_from(path.indices().first().copied().unwrap_or(0)).unwrap_or(0);
                let target = {
                    let state = for_row.state.borrow();
                    match state.rows.get(index) {
                        Some(row) if row.is_parent => parent_path(state.current_path()),
                        Some(row) if row.is_dir => Some(join_path(state.current_path(), &row.name)),
                        _ => None,
                    }
                };
                if let Some(target) = target {
                    for_row.cd(target);
                }
            });

        // Selection drives the toolbar rules (model layer, unit-tested).
        let for_selection = Rc::clone(self);
        let selection = self.tree_view.selection();
        selection.set_mode(gtk4::SelectionMode::Single);
        selection.connect_changed(move |selection| {
            let index = selection.selected().and_then(|(model, iter)| {
                usize::try_from(model.get::<i64>(&iter, COL_ROW as i32)).ok()
            });
            let picked = {
                let state = for_selection.state.borrow();
                index.and_then(|index| state.rows.get(index)).cloned()
            };
            for_selection.state.borrow_mut().select(picked.as_ref());
            for_selection.render_toolbar();
        });

        let for_upload = Rc::clone(self);
        self.buttons
            .upload
            .connect_clicked(move |_| for_upload.choose_upload_files());

        let for_download = Rc::clone(self);
        self.buttons
            .download
            .connect_clicked(move |_| for_download.choose_download_target());

        let for_delete = Rc::clone(self);
        self.buttons
            .delete
            .connect_clicked(move |_| for_delete.delete_selected());

        let for_rename = Rc::clone(self);
        self.buttons
            .rename
            .connect_clicked(move |_| for_rename.rename_selected());

        let for_refresh = Rc::clone(self);
        self.buttons
            .refresh
            .connect_clicked(move |_| for_refresh.refresh());

        let for_mkdir = Rc::clone(self);
        self.buttons
            .mkdir
            .connect_clicked(move |_| for_mkdir.mkdir());

        let for_cancel = Rc::clone(self);
        self.buttons
            .cancel
            .connect_clicked(move |_| for_cancel.cancel_transfer());
    }

    /// Uploads from drops: a `GdkFileList` target and a `text/uri-list` one.
    fn connect_drag_drop(self: &Rc<Self>) {
        let files_target =
            gtk4::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let for_files = Rc::clone(self);
        files_target.connect_drop(move |_target, value, _x, _y| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<String> = list
                .files()
                .iter()
                .filter_map(|file| file.path())
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            if paths.is_empty() {
                return false;
            }
            for_files.queue_uploads(paths);
            true
        });
        self.tree_view.add_controller(files_target);

        let uri_target = gtk4::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::COPY);
        let for_uris = Rc::clone(self);
        uri_target.connect_drop(move |_target, value, _x, _y| {
            let Ok(text) = value.get::<String>() else {
                return false;
            };
            let paths = parse_uri_list(&text);
            if paths.is_empty() {
                return false;
            }
            for_uris.queue_uploads(paths);
            true
        });
        self.tree_view.add_controller(uri_target);
    }

    /// Flip the sidebar's visibility and render it.
    fn toggle_visible(self: &Rc<Self>) -> bool {
        let visible = self.state.borrow_mut().toggle_visible();
        self.render();
        visible
    }

    /// Render the view into the widgets.
    fn render(&self) {
        // Copy the rows out: `model.clear()` fires the selection callback,
        // which borrows the state mutably.
        let rows = self.state.borrow().rows.clone();
        self.model.clear();
        for (index, row) in rows.iter().enumerate() {
            let iter = self.model.append(None);
            self.model.set(
                &iter,
                &[
                    (COL_ICON, &row.icon),
                    (COL_NAME, &row.name),
                    (COL_SIZE, &row.size),
                    (COL_MODIFIED, &row.modified),
                    (COL_PERMS, &row.permissions),
                    (COL_ROW, &(index as i64)),
                ],
            );
        }

        let state = self.state.borrow();
        match &state.error {
            Some(err) => {
                self.error_label.set_text(err);
                self.error_label.set_visible(true);
            }
            None => {
                self.error_label.set_text("");
                self.error_label.set_visible(false);
            }
        }

        self.progress_bar.set_fraction(state.fraction);
        self.progress_bar
            .set_text(Some(state.progress_text.as_str()));
        self.status_label.set_text(&state.status);
        self.container.set_visible(state.visible);
        drop(state);
        self.render_toolbar();
    }

    /// Separate from `render`, which must not re-enter from the selection
    /// callback that `TreeStore::clear` emits.
    fn render_toolbar(&self) {
        let toolbar = self.state.borrow().toolbar;
        self.buttons.upload.set_sensitive(toolbar.upload);
        self.buttons.download.set_sensitive(toolbar.download);
        self.buttons.delete.set_sensitive(toolbar.delete);
        self.buttons.rename.set_sensitive(toolbar.rename);
        self.buttons.refresh.set_sensitive(toolbar.refresh);
        self.buttons.mkdir.set_sensitive(toolbar.mkdir);
        self.buttons.cancel.set_sensitive(toolbar.cancel);
    }

    /// Reset the path bar to the directory actually shown.
    fn sync_path_entry(&self) {
        let text = self.state.borrow().path_text.clone();
        self.path_entry.set_text(&text);
    }

    fn client(&self) -> Option<SftpClient> {
        self.client.borrow().clone()
    }

    fn set_error(&self, err: &str) {
        self.state.borrow_mut().set_error(err);
        self.render();
    }

    /// Send a command, parking it for an idle flush if the channel is full.
    /// `false` when it cannot be delivered at all.
    fn send(self: &Rc<Self>, cmd: SftpCommand) -> bool {
        let Some(client) = self.client() else {
            self.set_error(NOT_CONNECTED);
            return false;
        };
        match client.try_request(cmd) {
            Ok(()) => true,
            Err(TrySendError::Full(cmd)) => {
                self.pending.borrow_mut().push(cmd);
                self.schedule_flush();
                true
            }
            Err(TrySendError::Closed(_)) => {
                self.pending.borrow_mut().clear();
                self.set_error(COMMAND_FAILED);
                false
            }
        }
    }

    /// One idle callback flushes `pending` until it is empty.
    fn schedule_flush(self: &Rc<Self>) {
        if self.flush_source.borrow().is_some() {
            return;
        }
        let sidebar = Rc::clone(self);
        let source = glib::idle_add_local(move || {
            if sidebar.flush_pending() {
                sidebar.flush_source.borrow_mut().take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        *self.flush_source.borrow_mut() = Some(source);
    }

    /// `true` when nothing is left waiting.
    fn flush_pending(&self) -> bool {
        let Some(client) = self.client() else {
            self.pending.borrow_mut().clear();
            return true;
        };
        let report = self
            .pending
            .borrow_mut()
            .drain_with(|cmd| client.try_request(cmd));
        if report.closed {
            self.set_error(COMMAND_FAILED);
            return true;
        }
        report.remaining == 0
    }

    /// Re-list the current directory.
    fn refresh(self: &Rc<Self>) {
        let path = self.state.borrow().current_path().to_string();
        self.cd(path);
    }

    /// Queue a `Cd` and apply the listing (or the error) when it answers.
    fn cd(self: &Rc<Self>, path: String) {
        if self.client().is_none() {
            self.set_error(NOT_CONNECTED);
            return;
        }
        let (reply, reply_rx) = oneshot::channel();
        if !self.send(SftpCommand::Cd {
            path: path.clone(),
            reply,
        }) {
            return;
        }

        let sidebar = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            match reply_rx.await {
                Ok(Ok(entries)) => sidebar.state.borrow_mut().set_listing(&path, &entries),
                Ok(Err(err)) => sidebar.state.borrow_mut().set_listing_failed(&err),
                // The task dropped the sender without answering.
                Err(_) => sidebar.state.borrow_mut().set_error(SESSION_ENDED),
            }
            sidebar.sync_path_entry();
            sidebar.render();
        });
    }

    /// Upload local files into the current directory (toolbar or drag-drop).
    fn queue_uploads(self: &Rc<Self>, local_paths: Vec<String>) {
        if self.client().is_none() {
            self.set_error(NOT_CONNECTED);
            return;
        }
        for local_path in local_paths {
            self.upload(local_path);
        }
    }

    fn upload(self: &Rc<Self>, local_path: String) {
        let name = file_name(&local_path);
        if name.is_empty() {
            return;
        }
        let remote = join_path(self.state.borrow().current_path(), &name);
        self.run_transfer(format!("Upload {name}"), move |id, progress, reply| {
            SftpCommand::Upload {
                transfer_id: id,
                local_path,
                remote_path: remote,
                progress,
                reply,
            }
        });
    }

    fn download(self: &Rc<Self>, remote: String, local: String) {
        let label = format!("Download {}", file_name(&remote));
        self.run_transfer(label, move |id, progress, reply| SftpCommand::Download {
            transfer_id: id,
            remote_path: remote,
            local_path: local,
            progress,
            reply,
        });
    }

    /// Queue a transfer, mirror its progress, and refresh when it ends.
    fn run_transfer(
        self: &Rc<Self>,
        label: String,
        make_cmd: impl FnOnce(
            u64,
            mpsc::Sender<Progress>,
            oneshot::Sender<Result<u64, String>>,
        ) -> SftpCommand,
    ) {
        let Some(client) = self.client() else {
            self.set_error(NOT_CONNECTED);
            return;
        };
        let transfer_id = client.next_transfer_id();
        let (progress_tx, mut progress_rx) = mpsc::channel::<Progress>(64);
        let (reply, reply_rx) = oneshot::channel();

        self.state.borrow_mut().begin_transfer(transfer_id, label);
        self.render();

        if !self.send(make_cmd(transfer_id, progress_tx, reply)) {
            // The command never reached the task — take the display back.
            self.state
                .borrow_mut()
                .end_transfer(transfer_id, &Err(COMMAND_FAILED.to_string()));
            self.render();
            return;
        }

        // Dropped samples are fine; drain as fast as they come.
        let pump = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            while let Some(sample) = progress_rx.recv().await {
                pump.state
                    .borrow_mut()
                    .on_progress(transfer_id, sample.transferred, sample.total);
                pump.render();
            }
        });

        // Completion: the reply carries the transferred byte count.
        let done = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let result = match reply_rx.await {
                Ok(result) => result,
                Err(_) => Err(SESSION_ENDED.to_string()),
            };
            let succeeded = result.is_ok();
            done.state.borrow_mut().end_transfer(transfer_id, &result);
            done.render();
            if succeeded {
                done.refresh();
            }
        });
    }

    fn cancel_transfer(self: &Rc<Self>) {
        let Some(transfer_id) = self.state.borrow().cancel_target() else {
            return;
        };
        let _ = self.send(SftpCommand::Cancel { transfer_id });
    }

    /// A file operation answering `()`; refresh on success.
    fn run_simple(
        self: &Rc<Self>,
        cmd: impl FnOnce(oneshot::Sender<Result<(), String>>) -> SftpCommand,
    ) {
        let (reply, reply_rx) = oneshot::channel();
        if !self.send(cmd(reply)) {
            return;
        }
        let sidebar = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            match reply_rx.await {
                Ok(Ok(())) => {
                    sidebar.state.borrow_mut().clear_error();
                    sidebar.render();
                    sidebar.refresh();
                }
                Ok(Err(err)) => sidebar.set_error(&err),
                Err(_) => sidebar.set_error(SESSION_ENDED),
            }
        });
    }

    /// Delete the selected row after confirmation.
    fn delete_selected(self: &Rc<Self>) {
        let Some((path, name)) = self.selected_target() else {
            return;
        };
        let sidebar = Rc::clone(self);
        confirm(
            self.parent_window().as_ref(),
            &format!("Delete '{name}'?"),
            "This cannot be undone. Directories must be empty.",
            "Delete",
            move || sidebar.run_simple(|reply| SftpCommand::Delete { path, reply }),
        );
    }

    /// Rename the selected row to what the user types.
    fn rename_selected(self: &Rc<Self>) {
        let Some((path, name)) = self.selected_target() else {
            return;
        };
        let parent = self.parent_window();
        let Some(new_name) = prompt_text(parent.as_ref(), "Rename", &name) else {
            return;
        };
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() || new_name == name {
            return;
        }
        let to = join_path(self.state.borrow().current_path(), &new_name);
        self.run_simple(|reply| SftpCommand::Rename {
            from: path.clone(),
            to: to.clone(),
            reply,
        });
    }

    fn mkdir(self: &Rc<Self>) {
        let parent = self.parent_window();
        let Some(name) = prompt_text(parent.as_ref(), "New Folder", "") else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let path = join_path(self.state.borrow().current_path(), &name);
        self.run_simple(|reply| SftpCommand::Mkdir { path, reply });
    }

    /// The selected row's path and name (never `..`).
    fn selected_target(&self) -> Option<(String, String)> {
        let state = self.state.borrow();
        let path = state.selected_path()?;
        let name = file_name(&path);
        (!name.is_empty() && name != PARENT_ROW).then_some((path, name))
    }

    /// Ask for local files and upload them into the current directory.
    fn choose_upload_files(self: &Rc<Self>) {
        let Some(parent) = self.parent_window() else {
            self.set_error("no window for the file chooser");
            return;
        };
        let dialog = gtk4::FileChooserNative::builder()
            .title("Upload files")
            .transient_for(&parent)
            .action(gtk4::FileChooserAction::Open)
            .select_multiple(true)
            .accept_label("Upload")
            .cancel_label("Cancel")
            .modal(true)
            .visible(true)
            .build();
        *self.chooser.borrow_mut() = Some(dialog.clone());

        let sidebar = Rc::clone(self);
        dialog.connect_response(move |dialog, response| {
            if response == gtk4::ResponseType::Accept {
                sidebar.queue_uploads(chooser_paths(dialog));
            }
            *sidebar.chooser.borrow_mut() = None;
        });
    }

    /// Ask where to save the selected row, then download it.
    fn choose_download_target(self: &Rc<Self>) {
        let Some((remote, name)) = self.selected_target() else {
            return;
        };
        let Some(parent) = self.parent_window() else {
            self.set_error("no window for the file chooser");
            return;
        };
        let dialog = gtk4::FileChooserNative::builder()
            .title("Download")
            .transient_for(&parent)
            .action(gtk4::FileChooserAction::Save)
            .accept_label("Download")
            .cancel_label("Cancel")
            .modal(true)
            .visible(true)
            .build();
        dialog.set_current_name(&name);
        *self.chooser.borrow_mut() = Some(dialog.clone());

        let sidebar = Rc::clone(self);
        dialog.connect_response(move |dialog, response| {
            if response == gtk4::ResponseType::Accept {
                let local = dialog
                    .file()
                    .and_then(|file| file.path())
                    .map(|path| path.to_string_lossy().into_owned());
                if let Some(local) = local {
                    sidebar.download(remote.clone(), local);
                }
            }
            *sidebar.chooser.borrow_mut() = None;
        });
    }

    /// The parent for dialogs and file choosers.
    fn parent_window(&self) -> Option<gtk4::Window> {
        window_of(&self.tree_view)
    }
}

#[cfg(test)]
mod tests;
