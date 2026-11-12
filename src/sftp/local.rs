//! The file task behind a local-shell tab's sidebar (doc 07 §Local tabs):
//! the same [`SftpCommand`]s, answered from this machine's own files. There
//! is no session to open or lose; `Upload` and `Download` both copy a file.

use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::task::{
    copy_with_progress, format_perms, format_timestamp, lock, sort_entries, until_cancelled,
    TransferRegistry,
};
use crate::sftp::commands::{DirEntry, Progress, SftpCommand};

/// Serve commands until every sender is gone or `shutdown` fires.
pub async fn local_task(mut rx: mpsc::Receiver<SftpCommand>, shutdown: CancellationToken) {
    let transfers = Arc::new(Mutex::new(TransferRegistry::default()));
    tokio::select! {
        biased;
        () = shutdown.cancelled() => {}
        () = serve(&mut rx, &transfers) => {}
    }
    lock(&transfers).cancel_all();
}

async fn serve(rx: &mut mpsc::Receiver<SftpCommand>, transfers: &Arc<Mutex<TransferRegistry>>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            SftpCommand::Cd { path, reply } => {
                let _ = reply.send(list_dir(path).await);
            }
            SftpCommand::Delete { path, reply } => {
                let _ = reply.send(delete(&path).await);
            }
            SftpCommand::Rename { from, to, reply } => {
                let _ = reply.send(rename(&from, &to).await);
            }
            SftpCommand::Mkdir { path, reply } => {
                let _ = reply.send(tokio::fs::create_dir(&path).await.map_err(text));
            }
            SftpCommand::Download {
                transfer_id,
                remote_path,
                local_path,
                progress,
                reply,
            } => spawn_copy(
                transfers,
                transfer_id,
                reply,
                remote_path,
                local_path,
                progress,
            ),
            SftpCommand::Upload {
                transfer_id,
                local_path,
                remote_path,
                progress,
                reply,
            } => spawn_copy(
                transfers,
                transfer_id,
                reply,
                local_path,
                remote_path,
                progress,
            ),
            SftpCommand::Cancel { transfer_id } => {
                lock(transfers).cancel(transfer_id);
            }
        }
    }
}

fn text(err: std::io::Error) -> String {
    err.to_string()
}

/// List `path` in one blocking pass: a directory of thousands is one hop
/// to the blocking pool, not one per entry.
async fn list_dir(path: String) -> Result<Vec<DirEntry>, String> {
    tokio::task::spawn_blocking(move || read_entries(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// A link shows as what it points to, so a linked directory opens; a
/// dangling one shows as itself.
fn read_entries(path: &str) -> Result<Vec<DirEntry>, String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(text)? {
        let entry = entry.map_err(text)?;
        let Ok(meta) = std::fs::metadata(entry.path()).or_else(|_| entry.metadata()) else {
            continue;
        };
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            size: meta.len(),
            modified: u32::try_from(meta.mtime())
                .map(format_timestamp)
                .unwrap_or_default(),
            permissions: format_perms(meta.mode()),
            is_dir: meta.is_dir(),
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// A link is removed, never what it points to.
async fn delete(path: &str) -> Result<(), String> {
    let meta = tokio::fs::symlink_metadata(path).await.map_err(text)?;
    if meta.is_dir() {
        tokio::fs::remove_dir(path).await
    } else {
        tokio::fs::remove_file(path).await
    }
    .map_err(text)
}

/// `rename(2)` replaces its target without a word; an SFTP server refuses,
/// and so does this.
async fn rename(from: &str, to: &str) -> Result<(), String> {
    if tokio::fs::symlink_metadata(to).await.is_ok() {
        return Err(format!("{to} already exists"));
    }
    tokio::fs::rename(from, to).await.map_err(text)
}

/// Run a copy on its own task, registered for `Cancel`.
fn spawn_copy(
    transfers: &Arc<Mutex<TransferRegistry>>,
    transfer_id: u64,
    reply: oneshot::Sender<Result<u64, String>>,
    from: String,
    to: String,
    progress: mpsc::Sender<Progress>,
) {
    let cancel = lock(transfers).register(transfer_id);
    let transfers = Arc::clone(transfers);
    tokio::spawn(async move {
        let result = until_cancelled(&cancel, copy_file(&from, &to, &progress)).await;
        lock(&transfers).finish(transfer_id);
        let _ = reply.send(result);
    });
}

/// Copy the regular file `from` over `to`, keeping its mode. Copying a
/// file onto itself is refused: creating the target would empty the source.
async fn copy_file(from: &str, to: &str, progress: &mpsc::Sender<Progress>) -> Result<u64, String> {
    let source = tokio::fs::File::open(from).await.map_err(text)?;
    let meta = source.metadata().await.map_err(text)?;
    if !meta.is_file() {
        return Err(format!("{from} is not a regular file"));
    }
    if let Ok(existing) = tokio::fs::metadata(to).await {
        if existing.dev() == meta.dev() && existing.ino() == meta.ino() {
            return Err(format!("{from} and {to} are the same file"));
        }
    }
    let mut target = tokio::fs::File::create(to).await.map_err(text)?;
    let copied = copy_with_progress(source, &mut target, meta.len(), progress).await?;
    // tokio writes in the background: only the flush reports a failed last
    // write (a full disk).
    target.flush().await.map_err(text)?;
    let _ = target.set_permissions(meta.permissions()).await;
    Ok(copied)
}

#[cfg(test)]
mod tests;
