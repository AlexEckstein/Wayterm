//! Tokio-side SFTP: commands, the per-sidebar task, and transfer helpers.
//!
//! This is the tokio half of the SFTP sidebar (doc 09). The GTK side never
//! touches `russh-sftp`: it holds an [`SftpClient`], pushes [`SftpCommand`]s
//! over a bounded channel, and bridges the oneshot replies onto the main
//! loop. Everything below runs on tokio workers — no `.await` in GTK code.
//!
//! Phase-3 (P3-1) refinements of the doc 09 sketch, kept consistent with the
//! docs:
//!
//! * **Session ownership.** [`SshConnection::open_sftp_channel`] opens the
//!   channel *and* requests the `sftp` subsystem; the task converts it into a
//!   `russh_sftp::client::SftpSession` with `channel.into_stream()` on first
//!   use, because `SftpSession::new` consumes the channel by value. The
//!   session is cached, dropped on any failure, and reopened lazily — so a
//!   server that closes the subsystem produces an observable error reply and
//!   the next command recovers without killing the task.
//! * **`transfer_id`.** `Download`/`Upload` carry a UI-pre-generated id that
//!   `Cancel { transfer_id }` matches against the task's cancellation-token
//!   map; ids come from [`SftpClient::next_transfer_id`].
//! * **Transfers are sub-tasks.** A transfer runs in its own spawned task so
//!   `Cancel` and browsing commands stay responsive mid-transfer (doc 09's
//!   inline sketch would queue `Cancel` behind the whole transfer). Replies
//!   still arrive on the command's oneshot, and dropping the last
//!   [`SftpClient`] cancels in-flight transfers before the task exits.
//! * **russh-sftp 3.0** (docs anchored 2.4 — see doc 11 §Version anchors):
//!   `read_dir` yields a sync iterator, `DirEntry::file_name()` returns
//!   `String`, `File::metadata()` is async, and `remove` is split into
//!   `remove_file`/`remove_dir` (Delete stats first).
//!
//! [`SshConnection::open_sftp_channel`]: crate::transport::connection::SshConnection::open_sftp_channel

pub mod commands;
pub mod task;

pub use commands::{DirEntry, Progress, SftpCommand};
pub use task::{sftp_task, SftpClient};

/// No-op that exercises the public SFTP API so the module stays compiled and
/// lint-clean in the binary target until the GTK sidebar (P3-2) wires it in.
/// Same pattern as [`crate::transport::touch_public_api`]; removed once the
/// UI calls these items directly.
pub fn touch_public_api() {
    use tokio::sync::{mpsc, oneshot};

    let _ = sftp_task;
    let _ = SftpClient::new;
    let _ = SftpClient::next_transfer_id;
    let _ = SftpClient::request;
    let _ = SftpClient::try_request;

    let entry = DirEntry {
        name: String::new(),
        size: 0,
        modified: String::new(),
        permissions: String::new(),
        is_dir: false,
    };
    let _ = (
        entry.name.len(),
        entry.size,
        entry.modified.len(),
        entry.permissions.len(),
        entry.is_dir,
    );

    let progress = Progress {
        transferred: 0,
        total: 0,
    };
    let _ = (progress.transferred, progress.total);

    let (dir_reply, _) = oneshot::channel();
    let (bytes_reply, _) = oneshot::channel();
    let (more_bytes_reply, _) = oneshot::channel();
    let (unit_reply, _) = oneshot::channel();
    let (other_unit_reply, _) = oneshot::channel();
    let (another_unit_reply, _) = oneshot::channel();
    let (progress_tx, _) = mpsc::channel(1);

    for cmd in [
        SftpCommand::Cd {
            path: String::new(),
            reply: dir_reply,
        },
        SftpCommand::Download {
            transfer_id: 0,
            remote_path: String::new(),
            local_path: String::new(),
            progress: progress_tx.clone(),
            reply: bytes_reply,
        },
        SftpCommand::Upload {
            transfer_id: 0,
            local_path: String::new(),
            remote_path: String::new(),
            progress: progress_tx,
            reply: more_bytes_reply,
        },
        SftpCommand::Delete {
            path: String::new(),
            reply: unit_reply,
        },
        SftpCommand::Rename {
            from: String::new(),
            to: String::new(),
            reply: other_unit_reply,
        },
        SftpCommand::Mkdir {
            path: String::new(),
            reply: another_unit_reply,
        },
        SftpCommand::Cancel { transfer_id: 0 },
    ] {
        match cmd {
            SftpCommand::Cd { path, reply } => {
                let _ = path.len();
                let _ = reply.send(Ok(Vec::new()));
            }
            SftpCommand::Download {
                transfer_id,
                remote_path,
                local_path,
                progress,
                reply,
            }
            | SftpCommand::Upload {
                transfer_id,
                local_path,
                remote_path,
                progress,
                reply,
            } => {
                let _ = transfer_id;
                let _ = remote_path.len();
                let _ = local_path.len();
                let _ = progress.max_capacity();
                let _ = reply.send(Ok(0));
            }
            SftpCommand::Delete { path, reply } | SftpCommand::Mkdir { path, reply } => {
                let _ = path.len();
                let _ = reply.send(Ok(()));
            }
            SftpCommand::Rename { from, to, reply } => {
                let _ = (from.len(), to.len());
                let _ = reply.send(Ok(()));
            }
            SftpCommand::Cancel { transfer_id } => {
                let _ = transfer_id;
            }
        }
    }
}
