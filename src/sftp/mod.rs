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
