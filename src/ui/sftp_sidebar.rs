//! The SFTP sidebar — the GTK half of doc 09. It holds an [`SftpClient`],
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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tokio::sync::{mpsc, oneshot};

use crate::backpressure::IdleSender;
use crate::sftp::{Progress, SftpClient, SftpCommand, ShellCwdProbe};
use crate::store::model::SessionType;
use crate::transport::connection::SshConnection;
use crate::ui::widgets::{confirm, prompt_text, set_margins, window_of};

mod model;
pub use model::*;

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
    column_view: gtk4::ColumnView,
    /// The listing's rows, each a boxed [`RowView`].
    model: gio::ListStore,
    selection: gtk4::SingleSelection,
    error_label: gtk4::Label,
    progress_bar: gtk4::ProgressBar,
    status_label: gtk4::Label,
    follow_check: gtk4::CheckButton,
    buttons: Buttons,
    state: RefCell<SftpView>,
    /// The live SFTP task, once [`SftpSidebar::attach`] has been called.
    attached: RefCell<Option<Attached>>,
    /// A connection waiting for the first show (`sftp_on_connect` off).
    deferred: RefCell<Option<Arc<SshConnection>>>,
    /// Asks the remote where the current shell is.
    probe: RefCell<Option<ShellCwdProbe>>,
    /// A probe is running; another request waits for it (`probe_again`).
    probing: Cell<bool>,
    probe_again: Cell<bool>,
}

/// One attached SFTP task: its client, and the never-blocking sender its
/// commands go through, in order (doc 02 §Backpressure).
#[derive(Clone)]
struct Attached {
    client: SftpClient,
    commands: IdleSender<SftpCommand>,
}

impl Attached {
    fn new(connection: Arc<SshConnection>) -> Self {
        let client = SftpClient::new(connection);
        let commands = IdleSender::new(client.sender());
        Self { client, commands }
    }
}

