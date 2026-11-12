//! The SFTP sidebar — the GTK half of doc 07. It holds an [`SftpClient`],
//! sends commands, and applies the replies from `spawn_local` futures, so
//! no GTK callback ever awaits.
//!
//! This file is the sidebar's life cycle and browsing. Around it:
//!
//! - `widgets` builds what doc 07 §Layout lists;
//! - `follow` keeps the listing on the shell's directory;
//! - `transfers` runs uploads and downloads, from the toolbar and drops;
//! - `open_files` opens a file in the desktop's application from a local
//!   copy and uploads each version saved there (doc 07 §Opening files);
//! - `model`, `paths`, `edit_sync` and `settings` are display-free: the
//!   [`SftpView`] the widgets render, path and URI handling with the
//!   [`DirectoryFollower`], opened-file bookkeeping, and the settings rows.
//!
//! The path bar shows the requested path (the protocol has no `getcwd`),
//! and browsing starts at `.`, the SFTP home directory, unless the shell's
//! directory is known by then. While "Follow terminal directory" is ticked
//! the sidebar follows the shell: from its OSC 7 reports, or, for a shell
//! that sends none, by asking the remote ([`ShellCwdProbe`]) after each
//! command.
//!
//! A local-shell tab's sidebar ([`SftpSidebar::local`]) browses this
//! machine's files through the same commands (doc 07 §Local tabs): paths
//! are absolute, a file opens where it is, and nothing is probed.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tokio::sync::oneshot;

use crate::backpressure::IdleSender;
use crate::sftp::{SftpClient, SftpCommand, ShellCwdProbe};
use crate::transport::connection::SshConnection;
use crate::ui::widgets::{confirm, prompt_text, window_of};

mod edit_sync;
mod follow;
mod model;
mod open_files;
mod paths;
mod settings;
mod transfers;
mod widgets;
pub use edit_sync::*;
pub use model::*;
pub use open_files::discard_edit_copies;
pub use paths::*;
pub use settings::*;

use follow::ShellProbe;
use open_files::OpenFiles;
use widgets::Widgets;

/// Widgets plus state, shared by the callbacks and reply futures.
struct SidebarInner {
    /// A local-shell tab's: it browses this machine's files.
    local: bool,
    widgets: Widgets,
    state: RefCell<SftpView>,
    /// The live SFTP task, once [`SftpSidebar::attach`] has been called.
    attached: RefCell<Option<Attached>>,
    /// What to attach to on the first show (`sftp_on_connect` off, or a
    /// local tab).
    deferred: RefCell<Option<Files>>,
    shell_probe: ShellProbe,
    open_files: OpenFiles,
}

