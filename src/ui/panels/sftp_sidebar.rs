//! The SFTP sidebar — the GTK half of doc 07. It holds an [`SftpClient`],
//! sends commands, and applies the replies from `spawn_local` futures, so
//! no GTK callback ever awaits.
//!
//! [`SftpView`] and the helpers in `model` are display-free (the listing,
//! toolbar rules, `text/uri-list` parsing, the [`DirectoryFollower`]);
//! [`SftpSidebar`] only renders.
//! Drops are accepted as `GdkFileList` and as `text/uri-list` through two
//! drop targets, because gtk4-rs does not bind the multi-type API. The
//! path bar shows the requested path (the protocol has no `getcwd`), and
//! browsing starts at `.`, the SFTP home directory, unless the shell's
//! directory is known by then. While "Follow terminal directory" is ticked
//! the sidebar follows the shell: from its OSC 7 reports, or, for a shell
//! that sends none, by asking the remote ([`ShellCwdProbe`]) after each
//! command.
//!
//! A file opens in the desktop's application for it from a local copy
//! ([`OpenFile`]); each version the application saves is uploaded over
//! the remote file (doc 07 §Opening files).
//!
//! A local-shell tab's sidebar ([`SftpSidebar::local`]) browses this
//! machine's files through the same commands (doc 07 §Local tabs): paths
//! are absolute, a file opens where it is, and nothing is probed.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, pango};
use tokio::sync::{mpsc, oneshot};

use crate::backpressure::IdleSender;
use crate::sftp::{Progress, SftpClient, SftpCommand, ShellCwdProbe};
use crate::transport::connection::SshConnection;
use crate::ui::widgets::{confirm, data, prompt_text, set_data, set_margins, window_of};

mod edit_sync;
mod model;
mod paths;
mod settings;
pub use edit_sync::*;
pub use model::*;
pub use paths::*;
pub use settings::*;

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
    /// A local-shell tab's: it browses this machine's files.
    local: bool,
    container: gtk4::Box,
    path_entry: gtk4::Entry,
    column_view: gtk4::ColumnView,
    /// The columns a header's menu can hide, each with the menu's action.
    columns: Vec<(Column, gtk4::ColumnViewColumn, gio::SimpleAction)>,
    /// The row menu, reused for every right-click on a row.
    row_menu: gtk4::PopoverMenu,
    /// The listing's rows, each a boxed [`RowView`].
    model: gio::ListStore,
    selection: gtk4::SingleSelection,
    error_label: gtk4::Label,
    /// The bar and Cancel, shown only while a transfer runs.
    progress_row: gtk4::Box,
    progress_bar: gtk4::ProgressBar,
    status_label: gtk4::Label,
    follow_check: gtk4::CheckButton,
    buttons: Buttons,
    state: RefCell<SftpView>,
    /// The live SFTP task, once [`SftpSidebar::attach`] has been called.
    attached: RefCell<Option<Attached>>,
    /// What to attach to on the first show (`sftp_on_connect` off, or a
    /// local tab).
    deferred: RefCell<Option<Files>>,
    /// Asks the remote where the current shell is.
    probe: RefCell<Option<ShellCwdProbe>>,
    /// A probe is running; another request waits for it (`probe_again`).
    probing: Cell<bool>,
    probe_again: Cell<bool>,
    /// The files opened in an editor since attaching.
    edits: RefCell<Vec<OpenFile>>,
    next_edit: Cell<u64>,
    /// Where their local copies go, if anywhere ([`edit_root`]).
    edit_root: RefCell<Option<PathBuf>>,
    /// What [`launch`](Self::launch) was asked to open, instead of opening it.
    #[cfg(test)]
    launched: RefCell<Vec<(PathBuf, bool)>>,
}

/// How long a local copy must stay untouched before it is looked at: an
/// editor's save is several writes.
const SETTLE: Duration = Duration::from_millis(300);

/// How long a finished transfer's line stays; a failure's stays until the
/// next transfer.
const DONE_SHOWS_FOR: Duration = Duration::from_secs(5);

/// Key under which each cell keeps a weak ref to its `ListItem`, so a
/// click can be traced back to a row.
const LIST_ITEM_KEY: &str = "wayterm-sftp-list-item";

/// A remote file opened in an editor: its local copy, watched for saves.
struct OpenFile {
    id: u64,
    remote: String,
    /// The local copy's own directory, removed with it.
    dir: PathBuf,
    local: PathBuf,
    /// Where a download lands before it replaces the copy, outside `dir`:
    /// a failed one leaves the copy alone.
    staged: PathBuf,
    sync: EditSync,
    /// Watches `dir`, once the first download has ended.
    monitor: Option<gio::FileMonitor>,
    /// The look at the local copy due once the editor stops writing.
    settle: Option<glib::SourceId>,
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            monitor.cancel();
        }
        if let Some(settle) = self.settle.take() {
            settle.remove();
        }
        let _ = std::fs::remove_file(&self.staged);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Remove every local copy of an opened file, whichever sidebar made it:
