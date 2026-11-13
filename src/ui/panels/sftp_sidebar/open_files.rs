//! Files opened in an editor (doc 07 §Opening files): each is downloaded
//! to a local copy the desktop opens, and every version the editor saves
//! there is uploaded over the remote file. [`EditSync`] decides what to
//! send; this is the copies, their watches and the transfers.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gio, glib};

use super::transfers::Transfer;
use super::{
    EditSync, FileStamp, Reopen, SidebarInner, create_edit_dir, edit_root, file_name,
    is_plain_file_name, place_download,
};

/// How long a local copy must stay untouched before it is looked at: an
/// editor's save is several writes.
pub(super) const SETTLE: Duration = Duration::from_millis(300);

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

impl OpenFile {
    fn stop_watching(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            monitor.cancel();
        }
        if let Some(settle) = self.settle.take() {
            settle.remove();
        }
        let _ = std::fs::remove_file(&self.staged);
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        self.stop_watching();
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

/// The files one sidebar has open in editors.
pub(super) struct OpenFiles {
    files: RefCell<Vec<OpenFile>>,
    next_id: Cell<u64>,
    /// Where their local copies go, if anywhere ([`edit_root`]).
    pub(super) root: RefCell<Option<PathBuf>>,
    /// What [`launch`](SidebarInner::launch) was asked to open, instead of
    /// opening it.
    #[cfg(test)]
    pub(super) launched: RefCell<Vec<(PathBuf, bool)>>,
}

impl OpenFiles {
    pub(super) fn new() -> Self {
        Self {
            files: RefCell::default(),
            next_id: Cell::new(0),
            root: RefCell::new(edit_root()),
            #[cfg(test)]
            launched: RefCell::default(),
        }
    }

    /// Close them all: their local copies go.
    pub(super) fn clear(&self) {
        self.files.borrow_mut().clear();
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.files.borrow().is_empty()
    }

    fn with<R>(&self, id: u64, f: impl FnOnce(&mut OpenFile) -> R) -> Option<R> {
        let mut files = self.files.borrow_mut();
        files.iter_mut().find(|file| file.id == id).map(f)
    }

