//! Keeping a file opened in an editor in step with the server (doc 07
//! §Opening files): where its local copy lives, and which of its versions
//! the server has.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
