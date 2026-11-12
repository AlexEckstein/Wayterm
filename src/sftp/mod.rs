//! The tokio half of the SFTP sidebar (doc 07). The GTK side holds an
//! [`SftpClient`], sends [`SftpCommand`]s over a bounded channel, and awaits
//! the oneshot replies on the main loop; it never touches `russh-sftp`.
//!
//! The task opens the SFTP session lazily over the shared
//! [`SshConnection`](crate::transport::connection::SshConnection), drops it on
//! any failure, and reopens it on the next command, so a server that closes
//! the subsystem shows up as an error reply and recovers. Transfers run as
//! their own tasks so `Cancel` and browsing stay responsive.
//!
//! A local-shell tab's sidebar gets the same commands answered from this
//! machine's files (`local`, doc 07 §Local tabs).
//!
//! [`ShellCwdProbe`] is not SFTP: it asks the remote where the tab's shell
//! is, so the sidebar can follow it (doc 07 §Following the shell).

pub mod commands;
mod listing;
pub mod local;
pub mod shell_cwd;
pub mod task;
mod transfer;

pub use commands::{DirEntry, Progress, SftpCommand};
pub use shell_cwd::ShellCwdProbe;
pub use task::{sftp_task, SftpClient};