    /// A file opened for the first time, its download about to start: it
    /// gets a directory for its copy. `Err` says why it cannot have one.
    fn add(&self, remote: &str) -> Result<u64, String> {
        let name = file_name(remote);
        if !is_plain_file_name(&name) {
            return Err("not a usable file name".into());
        }
        let root = self.root.borrow().clone().ok_or("no cache directory")?;
        let dir = create_edit_dir(&root).map_err(|err| err.to_string())?;
        let id = self.next_id.replace(self.next_id.get() + 1);
        self.files.borrow_mut().push(OpenFile {
            id,
            remote: remote.to_string(),
            local: dir.join(&name),
            staged: dir.with_extension("part"),
            dir,
            sync: EditSync::fetching(),
            monitor: None,
            settle: None,
        });
        Ok(id)
    }
}

impl SidebarInner {
    /// Open the remote file in its application, or one the user picks
    /// (`ask`), from a local copy. A file opened before is downloaded
    /// again into the copy it has, unless that holds a version the server
    /// lacks: then that version is sent, and shown. A local tab's file
    /// opens where it is.
    pub(super) fn open_file(self: &Rc<Self>, remote: String, ask: bool) {
        if self.client().is_none() {
            return self.set_error(self.not_connected());
        }
        if self.local {
            return self.launch(Path::new(&remote), ask);
        }
        let name = file_name(&remote);
        let known = {
            let mut files = self.open_files.files.borrow_mut();
            let file = files.iter_mut().find(|file| file.remote == remote);
            file.map(|file| {
                let reopen = file.sync.reopen(FileStamp::of(&file.local));
                (file.id, reopen, file.local.clone())
            })
        };
        let id = match known {
            Some((_, Reopen::Busy, _)) => return,
            Some((id, Reopen::Unsent, local)) => {
                self.sync_edit(id);
                return self.launch(&local, ask);
            }
            Some((id, Reopen::Fetch, _)) => id,
            None => match self.open_files.add(&remote) {
                Ok(id) => id,
                Err(why) => return self.set_error(&format!("Cannot open '{name}': {why}")),
            },
        };
        let staged = self.open_files.with(id, |file| file.staged.clone());
        let Some(staged) = staged else {
            return;
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
        let mut files = self.open_files.files.borrow_mut();
        let Some(index) = files.iter().position(|file| file.id == id) else {
            return;
        };
        let file = &mut files[index];
        let overtaken = !file.sync.is_sent(FileStamp::of(&file.local));
        let placed = (succeeded && !overtaken)
            .then(|| place_download(&file.staged, &file.local))
            .flatten();
        if placed.is_none() {
            let _ = std::fs::remove_file(&file.staged);
        }
        let changed = file.sync.fetch_ended(placed) || overtaken;
        let name = file_name(&file.remote);
        let mut failure = (succeeded && !overtaken && placed.is_none())
            .then(|| format!("Cannot open '{name}': its copy could not be written"));
        if file.monitor.is_none() && placed.is_some() {
            match self.watch(id, &file.dir) {
                Ok(monitor) => file.monitor = Some(monitor),
                // Shown unwatched, its saves would go nowhere.
                Err(err) => failure = Some(format!("Cannot watch '{name}' for changes: {err}")),
            }
        }
        let local = file.local.clone();
        if file.monitor.is_none() {
            files.remove(index);
        }
        drop(files);
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

    /// Take over the files `old` has open (see
    /// [`SftpSidebar::adopt_open_files`](super::SftpSidebar::adopt_open_files)).
    /// One whose first download had not ended has nothing to keep.
    pub(super) fn adopt_open_files(self: &Rc<Self>, old: &SidebarInner) {
        let mut files = old.open_files.files.take();
        files.retain_mut(|file| {
            let watched = file.monitor.is_some();
            file.stop_watching();
            file.sync.interrupted();
            if watched {
                file.monitor = self.watch(file.id, &file.dir).ok();
            }
            file.monitor.is_some()
        });
        let (mine, theirs) = (&self.open_files, &old.open_files);
        mine.next_id.set(theirs.next_id.get());
        mine.root.replace(theirs.root.borrow().clone());
        mine.files.replace(files);
    }

    /// Upload whatever the opened files' copies hold that the server
    /// lacks: versions saved while there was no session.
    pub(super) fn send_unsent_edits(self: &Rc<Self>) {
        let files = self.open_files.files.borrow();
        let ids: Vec<u64> = files.iter().map(|file| file.id).collect();
        drop(files);
        for id in ids {
            self.sync_edit(id);
        }
    }

    /// Watch an opened file's directory: editors save by writing the file
    /// in place, or by renaming a new one over it.
    fn watch(self: &Rc<Self>, id: u64, dir: &Path) -> Result<gio::FileMonitor, glib::Error> {
        let monitor = gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)?;
        let touched = self.weakly(move |inner, ()| inner.local_touched(id));
        monitor.connect_changed(move |_, _, _, _| _ = touched(()));
        Ok(monitor)
    }

    /// Something in an opened file's directory changed: look at the copy
    /// once the writes stop.
    fn local_touched(self: &Rc<Self>, id: u64) {
        let look = self.weakly(move |inner, ()| {
            inner.open_files.with(id, |file| file.settle = None);
            inner.sync_edit(id);
        });
        self.open_files.with(id, |file| {
            if let Some(pending) = file.settle.take() {
                pending.remove();
            }
            file.settle = Some(glib::timeout_add_local_once(SETTLE, move || _ = look(())));
        });
    }

    /// Upload an opened file's local copy over the remote file, if the
    /// editor saved a version the server does not have.
    fn sync_edit(self: &Rc<Self>, id: u64) {
        let changed = self.open_files.with(id, |file| {
            // Gone in the middle of a save: the write that brings it back
            // looks again.
            let stamp = FileStamp::of(&file.local)?;
            let local = file.local.to_string_lossy().into_owned();
            file.sync
                .local_changed(stamp)
                .then(|| (local, file.remote.clone()))
        });
        let Some((local, remote)) = changed.flatten() else {
            return;
        };
        let label = format!("Save {}", file_name(&remote));
        let transfer = Transfer::Upload { local, remote };
        self.run_transfer(label, transfer, move |inner, succeeded| {
            let again = inner
                .open_files
                .with(id, |file| file.sync.uploaded(succeeded));
            inner.refresh_after(succeeded);
            if again == Some(true) {
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
        let name = file_name(&local.to_string_lossy());
        let failed = self.weakly(move |inner, err: glib::Error| {
            inner.set_error(&format!("Cannot open '{name}': {}", err.message()));
        });
        launcher.launch(
            self.parent_window().as_ref(),
            gio::Cancellable::NONE,
            move |result| match result {
                // Closing the application chooser opens nothing.
                Err(err) if !err.matches(gtk4::DialogError::Dismissed) => _ = failed(err),
                _ => {}
            },
        );
    }

    #[cfg(test)]
    fn launch(self: &Rc<Self>, local: &Path, ask: bool) {
        let launched = &self.open_files.launched;
        launched.borrow_mut().push((local.to_path_buf(), ask));
    }
}