/// One attached SFTP task: its client, and the never-blocking sender its
/// commands go through, in order (doc 02 §Backpressure).
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
        Self::build(false, true)
    }

    /// A sidebar that stays hidden, and opens no SFTP channel, until its
    /// first [`show`](Self::show) (`sftp_on_connect` off).
    pub fn hidden() -> Self {
        Self::build(false, false)
    }

    /// A local-shell tab's sidebar, browsing this machine's files once
    /// [`attach_local_on_connect`](Self::attach_local_on_connect) has run.
    /// Not `shown`, it stays hidden, and reads no directory, until its
    /// first [`show`](Self::show).
    pub fn local(shown: bool) -> Self {
        Self::build(true, shown)
    }

    fn build(local: bool, shown: bool) -> Self {
        let mut state = SftpView::detached();
        state.shown = shown;
        let inner = Rc::new(SidebarInner {
            local,
            widgets: Widgets::new(local),
            state: RefCell::new(state),
            attached: RefCell::new(None),
            deferred: RefCell::new(None),
            shell_probe: ShellProbe::default(),
            open_files: OpenFiles::new(),
        });
        inner.connect_signals();
        inner.connect_drops();
        inner.widgets.apply_columns();
        inner.render();
        Self { inner }
    }

    pub fn is_local(&self) -> bool {
        self.inner.local
    }

    /// The root widget, hosted by the side dock's SFTP panel while its tab
    /// is current (`ui::panels::side_dock`).
    pub fn widget(&self) -> &gtk4::Widget {
        self.inner.widgets.container.upcast_ref()
    }

    /// Start an SFTP task over the shared connection and list the start
    /// path ([`START_PATH`] or the shell's directory). Re-attaching
    /// replaces (and ends) the previous task.
    pub fn attach(&self, connection: Arc<SshConnection>) {
        self.attach_to(Files::Remote(connection));
    }

    fn attach_to(&self, files: Files) {
        let inner = &self.inner;
        inner.deferred.take();
        let previous = inner.attached.replace(Some(Attached::new(files)));
        if previous.is_some() {
            // Its transfers still answer, about files this task never saw.
            inner.open_files.clear();
        }
        inner.state.borrow_mut().attach();
        inner.render();
        inner.refresh();
        inner.send_unsent_edits();
        // A reattached tmux session may be anywhere already.
        inner.probe_shell();
    }

    /// Attach the newly connected tab's sidebar now, or keep the
    /// connection for its first show (see [`attach_on_connect`]).
    pub fn attach_on_connect(&self, connection: Arc<SshConnection>) {
        if attach_on_connect(open_on_connect(), self.is_shown()) {
            self.attach(connection);
        } else {
            self.inner.deferred.replace(Some(Files::Remote(connection)));
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
            self.inner.deferred.replace(Some(Files::Local));
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
        self.inner.deferred.take();
        self.inner.open_files.clear();
        if let Some(attached) = self.inner.attached.take() {
            attached.client.shutdown();
        }
    }

    /// Show the jumped-session state instead of attaching: no SFTP task
    /// runs over a jumped connection (doc 04 §Limitations), so any deferred
    /// connection is dropped and the previous client, if any, is replaced
    /// by the limitation notice.
    pub fn show_jump_placeholder(&self) {
        self.inner.deferred.take();
        self.inner.attached.take();
        self.inner.open_files.clear();
        self.inner.state.borrow_mut().mark_jump_unsupported();
        self.inner.render();
    }

    /// Take over the files `old` has open in editors, before it detaches:
    /// the tab let go of its connection, and this sidebar replaces `old`.
    /// Their copies stay watched, and a version saved while there is no
    /// session is uploaded once this sidebar attaches.
    pub fn adopt_open_files(&self, old: &SftpSidebar) {
        self.inner.adopt_open_files(&old.inner);
    }

    /// The tab connected: follow its new shell, asking the remote through
    /// `probe` until the shell reports its directory itself. Call before
    /// attaching, so the first listing can probe.
    pub fn follow_new_shell(&self, probe: ShellCwdProbe) {
        self.inner.state.borrow_mut().new_shell();
        self.inner.shell_probe.probe.replace(Some(probe));
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
        self.inner.widgets.follow_check.set_active(on);
    }

    /// The path bar's text.
    #[cfg(test)]
    pub(crate) fn path_text(&self) -> String {
        self.inner.widgets.path_entry.text().into()
    }

    /// Attach to `client` instead of spawning a task.
    #[cfg(test)]
    pub(crate) fn attach_client(&self, client: SftpClient) {
        let attached = Attached::with_client(client);
        self.inner.attached.replace(Some(attached));
    }

    /// Press a toolbar button as a click would, and only while it is
    /// enabled, so a shortcut follows the toolbar's rules (doc 07).
    pub fn press(&self, button: SftpButton) {
        let buttons = &self.inner.widgets.buttons;
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
        self.inner.widgets.apply_columns();
        if !self.inner.state.borrow_mut().show() {
            return;
        }
        self.inner.render();
        if let Some(files) = self.inner.deferred.take() {
            self.attach_to(files);
        }
    }
}

