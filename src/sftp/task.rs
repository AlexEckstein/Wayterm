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

/// Read/write buffer size for both transfer directions (doc 09 §Transfers).
const TRANSFER_BUF_SIZE: usize = 32 * 1024;

/// Command-queue depth between the sidebar and the task. Bounded per doc 09:
/// if SFTP is slow the UI's `try_send` fails fast and its idle-flush retries.
const COMMAND_QUEUE: usize = 32;

/// Handle to the SFTP task (doc 09 §SFTP Task): the sidebar's seam onto
/// tokio. Cloning shares the same task. When the last clone is dropped the
/// task sees its channel close, cancels in-flight transfers, drops the
/// `SftpSession`, and exits — it does NOT disconnect the shared
/// [`SshConnection`]; that stays with the connection supervisor (doc 04).
#[derive(Clone)]
pub struct SftpClient {
    tx: mpsc::Sender<SftpCommand>,
    next_transfer_id: Arc<AtomicU64>,
}

impl SftpClient {
    /// Spawn one SFTP task on the current tokio runtime.
    ///
    /// The SFTP session itself is opened lazily on the first command, so this
    /// does no I/O.
    pub fn new(connection: Arc<SshConnection>) -> Self {
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        tokio::spawn(sftp_task(rx, connection));
        Self {
            tx,
            next_transfer_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Next transfer id for a `Download`/`Upload` command.
    ///
    /// Phase-3 adaptation: the UI pre-generates the id (doc 09's sketch had
    /// the task assign it) so it can send `Cancel { transfer_id }` later.
    /// Ids only need to be unique within this client's task.
    pub fn next_transfer_id(&self) -> u64 {
        self.next_transfer_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Queue one command, waiting for room if the bounded queue is full.
    pub async fn request(&self, cmd: SftpCommand) -> Result<(), SendError<SftpCommand>> {
        self.tx.send(cmd).await
    }

    /// Non-blocking variant of [`request`](Self::request) for the GTK side's
    /// try-send + idle-flush pattern (doc 09 §SFTP Task).
    pub fn try_request(&self, cmd: SftpCommand) -> Result<(), TrySendError<SftpCommand>> {
        self.tx.try_send(cmd)
    }
}

/// The tokio-side SFTP task (doc 09 §SFTP Task).
///
/// Runs until every sender is dropped. The `SftpSession` is created lazily
/// from the first command's connection, cached, dropped on failure, and
/// reopened lazily by the next command — so a server closing the subsystem
/// is observable (the reply errors) and recoverable without killing the task.
pub async fn sftp_task(rx: mpsc::Receiver<SftpCommand>, connection: Arc<SshConnection>) {
    run_sftp_task(rx, move || {
        let connection = Arc::clone(&connection);
        async move { open_session(&connection).await }
    })
    .await
}

/// Open a fresh SFTP session over the shared connection.
///
/// [`SshConnection::open_sftp_channel`] already opened the channel and
/// requested the `sftp` subsystem; `into_stream` consumes the channel, which
/// is why session creation lives here rather than in the connection seam.
async fn open_session(connection: &SshConnection) -> Result<SftpSession, String> {
    let channel = connection
        .open_sftp_channel()
        .await
        .map_err(|e| e.to_string())?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| e.to_string())
}

/// Command loop. `open` is the (mockable) session opener — `sftp_task` wires
/// it to the real connection; tests drive the loop with a stub.
async fn run_sftp_task<Open, Fut>(mut rx: mpsc::Receiver<SftpCommand>, mut open: Open)
where
    Open: FnMut() -> Fut,
    Fut: Future<Output = Result<SftpSession, String>>,
{
    let mut sftp: Option<Arc<SftpSession>> = None;
    let transfers = Arc::new(Mutex::new(TransferRegistry::default()));
    let session_dead = Arc::new(AtomicBool::new(false));

    while let Some(cmd) = rx.recv().await {
        // A transfer that failed behind our back may have taken the session
        // with it — forget it so this command reopens lazily.
        if session_dead.swap(false, Ordering::SeqCst) {
            sftp = None;
        }

        if let SftpCommand::Cancel { transfer_id } = cmd {
            transfers
                .lock()
                .expect("transfer registry poisoned")
                .cancel(transfer_id);
            continue;
        }

        if sftp.is_none() {
            match open().await {
                Ok(session) => sftp = Some(Arc::new(session)),
                Err(err) => {
                    reply_open_error(cmd, &err);
                    continue;
                }
            }
        }
        let session = sftp.clone().expect("session opened just above");
        if dispatch(session, cmd, transfers.clone(), Arc::clone(&session_dead)).await {
            // Any failure can mean the subsystem closed or the connection
            // dropped: forget the session so the next command reopens it.
            sftp = None;
        }
    }

    // Every sender dropped (sidebar gone): abort in-flight transfers and let
    // the session drop. The shared SshConnection stays up (doc 04).
    transfers
        .lock()
        .expect("transfer registry poisoned")
        .cancel_all();
}

/// Execute one command against `session`, sending its reply.
///
/// Returns `true` when the command failed and the cached session should be
/// dropped — any failure may indicate a dead session, and reopening one
/// subsystem channel on the next command is cheap.
async fn dispatch(
    session: Arc<SftpSession>,
    cmd: SftpCommand,
    transfers: Arc<Mutex<TransferRegistry>>,
    session_dead: Arc<AtomicBool>,
) -> bool {
    match cmd {
        SftpCommand::Cd { path, reply } => match list_dir(&session, &path).await {
            Ok(entries) => {
                let _ = reply.send(Ok(entries));
                false
            }
            Err(err) => {
                let _ = reply.send(Err(err));
                true
            }
        },
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
                move |sftp, cancel| async move {
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
                move |sftp, cancel| async move {
                    upload_file(&sftp, &local_path, &remote_path, &progress, &cancel).await
                },
            );
            false
        }
        SftpCommand::Delete { path, reply } => match delete(&session, &path).await {
            Ok(()) => {
                let _ = reply.send(Ok(()));
                false
            }
            Err(err) => {
                let _ = reply.send(Err(err));
                true
            }
        },
        SftpCommand::Rename { from, to, reply } => {
            match session.rename(&from, &to).await.map_err(|e| e.to_string()) {
                Ok(()) => {
                    let _ = reply.send(Ok(()));
                    false
                }
                Err(err) => {
                    let _ = reply.send(Err(err));
                    true
                }
            }
        }
        SftpCommand::Mkdir { path, reply } => {
            match session.create_dir(&path).await.map_err(|e| e.to_string()) {
                Ok(()) => {
                    let _ = reply.send(Ok(()));
                    false
                }
                Err(err) => {
                    let _ = reply.send(Err(err));
                    true
                }
            }
        }
        // Handled by the caller before any session is touched.
        SftpCommand::Cancel { .. } => false,
    }
}

/// Answer a command that could not even reach a session (open failed).
fn reply_open_error(cmd: SftpCommand, err: &str) {
    match cmd {
        SftpCommand::Cd { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        SftpCommand::Download { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        SftpCommand::Upload { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        SftpCommand::Delete { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        SftpCommand::Rename { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        SftpCommand::Mkdir { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        SftpCommand::Cancel { .. } => {}
    }
}

/// Run one download/upload in its own spawned task so `Cancel` (and browsing
/// commands) stay responsive while bytes move; the reply still arrives on the
/// command's oneshot. Failures flag the shared session as dead so the next
/// command reopens it.
fn spawn_transfer<F, Fut>(
    session: Arc<SftpSession>,
    transfers: Arc<Mutex<TransferRegistry>>,
    session_dead: Arc<AtomicBool>,
    transfer_id: u64,
    reply: oneshot::Sender<Result<u64, String>>,
    op: F,
) where
    F: FnOnce(Arc<SftpSession>, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<u64, String>> + Send + 'static,
{
    let cancel = transfers
        .lock()
        .expect("transfer registry poisoned")
        .register(transfer_id);
    tokio::spawn(async move {
        let result = op(session, cancel.clone()).await;
        transfers
            .lock()
            .expect("transfer registry poisoned")
            .finish(transfer_id);
        if result.is_err() && !cancel.is_cancelled() {
            session_dead.store(true, Ordering::SeqCst);
        }
        let _ = reply.send(result);
    });
}

/// Live transfers by id (doc 09: `Cancel { transfer_id }` → token map).
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

    /// Cancel a running transfer; `false` when the id is unknown (already
    /// finished, or never registered).
    fn cancel(&self, transfer_id: u64) -> bool {
        match self.active.get(&transfer_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    fn finish(&mut self, transfer_id: u64) {
        self.active.remove(&transfer_id);
    }

    fn cancel_all(&mut self) {
        for token in self.active.values() {
            token.cancel();
        }
        self.active.clear();
    }
}

/// List `path`, sorted per doc 09: directories first, then case-insensitive
/// alphabetical.
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<DirEntry>, String> {
    let entries = sftp.read_dir(path).await.map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    for entry in entries {
        let attrs = entry.metadata();
        result.push(DirEntry {
            name: entry.file_name(),
            size: attrs.size.unwrap_or(0),
            modified: attrs.mtime.map(format_timestamp).unwrap_or_default(),
            permissions: attrs.permissions.map(format_perms).unwrap_or_default(),
            is_dir: attrs.is_dir(),
        });
    }
    sort_entries(&mut result);
    Ok(result)
}

/// Remove a file or an empty directory. Doc 09 used `sftp.remove`, which
/// russh-sftp 3.0 split into `remove_file`/`remove_dir` — stat first so
/// directories and symlinks take the right path.
async fn delete(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let is_dir = sftp
        .symlink_metadata(path)
        .await
        .map(|attrs| attrs.is_dir())
        .map_err(|e| e.to_string())?;
    if is_dir {
        sftp.remove_dir(path).await.map_err(|e| e.to_string())
    } else {
        sftp.remove_file(path).await.map_err(|e| e.to_string())
    }
}

/// Download `remote_path` to `local_path`, reporting progress per doc 09:
/// 32 KiB buffer, `try_send` (drop on full — never block), flush, return the
/// transferred byte count.
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
        .map(|attrs| attrs.size.unwrap_or(0))
        .unwrap_or_default();
    let mut local = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; TRANSFER_BUF_SIZE];
    let mut transferred = 0u64;
    loop {
        let read = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = local.flush().await;
                return Err("transfer cancelled".to_string());
            }
            read = remote.read(&mut buf) => read.map_err(|e| e.to_string())?,
        };
        if read == 0 {
            break;
        }
        local
            .write_all(&buf[..read])
            .await
            .map_err(|e| e.to_string())?;
        transferred += read as u64;
        let _ = progress.try_send(Progress { transferred, total });
    }
    let _ = local.flush().await;
    Ok(transferred)
}

/// Upload `local_path` to `remote_path` — the mirror image of
/// [`download_file`], with the same buffer, progress, and cancellation.
///
/// Unlike the download side, a failed flush is an error: flushing drains the
/// server's write acknowledgements, so a failure there means the remote file
/// is incomplete and must not be reported as a successful transfer.
async fn upload_file(
    sftp: &SftpSession,
    local_path: &str,
    remote_path: &str,
    progress: &mpsc::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<u64, String> {
    let mut local = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| e.to_string())?;
    let total = local.metadata().await.map(|m| m.len()).unwrap_or_default();
    let mut remote = sftp.create(remote_path).await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; TRANSFER_BUF_SIZE];
    let mut transferred = 0u64;
    loop {
        let read = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("transfer cancelled".to_string()),
            read = local.read(&mut buf) => read.map_err(|e| e.to_string())?,
        };
        if read == 0 {
            break;
        }
        remote
            .write_all(&buf[..read])
            .await
            .map_err(|e| e.to_string())?;
        transferred += read as u64;
        let _ = progress.try_send(Progress { transferred, total });
    }
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

/// Render Unix permission bits as `rwxr-xr-x`-style nine characters (doc 09).
///
/// Masks off the file-type bits OpenSSH packs into the same word
/// (`S_IFDIR` & co.) as well as setuid/setgid/sticky.
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

/// Format a Unix timestamp (seconds since the epoch) as `YYYY-MM-DD HH:MM`
/// in UTC, per doc 09's `format_timestamp`.
///
/// UTC only: a timezone database is not worth a new dependency for a
/// file-listing column; local-time rendering stays a UI concern.
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

/// Days since 1970-01-01 → (year, month, day), Howard Hinnant's
/// `civil_from_days` — exact for the whole `u32` timestamp range.
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
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            size: 0,
            modified: String::new(),
            permissions: String::new(),
            is_dir,
        }
    }

    async fn expect_err<T: std::fmt::Debug>(rx: oneshot::Receiver<Result<T, String>>) -> String {
        timeout(Duration::from_secs(5), rx)
            .await
            .expect("reply timed out")
            .expect("task exited without replying")
            .expect_err("expected an error reply")
    }

    #[test]
    fn dir_entries_sort_directories_first_then_case_insensitive() {
        let mut entries = vec![
            entry("beta", false),
            entry("Alpha", false),
            entry("gamma", true),
            entry("Beta", true),
            entry("alpha", true),
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "gamma", "Alpha", "beta"]);
    }