/// for startup, when they are a killed run's leftovers, and shutdown.
pub fn discard_edit_copies() {
    if let Some(root) = edit_root() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// One attached SFTP task: its client, and the never-blocking sender its
/// commands go through, in order (doc 02 §Backpressure).
#[derive(Clone)]
struct Attached {
    client: SftpClient,
    commands: IdleSender<SftpCommand>,
}

/// What a sidebar's task serves.
enum Files {
    Remote(Arc<SshConnection>),
    Local,
}

impl Attached {
    fn new(files: Files) -> Self {
        Self::with_client(match files {
            Files::Remote(connection) => SftpClient::new(connection),
            Files::Local => SftpClient::local(),
        })
    }

    fn with_client(client: SftpClient) -> Self {
        let commands = IdleSender::new(client.sender());
        Self { client, commands }
    }
}

/// A file transfer to queue.
enum Transfer {
    Download { remote: String, local: String },
    Upload { local: String, remote: String },
}

impl Transfer {
    fn command(
        self,
        transfer_id: u64,
        progress: mpsc::Sender<Progress>,
        reply: oneshot::Sender<Result<u64, String>>,
    ) -> SftpCommand {
        match self {
            Self::Download { remote, local } => SftpCommand::Download {
                transfer_id,
                remote_path: remote,
                local_path: local,
                progress,
                reply,
            },
            Self::Upload { local, remote } => SftpCommand::Upload {
                transfer_id,
                local_path: local,
                remote_path: remote,
                progress,
                reply,
            },
        }
    }
}

/// A toolbar button the `sftp_*` actions press (doc 11 §Handlers).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SftpButton {
    Upload,
    Download,
    Rename,
    Delete,
}

/// The SFTP sidebar (doc 07). Clones share everything; dropping the last
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
            inner: build_sidebar(false),
        }
    }

    /// A local-shell tab's sidebar, browsing this machine's files once
    /// [`attach_local_on_connect`](Self::attach_local_on_connect) has run.
    /// Not `shown`, it stays hidden, and reads no directory, until its
    /// first [`show`](Self::show).
    pub fn local(shown: bool) -> Self {
        let sidebar = Self {
            inner: build_sidebar(true),
        };
        sidebar.inner.state.borrow_mut().shown = shown;
        sidebar.inner.render();
        sidebar
    }

    pub fn is_local(&self) -> bool {
        self.inner.local
    }

    /// Start an SFTP task over the shared connection and list the start
    /// path ([`START_PATH`] or the shell's directory). Re-attaching
    /// replaces (and ends) the previous task.
    pub fn attach(&self, connection: Arc<SshConnection>) {
        self.attach_to(Files::Remote(connection));
    }

    fn attach_to(&self, files: Files) {
        self.inner.deferred.borrow_mut().take();
        let previous = self
            .inner
            .attached
            .borrow_mut()
            .replace(Attached::new(files));
        if previous.is_some() {
            // Its transfers still answer, about files this task never saw.
            self.inner.edits.borrow_mut().clear();
        }
        self.inner.state.borrow_mut().attach();
        self.inner.render();
        let start = self.inner.state.borrow().current_path().to_string();
        self.inner.cd(start);
        self.inner.send_unsent_edits();
        // A reattached tmux session may be anywhere already.
        self.inner.probe_shell();
    }

    /// The root widget, hosted by the side dock's SFTP panel while its tab
    /// is current (`ui::panels::side_dock`).
    pub fn widget(&self) -> &gtk4::Widget {
        self.inner.container.upcast_ref()
    }

    /// Attach the newly connected tab's sidebar now, or keep the
    /// connection for its first show (see [`attach_on_connect`]).
    pub fn attach_on_connect(&self, connection: Arc<SshConnection>) {
        if attach_on_connect(open_on_connect(), self.is_shown()) {
            self.attach(connection);
        } else {
            *self.inner.deferred.borrow_mut() = Some(Files::Remote(connection));
        }
    }

    /// A local-shell tab's shell started in `start`: browse from there,
    /// now if the sidebar has been shown, else on its first show.
    pub fn attach_local_on_connect(&self, start: &str) {
        {
            let mut state = self.inner.state.borrow_mut();
            state.new_shell();
            state.start_at(start);
        }
        self.inner.sync_path_entry();
        if self.is_shown() {
            self.attach_to(Files::Local);
        } else {
            *self.inner.deferred.borrow_mut() = Some(Files::Local);
        }
    }

    /// End the SFTP task now, cancelling its running transfers and
    /// discarding queued commands, and drop a connection kept for the
    /// first show. A transfer's own futures hold the sidebar until their
    /// reply comes, so dropping the last handle alone would let a transfer
    /// run on unseen. The local copies of files opened in an editor go
    /// too, unless another sidebar took them over first
    /// ([`adopt_open_files`](Self::adopt_open_files)).
    pub fn detach(&self) {
        self.inner.deferred.borrow_mut().take();
        self.inner.edits.borrow_mut().clear();
        if let Some(attached) = self.inner.attached.borrow_mut().take() {
            attached.client.shutdown();
        }
    }

    /// Show the jumped-session state instead of attaching: no SFTP task
    /// runs over a jumped connection (doc 04 §Limitations), so any deferred
    /// connection is dropped and the previous client, if any, is replaced
    /// by the limitation notice.
    pub fn show_jump_placeholder(&self) {
        self.inner.deferred.borrow_mut().take();
        *self.inner.attached.borrow_mut() = None;
        self.inner.edits.borrow_mut().clear();
        self.inner.state.borrow_mut().mark_jump_unsupported();
        self.inner.render();
    }

    /// Take over the files `old` has open in editors, before it detaches:
    /// the tab let go of its connection, and this sidebar replaces `old`.
    /// Their copies stay watched, and a version saved while there is no
    /// session is uploaded once this sidebar attaches.
    pub fn adopt_open_files(&self, old: &SftpSidebar) {
        self.inner.adopt_edits(&old.inner);
    }

    /// A sidebar that stays hidden, and opens no SFTP channel, until its
    /// first [`show`](Self::show) (`sftp_on_connect` off).
    pub fn hidden() -> Self {
        let sidebar = Self::new();
        sidebar.inner.state.borrow_mut().shown = false;
        sidebar.inner.render();
        sidebar
    }

    /// The tab connected: follow its new shell, asking the remote through
    /// `probe` until the shell reports its directory itself. Call before
    /// attaching, so the first listing can probe.
    pub fn follow_new_shell(&self, probe: ShellCwdProbe) {
        self.inner.state.borrow_mut().new_shell();
        *self.inner.probe.borrow_mut() = Some(probe);
    }

    /// The tab's shell reported its directory (OSC 7, picked by a
    /// [`DirectoryFollower`]). Before a client is attached it becomes the
    /// directory the first listing starts at.
    pub fn follow_shell(&self, path: String) {
        self.inner.shell_seen(&path, true);
    }

    /// A command ran in the terminal: ask the remote where the shell is,
    /// unless following is off or the shell reports that itself.
    pub fn probe_shell(&self) {
        self.inner.probe_shell();
    }

    /// Whether "Follow terminal directory" is ticked.
    pub fn is_following(&self) -> bool {
        self.inner.state.borrow().following
    }

    /// Tick or untick "Follow terminal directory", as the user would.
    pub fn set_following(&self, on: bool) {
        self.inner.follow_check.set_active(on);
    }

    /// The path bar's text.
    #[cfg(test)]
    pub(crate) fn path_text(&self) -> String {
        self.inner.path_entry.text().into()
    }

    /// Attach to `client` instead of spawning a task.
    #[cfg(test)]
    pub(crate) fn attach_client(&self, client: SftpClient) {
        *self.inner.attached.borrow_mut() = Some(Attached::with_client(client));
    }

    /// Press a toolbar button as a click would, and only while it is
    /// enabled, so a shortcut follows the toolbar's rules (doc 07).
    pub fn press(&self, button: SftpButton) {
        let buttons = &self.inner.buttons;
        let button = match button {
            SftpButton::Upload => &buttons.upload,
            SftpButton::Download => &buttons.download,
            SftpButton::Rename => &buttons.rename,
            SftpButton::Delete => &buttons.delete,
        };
        if button.is_sensitive() {
            button.emit_clicked();
        }
    }

    /// Whether the sidebar has been shown.
    pub fn is_shown(&self) -> bool {
        self.inner.state.borrow().shown
    }

    /// Show the sidebar: the side dock's SFTP panel has it in view. The
    /// first show attaches a deferred connection.
    pub fn show(&self) {
        // Another tab's sidebar may have changed them since.
        self.inner.apply_columns();
        if !self.inner.state.borrow_mut().show() {
            return;
        }
        self.inner.render();
        let deferred = self.inner.deferred.borrow_mut().take();
        if let Some(files) = deferred {
            self.attach_to(files);
        }
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

/// The [`RowView`] behind a listing item.
fn row_of(item: &glib::Object) -> Option<RowView> {
    let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
    let row = boxed.borrow::<RowView>().clone();
    Some(row)
}

/// The `ListItem` a factory callback was handed.
fn list_item(object: &glib::Object) -> Option<&gtk4::ListItem> {
    object.downcast_ref::<gtk4::ListItem>()
}

/// The Name column: each row's icon beside its name, which takes the
/// width the other columns leave and is cut in the middle when short of it.
fn name_column() -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, object| {
        let Some(item) = list_item(object) else {
            return;
        };
        let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        pad_cell(&cell);
        cell.append(&gtk4::Image::new());
        cell.append(
            &gtk4::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(pango::EllipsizeMode::Middle)
                .width_chars(NAME_MIN_CHARS)
                .build(),
        );
        set_data(&cell, LIST_ITEM_KEY, item.downgrade());
        item.set_child(Some(&cell));
    });
    factory.connect_bind(|_, object| {
        let Some(item) = list_item(object) else {
            return;
        };
        let (Some(cell), Some(row)) = (item.child(), item.item().and_then(|o| row_of(&o))) else {
            return;
        };
        if let Some(image) = cell.first_child().and_downcast::<gtk4::Image>() {
            image.set_icon_name(Some(row.icon));
        }
        if let Some(label) = cell.last_child().and_downcast::<gtk4::Label>() {
            label.set_text(&row.name);
            label.set_tooltip_text(Some(&row.name));
        }
    });
    gtk4::ColumnViewColumn::builder()
        .title("Name")
        .expand(true)
        .resizable(true)
        .factory(&factory)
        .build()
}

