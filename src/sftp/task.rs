//! The per-sidebar SFTP task and its client handle.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use russh_sftp::client::SftpSession;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendError;
use tokio_util::sync::CancellationToken;

use crate::sftp::commands::{DirEntry, Progress, SftpCommand};
use crate::sftp::listing::sort_entries;
use crate::sftp::local::local_task;
use crate::sftp::transfer::{TransferRecord, Transfers, copy_with_progress, open_source, text};
use crate::transport::connection::SshConnection;

const COMMAND_QUEUE: usize = 32;

/// Handle to one SFTP task; clones share it. When the last clone drops,
/// or on [`shutdown`](Self::shutdown), the task cancels its transfers and
/// exits — the shared `SshConnection` stays with the supervisor.
#[derive(Clone)]
pub struct SftpClient {
    tx: mpsc::Sender<SftpCommand>,
    next_transfer_id: Arc<AtomicU64>,
    shutdown: CancellationToken,
}

impl SftpClient {
    /// Spawn the task on the current runtime. No I/O until the first command.
    pub fn new(connection: Arc<SshConnection>) -> Self {
        Self::spawn(|rx, shutdown| sftp_task(rx, connection, shutdown))
    }

    /// Spawn a task that answers from this machine's own files instead: a
    /// local-shell tab's sidebar (doc 07 §Local tabs).
    pub fn local() -> Self {
        Self::spawn(local_task)
    }

    fn spawn<Task>(
        task: impl FnOnce(mpsc::Receiver<SftpCommand>, CancellationToken) -> Task,
    ) -> Self
    where
        Task: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        let shutdown = CancellationToken::new();
        tokio::spawn(task(rx, shutdown.clone()));
        Self::from_parts(tx, shutdown)
    }

    /// A client whose commands go to `tx` and whose shutdown fires
    /// `shutdown`, whatever serves them.
    pub(crate) fn from_parts(tx: mpsc::Sender<SftpCommand>, shutdown: CancellationToken) -> Self {
        Self {
            tx,
            next_transfer_id: Arc::new(AtomicU64::new(1)),
            shutdown,
        }
    }

    /// End the task now: the command in progress is dropped, queued ones
    /// are discarded unanswered, and running transfers are cancelled.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// A fresh id for a `Download`/`Upload`, so `Cancel` can name it.
    pub fn next_transfer_id(&self) -> u64 {
        self.next_transfer_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn request(&self, cmd: SftpCommand) -> Result<(), SendError<SftpCommand>> {
        self.tx.send(cmd).await
    }

    /// The task's command channel, for the GTK side's never-blocking
    /// sender (doc 02 §Backpressure).
    pub fn sender(&self) -> mpsc::Sender<SftpCommand> {
        self.tx.clone()
    }
}

/// Serve commands until every sender is gone or `shutdown` fires.
pub async fn sftp_task(
    rx: mpsc::Receiver<SftpCommand>,
    connection: Arc<SshConnection>,
    shutdown: CancellationToken,
) {
    run_sftp_task(rx, shutdown, move || {
        let connection = Arc::clone(&connection);
        async move { open_session(&connection).await }
    })
    .await
}

/// `SftpSession::new` consumes the channel, so the session is built here
/// rather than in the connection seam.
async fn open_session(connection: &SshConnection) -> Result<SftpSession, String> {
    let channel = connection.open_sftp_channel().await.map_err(text)?;
    let session = SftpSession::new(channel.into_stream())
        .await
        .map_err(text)?;
    tracing::info!(
        host = %connection.host(),
        port = connection.port(),
        "SFTP session opened"
    );
    Ok(session)
}

/// The command loop; `open` is the session opener (stubbed in tests).
/// `shutdown` interrupts it anywhere, even inside a command's own await.
async fn run_sftp_task<Open, Fut>(
    mut rx: mpsc::Receiver<SftpCommand>,
    shutdown: CancellationToken,
    open: Open,
) where
    Open: FnMut() -> Fut,
    Fut: Future<Output = Result<SftpSession, String>>,
{
    let transfers = Transfers::default();
    let serving = serve(&mut rx, open, &transfers);
    transfers.serve_until(&shutdown, serving).await;
    tracing::debug!("SFTP task ended");
}

async fn serve<Open, Fut>(
    rx: &mut mpsc::Receiver<SftpCommand>,
    mut open: Open,
    transfers: &Transfers,
) where
    Open: FnMut() -> Fut,
    Fut: Future<Output = Result<SftpSession, String>>,
{
    let mut sftp: Option<Arc<SftpSession>> = None;
    // Any failure may mean a dead session, and reopening one is cheap: the
    // command after a failed one, or a failed transfer, opens a new one.
    let failed = Arc::new(AtomicBool::new(false));

    while let Some(cmd) = rx.recv().await {
        if failed.swap(false, Ordering::SeqCst) {
            sftp = None;
        }
        if let SftpCommand::Cancel { transfer_id } = cmd {
            transfers.cancel(transfer_id);
            continue;
        }
        let session = match &sftp {
            Some(session) => Arc::clone(session),
            None => match open().await {
                Ok(session) => Arc::clone(sftp.insert(Arc::new(session))),
                Err(err) => {
                    tracing::warn!(error = %err, "SFTP session failed to open");
                    cmd.fail(&err);
                    continue;
                }
            },
        };
        dispatch(session, cmd, transfers, &failed).await;
    }
}

