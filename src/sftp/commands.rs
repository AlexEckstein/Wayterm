//! The command vocabulary shared between the GTK sidebar and the SFTP task.

use tokio::sync::{mpsc, oneshot};

/// One progress sample. Sent with `try_send`: a dropped sample is fine, a
/// stalled transfer is not (doc 07 §Transfers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes done so far.
    pub transferred: u64,
    /// The file's size; 0 when unknown.
    pub total: u64,
}

/// One directory entry, formatted for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// File name, without its directory.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// `YYYY-MM-DD HH:MM` (UTC), or empty when the server sent no mtime.
    pub modified: String,
    /// `rwxr-xr-x`-style permission bits, or empty when unknown.
    pub permissions: String,
    /// Whether it is a directory.
    pub is_dir: bool,
}

/// Commands to the SFTP task (doc 07). Every variant but `Cancel` is
/// answered exactly once, with `Err` when the session is dead. Transfers
/// carry an id from [`SftpClient::next_transfer_id`] so `Cancel` can name
/// them.
///
/// [`SftpClient::next_transfer_id`]: crate::sftp::SftpClient::next_transfer_id
pub enum SftpCommand {
    /// List `path`; doubles as "go to".
    Cd {
        /// Directory to list.
        path: String,
        /// The entries, or why not.
        reply: oneshot::Sender<Result<Vec<DirEntry>, String>>,
    },
    /// Copy a remote file to a local path.
    Download {
        /// Pre-generated id, for `Cancel`.
        transfer_id: u64,
        /// Remote source.
        remote_path: String,
        /// Local destination.
        local_path: String,
        /// Progress samples.
        progress: mpsc::Sender<Progress>,
        /// Bytes copied, or why not.
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// Copy a local file to a remote path.
    Upload {
        /// Pre-generated id, for `Cancel`.
        transfer_id: u64,
        /// Local source.
        local_path: String,
        /// Remote destination.
        remote_path: String,
        /// Progress samples.
        progress: mpsc::Sender<Progress>,
        /// Bytes copied, or why not.
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// Remove a file or an empty directory.
    Delete {
        /// Path to remove.
        path: String,
        /// Done, or why not.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Rename or move.
    Rename {
        /// Current path.
        from: String,
        /// New path.
        to: String,
        /// Done, or why not.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Create a directory.
    Mkdir {
        /// Directory to create.
        path: String,
        /// Done, or why not.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Stop a running transfer; never answered.
    Cancel {
        /// The transfer to stop.
        transfer_id: u64,
    },
}

impl SftpCommand {
    /// Answer with `err`, whatever the command (`Cancel` has no reply). A
    /// requester that stopped waiting needs no answer.
    pub(crate) fn fail(self, err: &str) {
        let err = err.to_string();
        match self {
            Self::Cd { reply, .. } => _ = reply.send(Err(err)),
            Self::Download { reply, .. } | Self::Upload { reply, .. } => _ = reply.send(Err(err)),
            Self::Delete { reply, .. } | Self::Rename { reply, .. } | Self::Mkdir { reply, .. } => {
                _ = reply.send(Err(err))
            }
            Self::Cancel { .. } => {}
        }
    }
}
