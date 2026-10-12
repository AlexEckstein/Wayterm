//! The per-sidebar SFTP task, its client handle, and the transfer helpers.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::sftp::commands::{DirEntry, Progress, SftpCommand};
use crate::transport::connection::SshConnection;

const TRANSFER_BUF_SIZE: usize = 32 * 1024;
const COMMAND_QUEUE: usize = 32;

/// Handle to one SFTP task; clones share it. When the last clone drops,
/// the task cancels its transfers and exits — the shared `SshConnection`
/// stays with the supervisor.
#[derive(Clone)]
pub struct SftpClient {
    tx: mpsc::Sender<SftpCommand>,
    next_transfer_id: Arc<AtomicU64>,
}

impl SftpClient {
    /// Spawn the task on the current runtime. No I/O until the first command.
    pub fn new(connection: Arc<SshConnection>) -> Self {
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        tokio::spawn(sftp_task(rx, connection));
        Self {
            tx,
            next_transfer_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A fresh id for a `Download`/`Upload`, so `Cancel` can name it.
    pub fn next_transfer_id(&self) -> u64 {
        self.next_transfer_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn request(&self, cmd: SftpCommand) -> Result<(), SendError<SftpCommand>> {
        self.tx.send(cmd).await
    }

    /// Non-blocking send for the GTK side's try-send + idle-flush pattern.
    pub fn try_request(&self, cmd: SftpCommand) -> Result<(), TrySendError<SftpCommand>> {
        self.tx.try_send(cmd)
    }
}

/// Serve commands until every sender is gone.
pub async fn sftp_task(rx: mpsc::Receiver<SftpCommand>, connection: Arc<SshConnection>) {
    run_sftp_task(rx, move || {
        let connection = Arc::clone(&connection);
        async move { open_session(&connection).await }
    })
    .await
}

/// `SftpSession::new` consumes the channel, so the session is built here
/// rather than in the connection seam.
async fn open_session(connection: &SshConnection) -> Result<SftpSession, String> {
    let channel = connection
        .open_sftp_channel()
        .await
        .map_err(|e| e.to_string())?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| e.to_string())
}

/// The command loop; `open` is the session opener (stubbed in tests).
async fn run_sftp_task<Open, Fut>(mut rx: mpsc::Receiver<SftpCommand>, mut open: Open)
where
    Open: FnMut() -> Fut,
    Fut: Future<Output = Result<SftpSession, String>>,
{
    let mut sftp: Option<Arc<SftpSession>> = None;
    let transfers = Arc::new(Mutex::new(TransferRegistry::default()));
    // Set by a transfer that failed in the background.
    let session_dead = Arc::new(AtomicBool::new(false));

    while let Some(cmd) = rx.recv().await {
        if session_dead.swap(false, Ordering::SeqCst) {
            sftp = None;
        }
        if let SftpCommand::Cancel { transfer_id } = cmd {
            lock(&transfers).cancel(transfer_id);
            continue;
        }
        let session = match &sftp {
            Some(session) => Arc::clone(session),
            None => match open().await {
                Ok(session) => Arc::clone(sftp.insert(Arc::new(session))),
                Err(err) => {
                    cmd.fail(&err);
                    continue;
                }
            },
        };
        // Any failure may mean a dead session; reopening one is cheap.
        if dispatch(session, cmd, &transfers, &session_dead).await {
            sftp = None;
        }
    }
    lock(&transfers).cancel_all();
}

fn lock(registry: &Mutex<TransferRegistry>) -> std::sync::MutexGuard<'_, TransferRegistry> {
    registry.lock().unwrap_or_else(|e| e.into_inner())
}

/// Send `result` and report whether it was a failure.
fn answer<T>(reply: oneshot::Sender<Result<T, String>>, result: Result<T, String>) -> bool {
    let failed = result.is_err();
    let _ = reply.send(result);
    failed
}

/// Run one command; `true` when it failed (drop the cached session).
async fn dispatch(
    session: Arc<SftpSession>,
    cmd: SftpCommand,
    transfers: &Arc<Mutex<TransferRegistry>>,
    session_dead: &Arc<AtomicBool>,
) -> bool {
    let err = |e: russh_sftp::client::error::Error| e.to_string();
    match cmd {
        SftpCommand::Cd { path, reply } => answer(reply, list_dir(&session, &path).await),
        SftpCommand::Delete { path, reply } => answer(reply, delete(&session, &path).await),
        SftpCommand::Rename { from, to, reply } => {
            answer(reply, session.rename(&from, &to).await.map_err(err))
        }
        SftpCommand::Mkdir { path, reply } => {
            answer(reply, session.create_dir(&path).await.map_err(err))
        }
        SftpCommand::Download {
            transfer_id,
            remote_path,
            local_path,
            progress,
            reply,
        } => {
            spawn_transfer(
                session,
                transfers,
                session_dead,
                transfer_id,
                reply,
                |sftp, cancel| async move {
                    download_file(&sftp, &remote_path, &local_path, &progress, &cancel).await
                },
            );
            false
        }
        SftpCommand::Upload {
            transfer_id,
            local_path,
            remote_path,
            progress,
            reply,
        } => {
            spawn_transfer(
                session,
                transfers,
                session_dead,
                transfer_id,
                reply,
                |sftp, cancel| async move {
                    upload_file(&sftp, &local_path, &remote_path, &progress, &cancel).await
                },
            );
            false
        }
        SftpCommand::Cancel { .. } => false,
    }
}

/// Run a transfer on its own task, registered for `Cancel`. A failure that
/// was not a cancel marks the session dead for the next command.
fn spawn_transfer<F, Fut>(
    session: Arc<SftpSession>,
    transfers: &Arc<Mutex<TransferRegistry>>,
    session_dead: &Arc<AtomicBool>,
    transfer_id: u64,
    reply: oneshot::Sender<Result<u64, String>>,
    op: F,
) where
    F: FnOnce(Arc<SftpSession>, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<u64, String>> + Send + 'static,
{
    let cancel = lock(transfers).register(transfer_id);
    let transfers = Arc::clone(transfers);
    let session_dead = Arc::clone(session_dead);
    tokio::spawn(async move {
        let result = op(session, cancel.clone()).await;
        lock(&transfers).finish(transfer_id);
        if result.is_err() && !cancel.is_cancelled() {
            session_dead.store(true, Ordering::SeqCst);
        }
        let _ = reply.send(result);
    });
}

/// Cancellation tokens of the running transfers, by id.
#[derive(Default)]
struct TransferRegistry {
    active: HashMap<u64, CancellationToken>,
}

impl TransferRegistry {
    fn register(&mut self, transfer_id: u64) -> CancellationToken {
        let token = CancellationToken::new();
        self.active.insert(transfer_id, token.clone());
        token
    }

    /// `false` when the id is unknown (finished or never started).
    fn cancel(&self, transfer_id: u64) -> bool {
        self.active
            .get(&transfer_id)
            .map(CancellationToken::cancel)
            .is_some()
    }

    fn finish(&mut self, transfer_id: u64) {
        self.active.remove(&transfer_id);
    }

    fn cancel_all(&mut self) {
        self.active.drain().for_each(|(_, token)| token.cancel());
    }
}

/// List `path`: directories first, then case-insensitive by name.
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<DirEntry>, String> {
    let entries = sftp.read_dir(path).await.map_err(|e| e.to_string())?;
    let mut result: Vec<DirEntry> = entries
        .map(|entry| {
            let attrs = entry.metadata();
            DirEntry {
                name: entry.file_name(),
                size: attrs.size.unwrap_or(0),
                modified: attrs.mtime.map(format_timestamp).unwrap_or_default(),
                permissions: attrs.permissions.map(format_perms).unwrap_or_default(),
                is_dir: attrs.is_dir(),
            }
        })
        .collect();
    sort_entries(&mut result);
    Ok(result)
}

/// russh-sftp 3 splits removal into file/dir calls, so stat first.
async fn delete(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let attrs = sftp
        .symlink_metadata(path)
        .await
        .map_err(|e| e.to_string())?;
    if attrs.is_dir() {
        sftp.remove_dir(path).await
    } else {
        sftp.remove_file(path).await
    }
    .map_err(|e| e.to_string())
}

/// Copy `src` to `dst` in 32 KiB chunks, reporting progress, until EOF or
/// cancellation. Returns the byte count.
async fn copy_with_progress(
    mut src: impl tokio::io::AsyncRead + Unpin,
    dst: &mut (impl tokio::io::AsyncWrite + Unpin),
    total: u64,
    progress: &mpsc::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<u64, String> {
    let mut buf = vec![0u8; TRANSFER_BUF_SIZE];
    let mut transferred = 0u64;
    loop {
        let read = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("transfer cancelled".into()),
            read = src.read(&mut buf) => read.map_err(|e| e.to_string())?,
        };
        if read == 0 {
            return Ok(transferred);
        }
        dst.write_all(&buf[..read])
            .await
            .map_err(|e| e.to_string())?;
        transferred += read as u64;
        let _ = progress.try_send(Progress { transferred, total });
    }
}

async fn download_file(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &str,
    progress: &mpsc::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<u64, String> {
    let mut remote = sftp.open(remote_path).await.map_err(|e| e.to_string())?;
    let total = remote
        .metadata()
        .await
        .ok()
        .and_then(|attrs| attrs.size)
        .unwrap_or(0);
    let mut local = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| e.to_string())?;
    let result = copy_with_progress(&mut remote, &mut local, total, progress, cancel).await;
    let _ = local.flush().await;
    result
}

/// Unlike a download, a failed flush or close here is an error: flushing
/// drains the server's write acknowledgements, so the remote file may be
/// incomplete.
async fn upload_file(
    sftp: &SftpSession,
    local_path: &str,
    remote_path: &str,
    progress: &mpsc::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<u64, String> {
    let local = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| e.to_string())?;
    let total = local.metadata().await.map(|m| m.len()).unwrap_or_default();
    let mut remote = sftp.create(remote_path).await.map_err(|e| e.to_string())?;
    let transferred = copy_with_progress(local, &mut remote, total, progress, cancel).await?;
    remote.flush().await.map_err(|e| e.to_string())?;
    remote.close().await.map_err(|e| e.to_string())?;
    Ok(transferred)
}

/// Directories first, then case-insensitive alphabetical (doc 09).
fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// `rwxr-xr-x` from a mode word, ignoring file-type and setuid/sticky bits.
fn format_perms(mode: u32) -> String {
    let mode = mode & 0o777;
    let mut out = String::with_capacity(9);
    for group in [mode >> 6, mode >> 3, mode] {
        for (bit, ch) in [(0o4, 'r'), (0o2, 'w'), (0o1, 'x')] {
            out.push(if group & bit != 0 { ch } else { '-' });
        }
    }
    out
}

/// `YYYY-MM-DD HH:MM` in UTC (no timezone database dependency).
fn format_timestamp(secs: u32) -> String {
    let days = i64::from(secs) / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Days since 1970-01-01 → (year, month, day): Howard Hinnant's
/// `civil_from_days`, exact for every `u32` timestamp.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests;