/// Note a failure in `failed` on its way to whoever asked.
fn noted<T>(failed: &AtomicBool, result: Result<T, String>) -> Result<T, String> {
    if result.is_err() {
        failed.store(true, Ordering::SeqCst);
    }
    result
}

/// Run one command. A transfer runs on, answering when it ends; a
/// cancelled one is dropped mid-flight, and so notes no failure.
async fn dispatch(
    session: Arc<SftpSession>,
    cmd: SftpCommand,
    transfers: &Transfers,
    failed: &Arc<AtomicBool>,
) {
    match cmd {
        SftpCommand::Cd { path, reply } => {
            let _ = reply.send(noted(failed, list_dir(&session, &path).await));
        }
        SftpCommand::Delete { path, reply } => {
            let _ = reply.send(noted(failed, delete(&session, &path).await));
        }
        SftpCommand::Rename { from, to, reply } => {
            let renamed = session.rename(&from, &to).await.map_err(text);
            let _ = reply.send(noted(failed, renamed));
        }
        SftpCommand::Mkdir { path, reply } => {
            let made = session.create_dir(&path).await.map_err(text);
            let _ = reply.send(noted(failed, made));
        }
        SftpCommand::Download {
            transfer_id,
            remote_path,
            local_path,
            progress,
            reply,
        } => {
            let failed = Arc::clone(failed);
            transfers.spawn(transfer_id, reply, async move {
                let record =
                    TransferRecord::start("download", transfer_id, &local_path, &remote_path);
                let done = download_file(&session, &remote_path, &local_path, &progress).await;
                record.finish(&done);
                noted(&failed, done)
            });
        }
        SftpCommand::Upload {
            transfer_id,
            local_path,
            remote_path,
            progress,
            reply,
        } => {
            let failed = Arc::clone(failed);
            transfers.spawn(transfer_id, reply, async move {
                let record =
                    TransferRecord::start("upload", transfer_id, &local_path, &remote_path);
                let done = upload_file(&session, &local_path, &remote_path, &progress).await;
                record.finish(&done);
                noted(&failed, done)
            });
        }
        SftpCommand::Cancel { .. } => {}
    }
}

/// List `path`: directories first, then case-insensitive by name.
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<DirEntry>, String> {
    let entries = sftp.read_dir(path).await.map_err(text)?;
    let mut result: Vec<DirEntry> = entries
        .map(|entry| {
            let attrs = entry.metadata();
            DirEntry::new(
                entry.file_name(),
                attrs.size.unwrap_or(0),
                attrs.mtime,
                attrs.permissions,
                attrs.is_dir(),
            )
        })
        .collect();
    sort_entries(&mut result);
    Ok(result)
}

/// russh-sftp 3 splits removal into file/dir calls, so stat first.
async fn delete(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let attrs = sftp.symlink_metadata(path).await.map_err(text)?;
    if attrs.is_dir() {
        sftp.remove_dir(path).await
    } else {
        sftp.remove_file(path).await
    }
    .map_err(text)
}

async fn download_file(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &str,
    progress: &mpsc::Sender<Progress>,
) -> Result<u64, String> {
    let mut remote = sftp.open(remote_path).await.map_err(text)?;
    let attrs = remote.metadata().await.ok();
    let total = attrs.and_then(|attrs| attrs.size).unwrap_or(0);
    let mut local = tokio::fs::File::create(local_path).await.map_err(text)?;
    let transferred = copy_with_progress(&mut remote, &mut local, total, progress).await?;
    // tokio writes in the background: only the flush reports a failed last
    // write (a full disk), which would leave the local file short.
    local.flush().await.map_err(text)?;
    Ok(transferred)
}

/// A failed flush or close fails the upload: flushing drains the server's
/// write acknowledgements, so the remote file may be incomplete.
async fn upload_file(
    sftp: &SftpSession,
    local_path: &str,
    remote_path: &str,
    progress: &mpsc::Sender<Progress>,
) -> Result<u64, String> {
    let (local, meta) = open_source(local_path).await?;
    let mut remote = sftp.create(remote_path).await.map_err(text)?;
    let transferred = copy_with_progress(local, &mut remote, meta.len(), progress).await?;
    remote.flush().await.map_err(text)?;
    remote.close().await.map_err(text)?;
    Ok(transferred)
}

#[cfg(test)]
mod tests;