    #[test]
    fn format_perms_masks_file_type_bits_to_rwx_style() {
        assert_eq!(format_perms(0o100644), "rw-r--r--");
        assert_eq!(format_perms(0o40755), "rwxr-xr-x");
        assert_eq!(format_perms(0o100750), "rwxr-x---");
        assert_eq!(format_perms(0o100777), "rwxrwxrwx");
        assert_eq!(format_perms(0), "---------");
        // setuid/setgid/sticky are not part of the nine-character display
        assert_eq!(format_perms(0o104755), "rwxr-xr-x");
    }

    #[test]
    fn format_timestamp_formats_utc_minutes() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00");
        assert_eq!(format_timestamp(86_399), "1970-01-01 23:59");
        assert_eq!(format_timestamp(86_400), "1970-01-02 00:00");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13");
        // Past the year 2100, where naive date math goes wrong
        assert_eq!(format_timestamp(u32::MAX), "2106-02-07 06:28");
    }

    #[test]
    fn transfer_registry_cancels_by_id_and_forgets_finished_transfers() {
        let mut registry = TransferRegistry::default();
        let token = registry.register(7);
        assert!(!token.is_cancelled());
        assert!(!registry.cancel(99), "unknown id must be a no-op");

        assert!(registry.cancel(7));
        assert!(token.is_cancelled());

        registry.finish(7);
        assert!(!registry.cancel(7), "finished transfer must be forgotten");
    }

    #[test]
    fn transfer_registry_cancel_all_aborts_every_live_transfer() {
        let mut registry = TransferRegistry::default();
        let first = registry.register(1);
        let second = registry.register(2);
        registry.cancel_all();
        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        assert!(registry.active.is_empty());
    }

    #[tokio::test]
    async fn failed_session_open_replies_error_and_reopens_on_every_command() {
        let opens = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<SftpCommand>(16);
        let task = tokio::spawn({
            let opens = Arc::clone(&opens);
            run_sftp_task(rx, move || {
                opens.fetch_add(1, Ordering::SeqCst);
                async { Err::<SftpSession, String>("subsystem unavailable".to_string()) }
            })
        });

        let (reply, reply_rx) = oneshot::channel();
        tx.send(SftpCommand::Cd {
            path: ".".into(),
            reply,
        })
        .await
        .unwrap();
        let err = expect_err(reply_rx).await;
        assert!(err.contains("subsystem unavailable"), "got: {err}");

        let (progress, _keep) = mpsc::channel(4);
        let (reply, reply_rx) = oneshot::channel();
        tx.send(SftpCommand::Download {
            transfer_id: 1,
            remote_path: "/remote".into(),
            local_path: "/local".into(),
            progress,
            reply,
        })
        .await
        .unwrap();
        let err = expect_err(reply_rx).await;
        assert!(err.contains("subsystem unavailable"), "got: {err}");

        let (reply, reply_rx) = oneshot::channel();
        tx.send(SftpCommand::Mkdir {
            path: "/tmp/x".into(),
            reply,
        })
        .await
        .unwrap();
        expect_err(reply_rx).await;

        let (reply, reply_rx) = oneshot::channel();
        tx.send(SftpCommand::Rename {
            from: "a".into(),
            to: "b".into(),
            reply,
        })
        .await
        .unwrap();
        expect_err(reply_rx).await;

        let (reply, reply_rx) = oneshot::channel();
        tx.send(SftpCommand::Delete {
            path: "a".into(),
            reply,
        })
        .await
        .unwrap();
        expect_err(reply_rx).await;

        let (progress, _keep) = mpsc::channel(4);
        let (reply, reply_rx) = oneshot::channel();
        tx.send(SftpCommand::Upload {
            transfer_id: 2,
            local_path: "/local".into(),
            remote_path: "/remote".into(),
            progress,
            reply,
        })
        .await
        .unwrap();
        expect_err(reply_rx).await;

        assert_eq!(
            opens.load(Ordering::SeqCst),
            6,
            "every command after a failure must retry the lazy session open"
        );

        // Cancel needs no session — and no reply.
        tx.send(SftpCommand::Cancel { transfer_id: 1 })
            .await
            .unwrap();
        drop(tx);
        timeout(Duration::from_secs(5), task)
            .await
            .expect("task must exit when all senders drop")
            .expect("task panicked");
        assert_eq!(
            opens.load(Ordering::SeqCst),
            6,
            "Cancel and channel close must not open a session"
        );
    }

    #[tokio::test]
    async fn task_exits_only_when_the_last_sender_drops() {
        let (tx, rx) = mpsc::channel::<SftpCommand>(4);
        let tx2 = tx.clone();
        let mut task = tokio::spawn(run_sftp_task(rx, || async {
            Err::<SftpSession, String>("no server".to_string())
        }));

        drop(tx);
        let exited_early = timeout(Duration::from_millis(100), &mut task).await.is_ok();
        assert!(
            !exited_early,
            "task must keep running while any sender is alive"
        );

        drop(tx2);
        timeout(Duration::from_secs(5), task)
            .await
            .expect("task must exit once the last sender drops")
            .expect("task panicked");
    }
}
