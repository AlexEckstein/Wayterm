//! The SFTP sidebar — the GTK half of doc 09. It holds an [`SftpClient`],
//! sends commands, and applies the replies from `spawn_local` futures, so
//! no GTK callback ever awaits.
//!
//! [`SftpView`] and the helpers in `model` are display-free (the listing,
//! toolbar rules, `text/uri-list` parsing); [`SftpSidebar`] only renders.
//! Drops are accepted as `GdkFileList` and as `text/uri-list` through two
//! drop targets, because gtk4-rs does not bind the multi-type API. The
//! path bar shows the requested path (the protocol has no `getcwd`), and
//! browsing starts at `.`, the SFTP home directory.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use crate::backpressure::PendingQueue;
use crate::sftp::{Progress, SftpClient, SftpCommand};
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

    /// Show the jumped-session state instead of attaching: no SFTP task
    /// runs over a jumped connection (doc 11 v1), so any deferred
    /// connection is dropped and the previous client, if any, is replaced
    /// by the limitation notice.
    pub fn show_jump_placeholder(&self) {
        self.inner.deferred.borrow_mut().take();
        *self.inner.client.borrow_mut() = None;
        self.inner.state.borrow_mut().mark_jump_unsupported();
        self.inner.render();
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
        column_view,
        model,
        selection,
        error_label,
        progress_bar,
        status_label,
        buttons,
        state: RefCell::new(SftpView::detached()),
        client: RefCell::new(None),
        deferred: RefCell::new(None),
        pending: RefCell::default(),
        flush_source: RefCell::new(None),
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

    /// Flip the sidebar's visibility and render it.
    fn toggle_visible(self: &Rc<Self>) -> bool {
        let visible = self.state.borrow_mut().toggle_visible();
        self.render();
        visible
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
        self.container.set_visible(state.visible);
        drop(state);
        self.render_toolbar();
    }

    /// Separate from `render`, which must not re-enter from the selection
    /// callback that replacing the model emits.
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
}