/// A toolbar button the `sftp_*` actions press (doc 07 §Handlers).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SftpButton {
    Upload,
    Download,
    Rename,
    Delete,
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

    /// Start an SFTP task over the shared connection and list the start
    /// path ([`START_PATH`] or the shell's directory). Re-attaching
    /// replaces (and ends) the previous task.
    pub fn attach(&self, connection: Arc<SshConnection>) {
        self.inner.deferred.borrow_mut().take();
        *self.inner.attached.borrow_mut() = Some(Attached::new(connection));
        self.inner.state.borrow_mut().attach();
        self.inner.render();
        let start = self.inner.state.borrow().current_path().to_string();
        self.inner.cd(start);
        // A reattached tmux session may be anywhere already.
        self.inner.probe_shell();
    }

    /// The root widget, hosted by the side dock's SFTP panel while its tab
    /// is current (`ui::side_dock`).
    pub fn widget(&self) -> &gtk4::Widget {
        self.inner.container.upcast_ref()
    }

    /// Attach the newly connected tab's sidebar now, or keep the
    /// connection for its first show (see [`attach_on_connect`]).
    pub fn attach_on_connect(&self, connection: Arc<SshConnection>) {
        if attach_on_connect(open_on_connect(), self.is_shown()) {
            self.attach(connection);
        } else {
            *self.inner.deferred.borrow_mut() = Some(connection);
        }
    }

    /// Show the jumped-session state instead of attaching: no SFTP task
    /// runs over a jumped connection (doc 11 v1), so any deferred
    /// connection is dropped and the previous client, if any, is replaced
    /// by the limitation notice.
    pub fn show_jump_placeholder(&self) {
        self.inner.deferred.borrow_mut().take();
        *self.inner.attached.borrow_mut() = None;
        self.inner.state.borrow_mut().mark_jump_unsupported();
        self.inner.render();
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

    /// Press a toolbar button as a click would, and only while it is
    /// enabled, so a shortcut follows the toolbar's rules (doc 09).
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
        if !self.inner.state.borrow_mut().show() {
            return;
        }
        self.inner.render();
        let deferred = self.inner.deferred.borrow_mut().take();
        if let Some(connection) = deferred {
            self.attach(connection);
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

/// Append a column of labels showing `text` of each row.
fn add_column(
    column_view: &gtk4::ColumnView,
    title: &str,
    expand: bool,
    text: fn(&RowView) -> &str,
) {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk4::ListItem>() {
            item.set_child(Some(&gtk4::Label::builder().xalign(0.0).build()));
        }
    });
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let label = item.child().and_downcast::<gtk4::Label>();
        let row = item.item().and_then(|object| row_of(&object));
        if let (Some(label), Some(row)) = (label, row) {
            label.set_text(text(&row));
        }
    });
    let column = gtk4::ColumnViewColumn::builder()
        .title(title)
        .expand(expand)
        .factory(&factory)
        .build();
    column_view.append_column(&column);
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

    // 3. Listing: a ColumnView over boxed rows, one column per doc 09.
    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk4::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let column_view = gtk4::ColumnView::builder().model(&selection).build();
    add_column(&column_view, "Name", true, |row| &row.name);
    add_column(&column_view, "Size", false, |row| &row.size);
    add_column(&column_view, "Modified", false, |row| &row.modified);
    add_column(&column_view, "Perms", false, |row| &row.permissions);
    add_column(&column_view, "", false, |row| row.icon);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&column_view)
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

    // 6. Following the shell (doc 09 §Following the shell), on by default.
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
        container,
        path_entry,
        column_view,
        model,
        selection,
        error_label,
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
        self.column_view.connect_activate(move |_, position| {
            let row = for_row.model.item(position).and_then(|item| row_of(&item));
            let target = {
                let state = for_row.state.borrow();
                match row {
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
        self.selection
            .connect_selected_item_notify(move |selection| {
                let picked = selection.selected_item().and_then(|item| row_of(&item));
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

        let for_follow = Rc::clone(self);
        self.follow_check
            .connect_toggled(move |check| for_follow.set_following(check.is_active()));
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
        self.column_view.add_controller(files_target);

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
        self.column_view.add_controller(uri_target);
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

        self.progress_bar.set_fraction(state.fraction);
        self.progress_bar
            .set_text(Some(state.progress_text.as_str()));
        self.status_label.set_text(&state.status);
        self.follow_check.set_active(state.following);
        self.container.set_visible(state.shown);
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

    /// Send a command behind any parked ones; `false` when it cannot be
    /// delivered at all.
    fn send(self: &Rc<Self>, cmd: SftpCommand) -> bool {
        let commands = self.attached.borrow().as_ref().map(|a| a.commands.clone());
        let Some(commands) = commands else {
            self.set_error(NOT_CONNECTED);
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

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;
    use crate::sftp::DirEntry;

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            size: 2048,
            modified: "2026-10-11 12:00".to_string(),
            permissions: "rw-r--r--".to_string(),
            is_dir,
        }
    }

    /// The listing shows every row under doc 09's columns; selection feeds
    /// the toolbar rules, and a re-render (which clears the selection)
    /// does not re-enter the state borrow.
    pub(in crate::ui) fn listing_renders_and_selection_drives_toolbar() {
        let sidebar = SftpSidebar::new();
        let inner = &sidebar.inner;
        let titles: Vec<String> = (0..inner.column_view.columns().n_items())
            .filter_map(|index| inner.column_view.columns().item(index))
            .filter_map(|column| column.downcast::<gtk4::ColumnViewColumn>().ok())
            .filter_map(|column| column.title().map(|title| title.to_string()))
            .collect();
        assert_eq!(titles, ["Name", "Size", "Modified", "Perms", ""]);

        inner.state.borrow_mut().attach();
        inner
            .state
            .borrow_mut()
            .set_listing("/home/u", &[entry("docs", true), entry("a.txt", false)]);
        inner.render();
        assert_eq!(inner.model.n_items(), 3, "`..`, docs, a.txt");

        inner.selection.set_selected(2);
        assert_eq!(
            inner.state.borrow().selected_path().as_deref(),
            Some("/home/u/a.txt")
        );
        assert!(inner.buttons.download.is_sensitive());

        inner.selection.set_selected(0);
        assert_eq!(inner.state.borrow().selected_path(), None);
        assert!(!inner.buttons.download.is_sensitive());

        inner.selection.set_selected(2);
        inner.render();
        assert_eq!(inner.state.borrow().selected_path(), None);
        assert!(!inner.buttons.download.is_sensitive());
    }

    /// "Follow terminal directory" is the bottom row, ticked. Unticked, the
    /// shell's moves leave the sidebar where it is; ticking it again
    /// catches up with the shell.
    pub(in crate::ui) fn follow_checkbox_toggles_following() {
        let sidebar = SftpSidebar::new();
        let inner = &sidebar.inner;
        assert_eq!(
            inner.container.last_child().as_ref(),
            Some(inner.follow_check.upcast_ref::<gtk4::Widget>())
        );
        assert!(inner.follow_check.is_active());
        assert!(sidebar.is_following());

        sidebar.follow_shell("/srv".into());
        assert_eq!(sidebar.path_text(), "/srv");

        inner.follow_check.set_active(false);
        assert!(!sidebar.is_following());
        sidebar.follow_shell("/var/log".into());
        assert_eq!(sidebar.path_text(), "/srv", "unticked: stays");

        sidebar.set_following(true);
        assert!(inner.follow_check.is_active());
        assert_eq!(sidebar.path_text(), "/var/log", "ticked: catches up");
    }
}
