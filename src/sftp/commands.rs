//! The command vocabulary shared between the GTK sidebar and the SFTP task.

use tokio::sync::{mpsc, oneshot};

/// One progress sample. Sent with `try_send`: a dropped sample is fine, a
/// stalled transfer is not (doc 09 §Transfers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub transferred: u64,
    pub total: u64,
}

/// One directory entry, formatted for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub size: u64,
    /// `YYYY-MM-DD HH:MM` (UTC), or empty when the server sent no mtime.
    pub modified: String,
    /// `rwxr-xr-x`-style permission bits, or empty when unknown.
    pub permissions: String,
    pub is_dir: bool,
}

/// Commands to the SFTP task (doc 09). Every variant but `Cancel` is
/// answered exactly once, with `Err` when the session is dead. Transfers
/// carry an id from [`SftpClient::next_transfer_id`] so `Cancel` can name
/// them.
///
/// [`SftpClient::next_transfer_id`]: crate::sftp::SftpClient::next_transfer_id
pub enum SftpCommand {
    Cd {
        path: String,
        reply: oneshot::Sender<Result<Vec<DirEntry>, String>>,
    },
    Download {
        transfer_id: u64,
        remote_path: String,
        local_path: String,
        progress: mpsc::Sender<Progress>,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    Upload {
        transfer_id: u64,
        local_path: String,
        remote_path: String,
        progress: mpsc::Sender<Progress>,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    Delete {
        path: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Rename {
        from: String,
        to: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Mkdir {
        path: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Cancel {
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