/// The narrowest the Name column gets before the listing scrolls sideways.
const NAME_MIN_CHARS: i32 = 14;

/// Space between a cell's content and its column's edges: not every theme
/// pads cells, and unpadded columns run into each other.
fn pad_cell(cell: &impl IsA<gtk4::Widget>) {
    cell.set_margin_start(6);
    cell.set_margin_end(6);
}

/// A column of `column`'s text for each row, dimmed beside the names.
/// Digits are all one width, so sizes and dates line up down the column;
/// sizes sit against the column's right edge.
fn detail_column(column: Column) -> gtk4::ColumnViewColumn {
    let (xalign, text): (f32, fn(&RowView) -> &str) = match column {
        Column::Size => (1.0, |row| &row.size),
        Column::Modified => (0.0, |row| &row.modified),
        Column::Permissions => (0.0, |row| &row.permissions),
    };
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        let Some(item) = list_item(object) else {
            return;
        };
        let attributes = pango::AttrList::new();
        attributes.insert(pango::AttrFontFeatures::new("tnum=1"));
        if column == Column::Permissions {
            attributes.insert(pango::AttrString::new_family("monospace"));
        }
        let label = gtk4::Label::builder()
            .xalign(xalign)
            .attributes(&attributes)
            .build();
        label.add_css_class("dim-label");
        pad_cell(&label);
        set_data(&label, LIST_ITEM_KEY, item.downgrade());
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, object| {
        let Some(item) = list_item(object) else {
            return;
        };
        let label = item.child().and_downcast::<gtk4::Label>();
        let row = item.item().and_then(|object| row_of(&object));
        if let (Some(label), Some(row)) = (label, row) {
            label.set_text(text(&row));
        }
    });
    gtk4::ColumnViewColumn::builder()
        .title(column.title())
        .resizable(true)
        .factory(&factory)
        .build()
}