impl Default for SftpSidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl SidebarInner {
    /// `action` for a signal handler or a timer, run only while the
    /// sidebar lives. Handlers hang on the sidebar's own widgets, so a
    /// strong hold would keep it, and its SFTP task, alive after the last
    /// handle drops.
    fn weakly<Arg, Out>(
        self: &Rc<Self>,
        action: impl Fn(&Rc<Self>, Arg) -> Out + 'static,
    ) -> impl Fn(Arg) -> Option<Out> + 'static {
        let weak = Rc::downgrade(self);
        move |arg| weak.upgrade().map(|inner| action(&inner, arg))
    }

    /// Callbacks only queue work or update the model.
    fn connect_signals(self: &Rc<Self>) {
        let widgets = &self.widgets;

        // Enter in the path bar navigates to what is typed.
        let go = self.weakly(|inner, path: String| inner.cd(path));
        widgets
            .path_entry
            .connect_activate(move |entry| _ = go(entry.text().into()));

        // Double-click a directory navigates into it, `..` goes up, and a
        // file opens.
        let open = self.weakly(|inner, position: u32| {
            if let Some(row) = inner.widgets.row_at(position) {
                inner.open_row(&row, false);
            }
        });
        widgets
            .column_view
            .connect_activate(move |_, position| _ = open(position));

        // A right-click selects the row under it and offers to open it.
        let menu = self.weakly(|inner, (x, y): (f64, f64)| {
            let position = inner.widgets.position_at(x, y)?;
            inner.widgets.selection.set_selected(position);
            inner.show_row_menu(x, y);
            Some(())
        });
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        gesture.connect_pressed(move |gesture, _, x, y| {
            if menu((x, y)).flatten().is_some() {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            }
        });
        widgets.column_view.add_controller(gesture);

        for (column, _, action) in &widgets.columns {
            let column = *column;
            let choose = self.weakly(move |inner, shown: bool| {
                choose_columns(columns().with(column, shown));
                inner.widgets.apply_columns();
            });
            action.connect_change_state(move |_, state| {
                if let Some(shown) = state.and_then(|state| state.get::<bool>()) {
                    choose(shown);
                }
            });
        }

        // Selection drives the toolbar rules (model layer, unit-tested).
        let select = self.weakly(|inner, ()| {
            let picked = inner.widgets.selected_row();
            inner.state.borrow_mut().select(picked.as_ref());
            inner.render_toolbar();
        });
        widgets
            .selection
            .connect_selected_item_notify(move |_| _ = select(()));

        let buttons = &widgets.buttons;
        self.on_click(&buttons.upload, Self::choose_upload_files);
        self.on_click(&buttons.download, Self::choose_download_target);
        self.on_click(&buttons.delete, Self::delete_selected);
        self.on_click(&buttons.rename, Self::rename_selected);
        self.on_click(&buttons.refresh, Self::refresh);
        self.on_click(&buttons.mkdir, Self::mkdir);
        self.on_click(&buttons.cancel, Self::cancel_transfer);

        let follow = self.weakly(|inner, on: bool| inner.set_following(on));
        widgets
            .follow_check
            .connect_toggled(move |check| _ = follow(check.is_active()));
    }

    fn on_click(self: &Rc<Self>, button: &gtk4::Button, action: fn(&Rc<Self>)) {
        let click = self.weakly(move |inner, ()| action(inner));
        button.connect_clicked(move |_| _ = click(()));
    }

    /// The selected row's menu, at the pointer: Open, and for a file Open
    /// With….
    fn show_row_menu(self: &Rc<Self>, x: f64, y: f64) {
        let Some(row) = self.widgets.selected_row() else {
            return;
        };
        let menu = gio::Menu::new();
        let actions = gio::SimpleActionGroup::new();
        let open_with = (!row.is_dir).then_some(("Open With…", "open-with", true));
        for (label, name, ask) in [("Open", "open", false)].into_iter().chain(open_with) {
            menu.append(Some(label), Some(&format!("row.{name}")));
            let row = row.clone();
            let open = self.weakly(move |inner, ()| inner.open_row(&row, ask));
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| _ = open(()));
            actions.add_action(&action);
        }
        self.widgets.popup_row_menu(&menu, &actions, x, y);
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
        let widgets = &self.widgets;
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
        widgets.rows.splice(0, widgets.rows.n_items(), &rows);

        let state = self.state.borrow();
        let error = state.error.as_deref();
        widgets.error_label.set_text(error.unwrap_or_default());
        widgets.error_label.set_visible(error.is_some());
        widgets.follow_check.set_active(state.following);
        widgets.container.set_visible(state.shown);
        drop(state);
        self.render_transfers();
    }

    /// Only the bar, the status line and the toolbar: a progress sample or
    /// an expiring result leaves the listing, and its selection, alone.
    fn render_transfers(&self) {
        let widgets = &self.widgets;
        let state = self.state.borrow();
        let bar = state.bar();
        if let Some(bar) = &bar {
            widgets.progress_bar.set_fraction(bar.fraction);
            widgets.progress_bar.set_text(Some(bar.text.as_str()));
            widgets
                .progress_bar
                .set_tooltip_text(Some(bar.text.as_str()));
        }
        widgets.progress_row.set_visible(bar.is_some());
        // A long error is cut short in a narrow dock; hovering shows it all.
        let status = Some(state.status.as_str()).filter(|status| !status.is_empty());
        widgets.status_label.set_text(status.unwrap_or_default());
        widgets.status_label.set_tooltip_text(status);
        widgets.status_label.set_visible(status.is_some());
        drop(state);
        self.render_toolbar();
    }

    /// Separate from `render`, which must not re-enter from the selection
    /// callback that replacing the model emits.
    fn render_toolbar(&self) {
        let toolbar = self.state.borrow().toolbar();
        let buttons = &self.widgets.buttons;
        buttons.upload.set_sensitive(toolbar.upload);
        buttons.download.set_sensitive(toolbar.download);
        buttons.delete.set_sensitive(toolbar.delete);
        buttons.rename.set_sensitive(toolbar.rename);
        buttons.refresh.set_sensitive(toolbar.refresh);
        buttons.mkdir.set_sensitive(toolbar.mkdir);
        buttons.cancel.set_sensitive(toolbar.cancel);
    }

    /// Reset the path bar to the directory actually shown.
    fn sync_path_entry(&self) {
        let text = self.state.borrow().path_text.clone();
        self.widgets.path_entry.set_text(&text);
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

    /// Send a command behind any parked ones; `false`, with the inline
    /// error saying why, when it cannot be delivered at all.
    fn send(&self, cmd: SftpCommand) -> bool {
        let commands = self.attached.borrow().as_ref().map(|a| a.commands.clone());
        let failure = match commands {
            None => self.not_connected(),
            Some(commands) if !commands.send(cmd) => COMMAND_FAILED,
            Some(_) => return true,
        };
        self.set_error(failure);
        false
    }

    /// Send the command `make` builds around a reply channel, and hand
    /// its answer to `done` on the main loop. A task that ended without
    /// answering reads as [`SESSION_ENDED`]. A command that could not be
    /// sent gives `done` back uncalled.
    fn request<T: 'static, Done: FnOnce(&Rc<Self>, Result<T, String>) + 'static>(
        self: &Rc<Self>,
        make: impl FnOnce(oneshot::Sender<Result<T, String>>) -> SftpCommand,
        done: Done,
    ) -> Option<Done> {
        let (reply, answer) = oneshot::channel();
        if !self.send(make(reply)) {
            return Some(done);
        }
        let sidebar = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let ended = |_| Err(SESSION_ENDED.to_string());
            done(&sidebar, answer.await.unwrap_or_else(ended));
        });
        None
    }

    /// Re-list the current directory.
    fn refresh(self: &Rc<Self>) {
        let path = self.state.borrow().current_path().to_string();
        self.cd(path);
    }

    /// Queue a `Cd` and apply the listing (or the error) when it answers.
    fn cd(self: &Rc<Self>, path: String) {
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
        let listed = path.clone();
        self.request(
            |reply| SftpCommand::Cd { path, reply },
            move |sidebar, listing| {
                let mut state = sidebar.state.borrow_mut();
                match listing {
                    Ok(entries) => state.set_listing(&listed, &entries),
                    Err(err) => state.set_error(&err),
                }
                drop(state);
                sidebar.sync_path_entry();
                sidebar.render();
            },
        );
    }

    /// A file operation answering `()`; refresh on success.
    fn run_simple(
        self: &Rc<Self>,
        make: impl FnOnce(oneshot::Sender<Result<(), String>>) -> SftpCommand,
    ) {
        self.request(make, |sidebar, result| match result {
            Ok(()) => {
                sidebar.state.borrow_mut().clear_error();
                sidebar.render();
                sidebar.refresh();
            }
            Err(err) => sidebar.set_error(&err),
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
        let Some((from, name)) = self.selected_target() else {
            return;
        };
        let Some(new_name) = self.ask_name("Rename", &name).filter(|new| *new != name) else {
            return;
        };
        let to = join_path(self.state.borrow().current_path(), &new_name);
        self.run_simple(|reply| SftpCommand::Rename { from, to, reply });
    }

    fn mkdir(self: &Rc<Self>) {
        let Some(name) = self.ask_name("New Folder", "") else {
            return;
        };
        let path = join_path(self.state.borrow().current_path(), &name);
        self.run_simple(|reply| SftpCommand::Mkdir { path, reply });
    }

    /// The name the user types into a prompt, trimmed; `None` for none.
    fn ask_name(&self, title: &str, initial: &str) -> Option<String> {
        let typed = prompt_text(self.parent_window().as_ref(), title, initial)?;
        Some(typed.trim().to_string()).filter(|name| !name.is_empty())
    }

    /// The selected row's path and name (never `..`).
    fn selected_target(&self) -> Option<(String, String)> {
        let state = self.state.borrow();
        let path = state.selected_path()?;
        let name = file_name(&path);
        (!name.is_empty() && name != PARENT_ROW).then_some((path, name))
    }

    /// The parent for dialogs and file choosers.
    fn parent_window(&self) -> Option<gtk4::Window> {
        window_of(&self.widgets.column_view)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::ui) mod gtk_tests;
