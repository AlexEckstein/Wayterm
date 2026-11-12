//! Uploads and downloads (doc 07 §Transfers): queueing one, mirroring its
//! progress and result, and the file choosers and drops that start them.

use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tokio::sync::{mpsc, oneshot};

use super::{file_name, join_path, parse_uri_list, SidebarInner, COMMAND_FAILED};
use crate::sftp::{Progress, SftpCommand};

/// How long a finished transfer's line stays; a failure's stays until the
/// next transfer.
const DONE_SHOWS_FOR: Duration = Duration::from_secs(5);

/// A file transfer to queue.
pub(super) enum Transfer {
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

/// The local paths of `files`; one without a path is skipped.
fn local_paths(files: impl IntoIterator<Item = gio::File>) -> Vec<String> {
    let paths = files.into_iter().filter_map(|file| file.path());
    paths
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

impl SidebarInner {
    /// Uploads from drops, as `GdkFileList` and as `text/uri-list`:
    /// gtk4-rs does not bind the one-target multi-type API.
    pub(super) fn connect_drops(self: &Rc<Self>) {
        self.accept_drops(gdk::FileList::static_type(), |value| {
            let list = value.get::<gdk::FileList>();
            list.map(|list| local_paths(list.files()))
                .unwrap_or_default()
        });
        self.accept_drops(glib::Type::STRING, |value| {
            let text = value.get::<String>();
            text.map(|text| parse_uri_list(&text)).unwrap_or_default()
        });
    }

    fn accept_drops(self: &Rc<Self>, kind: glib::Type, paths: fn(&glib::Value) -> Vec<String>) {
        let target = gtk4::DropTarget::new(kind, gdk::DragAction::COPY);
        let upload = self.weakly(|inner, paths: Vec<String>| inner.queue_uploads(paths));
        target.connect_drop(move |_, value, _, _| {
            let paths = paths(value);
            !paths.is_empty() && upload(paths).is_some()
        });
        self.widgets.column_view.add_controller(target);
    }

    /// Upload local files into the current directory (toolbar or drag-drop).
    pub(super) fn queue_uploads(self: &Rc<Self>, local_paths: Vec<String>) {
        if self.client().is_none() {
            return self.set_error(self.not_connected());
        }
        for local in local_paths {
            let name = file_name(&local);
            if name.is_empty() {
                continue;
            }
            let remote = join_path(self.state.borrow().current_path(), &name);
            self.run_transfer(
                format!("Upload {name}"),
                Transfer::Upload { local, remote },
                Self::refresh_after,
            );
        }
    }

    pub(super) fn download(self: &Rc<Self>, remote: String, local: String) {
        let label = format!("Download {}", file_name(&remote));
        self.run_transfer(
            label,
            Transfer::Download { remote, local },
            Self::refresh_after,
        );
    }

    /// A finished transfer changed what the listing shows.
    pub(super) fn refresh_after(self: &Rc<Self>, succeeded: bool) {
        if succeeded {
            self.refresh();
        }
    }

    /// Queue a transfer and mirror its progress. `on_done` runs once, with
    /// whether it succeeded, also when it could not be queued.
    pub(super) fn run_transfer(
        self: &Rc<Self>,
        label: String,
        transfer: Transfer,
        on_done: impl FnOnce(&Rc<Self>, bool) + 'static,
    ) {
        let Some(client) = self.client() else {
            self.set_error(self.not_connected());
            return on_done(self, false);
        };
        let id = client.next_transfer_id();
        let (progress, mut samples) = mpsc::channel::<Progress>(64);
        self.state.borrow_mut().begin_transfer(id, label);
        self.render_transfers();

        let unsent = self.request(
            |reply| transfer.command(id, progress, reply),
            move |inner, result: Result<u64, String>| {
                inner.state.borrow_mut().end_transfer(id, &result);
                inner.render_transfers();
                if result.is_ok() {
                    let expire = inner.weakly(move |inner, ()| {
                        inner.state.borrow_mut().expire_status(id);
                        inner.render_transfers();
                    });
                    glib::timeout_add_local_once(DONE_SHOWS_FOR, move || _ = expire(()));
                }
                on_done(inner, result.is_ok());
            },
        );
        if let Some(ended) = unsent {
            // The command never reached the task: take the display back.
            return ended(self, Err(COMMAND_FAILED.to_string()));
        }

        // Dropped samples are fine; drain as fast as they come.
        let sidebar = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            while let Some(sample) = samples.recv().await {
                let mut state = sidebar.state.borrow_mut();
                state.on_progress(id, sample.transferred, sample.total);
                drop(state);
                sidebar.render_transfers();
            }
        });
    }

    pub(super) fn cancel_transfer(self: &Rc<Self>) {
        let target = self.state.borrow().cancel_target();
        if let Some(transfer_id) = target {
            self.send(SftpCommand::Cancel { transfer_id });
        }
    }

    /// Ask for local files and upload them into the current directory.
    pub(super) fn choose_upload_files(self: &Rc<Self>) {
        let Some(parent) = self.parent_window() else {
            return self.set_error("no window for the file chooser");
        };
        let dialog = gtk4::FileDialog::builder()
            .title("Upload files")
            .accept_label("Upload")
            .modal(true)
            .build();
        let sidebar = Rc::clone(self);
        // Dismissing the dialog is an error too; like any cancel, it uploads
        // nothing.
        dialog.open_multiple(Some(&parent), gio::Cancellable::NONE, move |result| {
            if let Ok(files) = result {
                let files = (0..files.n_items()).filter_map(|index| files.item(index));
                let files = files.filter_map(|object| object.downcast::<gio::File>().ok());
                sidebar.queue_uploads(local_paths(files));
            }
        });
    }

    /// Ask where to save the selected row, then download it.
    pub(super) fn choose_download_target(self: &Rc<Self>) {
        let Some((remote, name)) = self.selected_target() else {
            return;
        };
        let Some(parent) = self.parent_window() else {
            return self.set_error("no window for the file chooser");
        };
        let dialog = gtk4::FileDialog::builder()
            .title("Download")
            .accept_label("Download")
            .initial_name(name.as_str())
            .modal(true)
            .build();
        let sidebar = Rc::clone(self);
        dialog.save(Some(&parent), gio::Cancellable::NONE, move |result| {
            if let Some(local) = local_paths(result.ok()).pop() {
                sidebar.download(remote, local);
            }
        });
    }
}