/// Local paths of a file dialog's selection (entries without one are
/// skipped).
fn chooser_paths(model: &gio::ListModel) -> Vec<String> {
    (0..model.n_items())
        .filter_map(|index| model.item(index))
        .filter_map(|object| object.downcast::<gio::File>().ok())
        .filter_map(|file| file.path())
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// Build and wire the widgets.
fn build_sidebar(local: bool) -> Rc<SidebarInner> {
    // 1. Path bar (doc 07 §Layout): Enter navigates.
    let path_entry = gtk4::Entry::new();
    path_entry.set_text(START_PATH);
    path_entry.set_placeholder_text(Some(if local { "path" } else { "remote path" }));
    path_entry.set_hexpand(true);

    // 2. Toolbar — exactly doc 07's six buttons, their icons bundled with
    // the app (`ui::icons`).
    let buttons = Buttons {
        upload: icon_button("wayterm-upload-symbolic", "Upload"),
        download: icon_button("wayterm-download-symbolic", "Download"),
        delete: icon_button("wayterm-trash-symbolic", "Delete"),
        rename: icon_button("wayterm-rename-symbolic", "Rename"),
        refresh: icon_button("wayterm-refresh-symbolic", "Refresh"),
        mkdir: icon_button("wayterm-folder-new-symbolic", "New folder"),
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

    // 3. Listing: a ColumnView over boxed rows, one column per doc 07.
    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk4::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let column_view = gtk4::ColumnView::builder().model(&selection).build();
    let name = name_column();
    column_view.append_column(&name);
    // A right-click on any header picks the columns shown beside Name.
    let header_menu = gio::Menu::new();
    let column_actions = gio::SimpleActionGroup::new();
    let mut columns = Vec::new();
    for column in Column::ALL {
        let action = gio::SimpleAction::new_stateful(column.id(), None, &true.to_variant());
        column_actions.add_action(&action);
        header_menu.append(
            Some(column.title()),
            Some(&format!("columns.{}", column.id())),
        );
        let view_column = detail_column(column);
        column_view.append_column(&view_column);
        columns.push((column, view_column, action));
    }
    column_view.insert_action_group("columns", Some(&column_actions));
    name.set_header_menu(Some(&header_menu));
    for (_, view_column, _) in &columns {
        view_column.set_header_menu(Some(&header_menu));
    }

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .hexpand(true)
        .build();
    let row_menu = gtk4::PopoverMenu::from_model(None::<&gio::MenuModel>);
    row_menu.set_parent(&scrolled);
    row_menu.set_has_arrow(false);
    row_menu.set_halign(gtk4::Align::Start);
    // The scrolled window only unparents the children it knows about.
    scrolled.connect_destroy(glib::clone!(
        #[strong]
        row_menu,
        move |_| row_menu.unparent()
    ));

    // 4. Inline error line, hidden while there is no error.
    let error_label = gtk4::Label::new(None);
    error_label.set_xalign(0.0);
    error_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    error_label.add_css_class("dim-label");
    error_label.set_visible(false);

    // 5. Progress bar with Cancel, in the transfer status line's place
    // while a transfer runs; each shows only when it has something to say.
    let progress_bar = gtk4::ProgressBar::new();
    progress_bar.set_show_text(true);
    progress_bar.set_hexpand(true);
    progress_bar.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    let progress_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    progress_row.append(&progress_bar);
    progress_row.append(&buttons.cancel);

    let status_label = gtk4::Label::new(None);
    status_label.set_xalign(0.0);
    status_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);

    // 6. Following the shell (doc 07 §Following the shell), on by default.
    let follow_check = gtk4::CheckButton::with_label("Follow terminal directory");
    follow_check.set_active(true);
    follow_check.set_tooltip_text(Some(
        "Show the directory the terminal's shell is in, whenever it changes",
    ));
    follow_check.set_focus_on_click(false);

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    set_margins(&container, 4);
    container.append(&path_entry);
    container.append(&toolbar);
    container.append(&error_label);
    container.append(&scrolled);
    container.append(&progress_row);
    container.append(&status_label);
    container.append(&follow_check);

    let inner = Rc::new(SidebarInner {
        local,
        container,
        path_entry,
        column_view,
        columns,
        row_menu,
        model,
        selection,
        error_label,
        progress_row,
        progress_bar,
        status_label,
        follow_check,
        buttons,
        state: RefCell::new(SftpView::detached()),
        attached: RefCell::new(None),
        deferred: RefCell::new(None),
        probe: RefCell::new(None),
        probing: Cell::new(false),
        probe_again: Cell::new(false),
        edits: RefCell::default(),
        next_edit: Cell::new(0),
        edit_root: RefCell::new(edit_root()),
        #[cfg(test)]
        launched: RefCell::default(),
    });
    inner.connect_signals();
    inner.connect_drag_drop();
    inner.apply_columns();
    inner.render();
    inner
}

impl SidebarInner {
    /// Callbacks only queue work or update the model. They hold the
    /// sidebar weakly: they hang on its own widgets, so a strong hold would
    /// keep it, and its SFTP task, alive after the last handle drops.
    fn connect_signals(self: &Rc<Self>) {
        // Enter in the path bar navigates to what is typed.
        let weak = Rc::downgrade(self);
        self.path_entry.connect_activate(move |entry| {
            if let Some(inner) = weak.upgrade() {
                inner.cd(entry.text().to_string());
            }
        });

        // Double-click a directory navigates into it, `..` goes up, and a
        // file opens.
        let weak = Rc::downgrade(self);
        self.column_view.connect_activate(move |_, position| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Some(row) = inner.model.item(position).and_then(|item| row_of(&item)) {
                inner.open_row(&row, false);
            }
        });

        // A right-click selects the row under it and offers to open it.
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let weak = Rc::downgrade(self);
        gesture.connect_pressed(move |gesture, _, x, y| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Some(position) = inner.position_at(x, y) {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                inner.selection.set_selected(position);
                inner.show_row_menu(x, y);
            }
        });
        self.column_view.add_controller(gesture);

        for (column, _, action) in &self.columns {
            let (column, weak) = (*column, Rc::downgrade(self));
            action.connect_change_state(move |_, state| {
                let shown = state.and_then(|state| state.get::<bool>());
                if let (Some(shown), Some(inner)) = (shown, weak.upgrade()) {
                    choose_columns(columns().with(column, shown));
                    inner.apply_columns();
                }
            });
        }

        // Selection drives the toolbar rules (model layer, unit-tested).
        let weak = Rc::downgrade(self);
        self.selection
            .connect_selected_item_notify(move |selection| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let picked = selection.selected_item().and_then(|item| row_of(&item));
                inner.state.borrow_mut().select(picked.as_ref());
                inner.render_toolbar();
            });

        let buttons = &self.buttons;
        self.on_click(&buttons.upload, Self::choose_upload_files);
        self.on_click(&buttons.download, Self::choose_download_target);
        self.on_click(&buttons.delete, Self::delete_selected);
        self.on_click(&buttons.rename, Self::rename_selected);
        self.on_click(&buttons.refresh, Self::refresh);
        self.on_click(&buttons.mkdir, Self::mkdir);
        self.on_click(&buttons.cancel, Self::cancel_transfer);

        let weak = Rc::downgrade(self);
        self.follow_check.connect_toggled(move |check| {
            if let Some(inner) = weak.upgrade() {
                inner.set_following(check.is_active());
            }
        });
    }

    fn on_click(self: &Rc<Self>, button: &gtk4::Button, action: fn(&Rc<Self>)) {
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                action(&inner);
            }
        });
    }

    /// Uploads from drops: a `GdkFileList` target and a `text/uri-list` one.
    fn connect_drag_drop(self: &Rc<Self>) {
        let files_target =
            gtk4::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        files_target.connect_drop(move |_target, value, _x, _y| {
            let (Some(inner), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else {
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
            inner.queue_uploads(paths);
            true
        });
        self.column_view.add_controller(files_target);

        let uri_target = gtk4::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        uri_target.connect_drop(move |_target, value, _x, _y| {
            let (Some(inner), Ok(text)) = (weak.upgrade(), value.get::<String>()) else {
                return false;
            };
            let paths = parse_uri_list(&text);
            if paths.is_empty() {
                return false;
            }
            inner.queue_uploads(paths);
            true
        });
        self.column_view.add_controller(uri_target);
    }

    /// Show the chosen columns, and tick them in the header menu.
    fn apply_columns(&self) {
        let chosen = columns();
        for (column, view_column, action) in &self.columns {
            let shown = chosen.shows(*column);
            view_column.set_visible(shown);
            action.set_state(&shown.to_variant());
        }
    }

    /// The model position of the row at `(x, y)` in listing coordinates;
    /// `None` over the headers or below the rows.
    fn position_at(&self, x: f64, y: f64) -> Option<u32> {
        let mut widget = self.column_view.pick(x, y, gtk4::PickFlags::DEFAULT);
        while let Some(current) = widget {
            if let Some(item) = data::<glib::WeakRef<gtk4::ListItem>>(&current, LIST_ITEM_KEY) {
                return item
                    .upgrade()
                    .map(|item| item.position())
                    .filter(|&position| position != gtk4::INVALID_LIST_POSITION);
            }
            if current == *self.column_view.upcast_ref::<gtk4::Widget>() {
                break;
            }
            widget = current.parent();
        }
        None
    }

    /// The selected row's menu, at the pointer: Open, and for a file Open
    /// With….
    fn show_row_menu(self: &Rc<Self>, x: f64, y: f64) {
        let Some(row) = self.selection.selected_item().and_then(|i| row_of(&i)) else {
            return;
        };
        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        let item = |label: &str, name: &str, ask: bool| {
            menu.append(Some(label), Some(&format!("row.{name}")));
            let action = gio::SimpleAction::new(name, None);
            let (weak, row) = (Rc::downgrade(self), row.clone());
            action.connect_activate(move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.open_row(&row, ask);
                }
            });
            group.add_action(&action);
        };
        item("Open", "open", false);
        if !row.is_dir {
            item("Open With…", "open-with", true);
        }
        self.row_menu.set_menu_model(Some(&menu));
        self.row_menu.insert_action_group("row", Some(&group));
        let click = gtk4::graphene::Point::new(x as f32, y as f32);
        let at = self
            .row_menu
            .parent()
            .and_then(|parent| self.column_view.compute_point(&parent, &click))
            .unwrap_or(click);
        self.row_menu.set_pointing_to(Some(&gdk::Rectangle::new(
            at.x() as i32,
            at.y() as i32,
            0,
            0,
        )));
        self.row_menu.popup();
    }

    /// Open `row`: go into a directory, up from `..`, or open a file in
    /// its application (`ask` to pick one).
    fn open_row(self: &Rc<Self>, row: &RowView, ask: bool) {
        let target = {
            let state = self.state.borrow();
            if row.is_parent {
                parent_path(state.current_path())
            } else {
                Some(join_path(state.current_path(), &row.name))
            }
        };
        match target {
            Some(target) if row.is_dir => self.cd(target),
            Some(target) => self.open_file(target, ask),
            None => {}
        }
    }

    /// Render the view into the widgets.
    fn render(&self) {
        // Copy the rows out: replacing the model fires the selection
        // callback, which borrows the state mutably.
        let rows: Vec<glib::BoxedAnyObject> = self
            .state
            .borrow()
            .rows
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        self.model.splice(0, self.model.n_items(), &rows);

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

        self.follow_check.set_active(state.following);
        self.container.set_visible(state.shown);
        drop(state);
        self.render_transfers();
    }

    /// Only the bar, the status line and the toolbar: a progress sample or
    /// an expiring result leaves the listing, and its selection, alone.
    fn render_transfers(&self) {
        let state = self.state.borrow();
        let bar = state.bar();
        if let Some(bar) = &bar {
            self.progress_bar.set_fraction(bar.fraction);
            self.progress_bar.set_text(Some(bar.text.as_str()));
            self.progress_bar.set_tooltip_text(Some(bar.text.as_str()));
        }
        self.progress_row.set_visible(bar.is_some());
        // A long error is cut short in a narrow dock; hovering shows it all.
        self.status_label.set_text(&state.status);
        self.status_label
            .set_tooltip_text(Some(state.status.as_str()).filter(|s| !s.is_empty()));
        self.status_label.set_visible(!state.status.is_empty());
        drop(state);
        self.render_toolbar();
    }

    /// Separate from `render`, which must not re-enter from the selection
    /// callback that replacing the model emits.
    fn render_toolbar(&self) {
        let toolbar = self.state.borrow().toolbar();
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
        Some(self.attached.borrow().as_ref()?.client.clone())
    }

    fn set_error(&self, err: &str) {
        self.state.borrow_mut().set_error(err);
        self.render();
    }

    /// The inline error for a sidebar with no task behind it.
    fn not_connected(&self) -> &'static str {
        if self.local {
            SHELL_NOT_RUNNING
        } else {
            NOT_CONNECTED
        }
    }

    /// Send a command behind any parked ones; `false` when it cannot be
    /// delivered at all.
    fn send(self: &Rc<Self>, cmd: SftpCommand) -> bool {
        let commands = self.attached.borrow().as_ref().map(|a| a.commands.clone());
        let Some(commands) = commands else {
            self.set_error(self.not_connected());
            return false;
        };
        if !commands.send(cmd) {
            self.set_error(COMMAND_FAILED);
            return false;
        }
        true
    }

    /// The shell is in `path` (`reported` over OSC 7, or probed): go there
    /// if following says so. Before a client is attached, the first
    /// listing starts there instead.
    fn shell_seen(self: &Rc<Self>, path: &str, reported: bool) {
        let target = self.state.borrow_mut().shell_moved(path, reported);
        if let Some(target) = target {
            self.go_to_shell_dir(target);
        }
    }

    fn go_to_shell_dir(self: &Rc<Self>, path: String) {
        if self.client().is_some() {
            self.cd(path);
        } else {
            self.state.borrow_mut().start_at(&path);
            self.sync_path_entry();
        }
    }

    /// The checkbox changed (render setting it to the state is no change).
    /// Ticking it catches up with the shell and asks where it is now.
    fn set_following(self: &Rc<Self>, on: bool) {
        if self.state.borrow().following == on {
            return;
        }
        let target = self.state.borrow_mut().set_following(on);
        if let Some(target) = target {
            self.go_to_shell_dir(target);
        }
        self.probe_shell();
    }

    /// Ask the remote where the shell is, one probe at a time: a request
    /// made while one runs is served once it ends.
    fn probe_shell(self: &Rc<Self>) {
        if !self.state.borrow().wants_probe() {
            return;
        }
        let Some(probe) = self.probe.borrow().clone() else {
            return;
        };
        if self.probing.replace(true) {
            self.probe_again.set(true);
            return;
        }
        let reply = probe.spawn();
        let sidebar = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let dir = reply.await.ok().flatten();
            sidebar.probing.set(false);
            if let Some(dir) = dir {
                sidebar.shell_seen(&dir, false);
            }
            if sidebar.probe_again.replace(false) {
                sidebar.probe_shell();
            }
        });
    }

    /// Re-list the current directory.
    fn refresh(self: &Rc<Self>) {
        let path = self.state.borrow().current_path().to_string();
        self.cd(path);
    }

    /// Queue a `Cd` and apply the listing (or the error) when it answers.
    fn cd(self: &Rc<Self>, path: String) {
        if self.client().is_none() {
            self.set_error(self.not_connected());
            return;
        }
        let path = if self.local {
            let home = glib::home_dir();
            local_target(
                self.state.borrow().current_path(),
                &path,
                &home.to_string_lossy(),
            )
        } else {
            path
        };
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
                Ok(Err(err)) => sidebar.state.borrow_mut().set_error(&err),
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
            self.set_error(self.not_connected());
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
        self.run_transfer(
            format!("Upload {name}"),
            Transfer::Upload {
                local: local_path,
                remote,
            },
            Self::refresh_after,
        );
    }

    fn download(self: &Rc<Self>, remote: String, local: String) {
        let label = format!("Download {}", file_name(&remote));
        self.run_transfer(
            label,
            Transfer::Download { remote, local },
            Self::refresh_after,
        );
    }

    /// A finished transfer changed what the listing shows.
    fn refresh_after(self: &Rc<Self>, succeeded: bool) {
        if succeeded {
            self.refresh();
        }
    }

    /// Queue a transfer and mirror its progress. `on_done` runs once, with
    /// whether it succeeded, also when it could not be queued.
    fn run_transfer(
        self: &Rc<Self>,
        label: String,
        transfer: Transfer,
        on_done: impl FnOnce(&Rc<Self>, bool) + 'static,
    ) {
        let Some(client) = self.client() else {
            self.set_error(self.not_connected());
            on_done(self, false);
            return;
        };
        let transfer_id = client.next_transfer_id();
        let (progress_tx, mut progress_rx) = mpsc::channel::<Progress>(64);
        let (reply, reply_rx) = oneshot::channel();

        self.state.borrow_mut().begin_transfer(transfer_id, label);
        self.render_transfers();

        if !self.send(transfer.command(transfer_id, progress_tx, reply)) {
            // The command never reached the task — take the display back.
            self.state
                .borrow_mut()
                .end_transfer(transfer_id, &Err(COMMAND_FAILED.to_string()));
            self.render_transfers();
            on_done(self, false);
            return;
        }

        // Dropped samples are fine; drain as fast as they come.
        let pump = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            while let Some(sample) = progress_rx.recv().await {
                pump.state
                    .borrow_mut()
                    .on_progress(transfer_id, sample.transferred, sample.total);
                pump.render_transfers();
            }
        });

        // Completion: the reply carries the transferred byte count.
        let done = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let result = match reply_rx.await {
                Ok(result) => result,
                Err(_) => Err(SESSION_ENDED.to_string()),
            };
            done.state.borrow_mut().end_transfer(transfer_id, &result);
            done.render_transfers();
            if result.is_ok() {
                let weak = Rc::downgrade(&done);
                glib::timeout_add_local_once(DONE_SHOWS_FOR, move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.state.borrow_mut().expire_status(transfer_id);
                        inner.render_transfers();
                    }
                });
            }
            on_done(&done, result.is_ok());
        });
    }

    /// Open the remote file in its application, or one the user picks
    /// (`ask`), from a local copy. A file opened before is downloaded
    /// again into the copy it has, unless that holds a version the server
    /// lacks: then that version is sent, and shown. A local tab's file
    /// opens where it is.
    fn open_file(self: &Rc<Self>, remote: String, ask: bool) {
        if self.client().is_none() {
            self.set_error(self.not_connected());
            return;
        }
        if self.local {
            return self.launch(Path::new(&remote), ask);
        }
        let name = file_name(&remote);
        if !is_plain_file_name(&name) {
            self.set_error(&format!("Cannot open '{name}': not a usable file name"));
            return;
        }
        let known = self
            .edits
            .borrow_mut()
            .iter_mut()
            .find(|edit| edit.remote == remote)
            .map(|edit| {
                let reopen = edit.sync.reopen(FileStamp::of(&edit.local));
                (edit.id, edit.local.clone(), edit.staged.clone(), reopen)
            });
        let (id, staged) = match known {
            Some((_, _, _, Reopen::Busy)) => return,
            Some((id, local, _, Reopen::Unsent)) => {
                self.sync_edit(id);
                self.launch(&local, ask);
                return;
            }
            Some((id, _, staged, Reopen::Fetch)) => (id, staged),
            None => {
                let root = self.edit_root.borrow().clone();
                let dir = match root.as_deref().map(create_edit_dir) {
                    Some(Ok(dir)) => dir,
                    Some(Err(err)) => {
                        self.set_error(&format!("Cannot open '{name}': {err}"));
                        return;
                    }
                    None => {
                        self.set_error(&format!("Cannot open '{name}': no cache directory"));
                        return;
                    }
                };
                let id = self.next_edit.replace(self.next_edit.get() + 1);
                let staged = dir.with_extension("part");
                self.edits.borrow_mut().push(OpenFile {
                    id,
                    remote: remote.clone(),
                    local: dir.join(&name),
                    dir,
                    staged: staged.clone(),
                    sync: EditSync::fetching(),
                    monitor: None,
                    settle: None,
                });
                (id, staged)
            }
        };
        let transfer = Transfer::Download {
            remote,
            local: staged.to_string_lossy().into_owned(),
        };
        self.run_transfer(format!("Open {name}"), transfer, move |inner, succeeded| {
            inner.fetched(id, succeeded, ask)
        });
    }

    /// The download of an opened file ended: move it over the local
    /// copy, watch that and show it. A failed download leaves the copy as
    /// it was, and so does one the editor overtook with a save, which is
    /// sent instead. A first download that failed leaves nothing to show.
    fn fetched(self: &Rc<Self>, id: u64, succeeded: bool, ask: bool) {
        let mut edits = self.edits.borrow_mut();
        let Some(index) = edits.iter().position(|edit| edit.id == id) else {
            return;
        };
        let edit = &mut edits[index];
        let overtaken = !edit.sync.is_sent(FileStamp::of(&edit.local));
        let placed = (succeeded && !overtaken)
            .then(|| place_download(&edit.staged, &edit.local))
            .flatten();
        if placed.is_none() {
            let _ = std::fs::remove_file(&edit.staged);
        }
        let changed = edit.sync.fetch_ended(placed) || overtaken;
        let name = file_name(&edit.remote);
        let first = edit.monitor.is_none();
        let mut failure = (succeeded && !overtaken && placed.is_none())
            .then(|| format!("Cannot open '{name}': its copy could not be written"));
        if first && placed.is_some() {
            match self.watch(id, &edit.dir) {
                Ok(monitor) => edit.monitor = Some(monitor),
                // Shown unwatched, its saves would go nowhere.
                Err(err) => failure = Some(format!("Cannot watch '{name}' for changes: {err}")),
            }
        }
        let local = edit.local.clone();
        if edit.monitor.is_none() {
            edits.remove(index);
        }
        drop(edits);
        if let Some(failure) = failure {
            return self.set_error(&failure);
        }
        if changed {
            self.sync_edit(id);
        }
        if succeeded {
            self.launch(&local, ask);
        }
    }

    /// Take `old`'s opened files (see [`SftpSidebar::adopt_open_files`]).
    /// One whose first download had not ended has nothing to keep.
    fn adopt_edits(self: &Rc<Self>, old: &SidebarInner) {
        let mut edits = std::mem::take(&mut *old.edits.borrow_mut());
        edits.retain_mut(|edit| {
            if let Some(pending) = edit.settle.take() {
                pending.remove();
            }
            let Some(monitor) = edit.monitor.take() else {
                return false;
            };
            monitor.cancel();
            edit.sync.interrupted();
            let _ = std::fs::remove_file(&edit.staged);
            edit.monitor = self.watch(edit.id, &edit.dir).ok();
            edit.monitor.is_some()
        });
        self.next_edit.set(old.next_edit.get());
        *self.edit_root.borrow_mut() = old.edit_root.borrow().clone();
        *self.edits.borrow_mut() = edits;
    }

    /// Upload whatever the opened files' copies hold that the server
    /// lacks: versions saved while there was no session.
    fn send_unsent_edits(self: &Rc<Self>) {
        let ids: Vec<u64> = self.edits.borrow().iter().map(|edit| edit.id).collect();
        for id in ids {
            self.sync_edit(id);
        }
    }

    /// Watch an opened file's directory: editors save by writing the file
    /// in place, or by renaming a new one over it.
    fn watch(self: &Rc<Self>, id: u64, dir: &Path) -> Result<gio::FileMonitor, glib::Error> {
        let monitor = gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)?;
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.local_touched(id);
            }
        });
        Ok(monitor)
    }

    /// Something in an opened file's directory changed: look at the copy
    /// once the writes stop.
    fn local_touched(self: &Rc<Self>, id: u64) {
        let mut edits = self.edits.borrow_mut();
        let Some(edit) = edits.iter_mut().find(|edit| edit.id == id) else {
            return;
        };
        if let Some(pending) = edit.settle.take() {
            pending.remove();
        }
        let weak = Rc::downgrade(self);
        edit.settle = Some(glib::timeout_add_local_once(SETTLE, move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Some(edit) = inner.edits.borrow_mut().iter_mut().find(|e| e.id == id) {
                edit.settle = None;
            }
            inner.sync_edit(id);
        }));
    }

    /// Upload an opened file's local copy over the remote file, if the
    /// editor saved a version the server does not have.
    fn sync_edit(self: &Rc<Self>, id: u64) {
        let changed = {
            let mut edits = self.edits.borrow_mut();
            let Some(edit) = edits.iter_mut().find(|edit| edit.id == id) else {
                return;
            };
            // Gone in the middle of a save: the write that brings it back
            // looks again.
            let Some(stamp) = FileStamp::of(&edit.local) else {
                return;
            };
            edit.sync
                .local_changed(stamp)
                .then(|| (edit.local.clone(), edit.remote.clone()))
        };
        let Some((local, remote)) = changed else {
            return;
        };
        let label = format!("Save {}", file_name(&remote));
        let transfer = Transfer::Upload {
            local: local.to_string_lossy().into_owned(),
            remote,
        };
        self.run_transfer(label, transfer, move |inner, succeeded| {
            let again = inner
                .edits
                .borrow_mut()
                .iter_mut()
                .find(|edit| edit.id == id)
                .is_some_and(|edit| edit.sync.uploaded(succeeded));
            inner.refresh_after(succeeded);
            if again {
                inner.sync_edit(id);
            }
        });
    }

    /// Hand the local copy to the desktop: its default application, or
    /// the one the user picks when `ask`. Writable, so a sandboxed
    /// application can save it.
    #[cfg(not(test))]
    fn launch(self: &Rc<Self>, local: &Path, ask: bool) {
        let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(local)));
        launcher.set_always_ask(ask);
        launcher.set_writable(true);
        let weak = Rc::downgrade(self);
        let name = file_name(&local.to_string_lossy());
        launcher.launch(
            self.parent_window().as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let Err(err) = result else {
                    return;
                };
                // Closing the application chooser opens nothing.
                if err.matches(gtk4::DialogError::Dismissed) {
                    return;
                }
                if let Some(inner) = weak.upgrade() {
                    inner.set_error(&format!("Cannot open '{name}': {}", err.message()));
                }
            },
        );
    }

    #[cfg(test)]
    fn launch(self: &Rc<Self>, local: &Path, ask: bool) {
        self.launched.borrow_mut().push((local.to_path_buf(), ask));
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
        let dialog = gtk4::FileDialog::builder()
            .title("Upload files")
            .accept_label("Upload")
            .modal(true)
            .build();
        let sidebar = Rc::clone(self);
        // Dismissing the dialog is an error too; like any cancel, it uploads
        // nothing.
        dialog.open_multiple(Some(&parent), None::<&gio::Cancellable>, move |result| {
            if let Ok(files) = result {
                sidebar.queue_uploads(chooser_paths(&files));
            }
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
        let dialog = gtk4::FileDialog::builder()
            .title("Download")
            .accept_label("Download")
            .initial_name(name.as_str())
            .modal(true)
            .build();
        let sidebar = Rc::clone(self);
        dialog.save(Some(&parent), None::<&gio::Cancellable>, move |result| {
            let local = result
                .ok()
                .and_then(|file| file.path())
                .map(|path| path.to_string_lossy().into_owned());
            if let Some(local) = local {
                sidebar.download(remote, local);
            }
        });
    }

    /// The parent for dialogs and file choosers.
    fn parent_window(&self) -> Option<gtk4::Window> {
        window_of(&self.column_view)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::ui) mod gtk_tests;
