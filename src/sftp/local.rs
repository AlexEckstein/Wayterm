//! The file task behind a local-shell tab's sidebar (doc 07 §Local tabs):
//! the same [`SftpCommand`]s, answered from this machine's own files. There
//! is no session to open or lose; `Upload` and `Download` both copy a file.

use std::os::unix::fs::MetadataExt;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::sftp::commands::{DirEntry, Progress, SftpCommand};
use crate::sftp::listing::sort_entries;
use crate::sftp::transfer::{copy_with_progress, open_source, text, Transfers};

/// Serve commands until every sender is gone or `shutdown` fires.
pub async fn local_task(mut rx: mpsc::Receiver<SftpCommand>, shutdown: CancellationToken) {
    let transfers = Transfers::default();
    let serving = serve(&mut rx, &transfers);
    transfers.serve_until(&shutdown, serving).await;
}

async fn serve(rx: &mut mpsc::Receiver<SftpCommand>, transfers: &Transfers) {
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
            // Either way a copy, from the first path to the second.
            SftpCommand::Download {
                transfer_id,
                remote_path: from,
                local_path: to,
                progress,
                reply,
            }
            | SftpCommand::Upload {
                transfer_id,
                local_path: from,
                remote_path: to,
                progress,
                reply,
            } => transfers.spawn(transfer_id, reply, async move {
                copy_file(&from, &to, &progress).await
            }),
            SftpCommand::Cancel { transfer_id } => transfers.cancel(transfer_id),
        }
    }
}

/// List `path` in one blocking pass: a directory of thousands is one hop
/// to the blocking pool, not one per entry.
async fn list_dir(path: String) -> Result<Vec<DirEntry>, String> {
    tokio::task::spawn_blocking(move || read_entries(&path))
        .await
        .map_err(text)?
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
        entries.push(DirEntry::new(
            entry.file_name().to_string_lossy().into_owned(),
            meta.len(),
            u32::try_from(meta.mtime()).ok(),
            Some(meta.mode()),
            meta.is_dir(),
        ));
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

/// Copy the regular file `from` to `to`, as `cp` does: a new file gets the
/// source's mode, one already there is replaced and keeps its own. Neither
/// end may be a pipe or a device, and a file is never copied onto itself:
/// creating the target would empty the source.
async fn copy_file(from: &str, to: &str, progress: &mpsc::Sender<Progress>) -> Result<u64, String> {
    let (source, meta) = open_source(from).await?;
    if let Ok(existing) = tokio::fs::metadata(to).await {
        if !existing.is_file() {
            return Err(format!("{to} is not a regular file"));
        }
        if existing.dev() == meta.dev() && existing.ino() == meta.ino() {
            return Err(format!("{from} and {to} are the same file"));
        }
    }
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(meta.mode() & 0o777)
        .open(to)
        .await
        .map_err(text)?;
    let copied = copy_with_progress(source, &mut target, meta.len(), progress).await?;
    // tokio writes in the background: only the flush reports a failed last
    // write (a full disk).
    target.flush().await.map_err(text)?;
    Ok(copied)
}

#[cfg(test)]
mod tests;
