//! What the SFTP task and the local file task share: running transfers
//! that `Cancel` and a shutdown can stop, and the copy loop (doc 07
//! §Transfers).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::sftp::commands::Progress;

const TRANSFER_BUF_SIZE: usize = 32 * 1024;

pub(super) fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// One task's running transfers, by id. Each runs on its own tokio task,
/// so `Cancel` and browsing stay responsive. Clones share them.
#[derive(Clone, Default)]
pub(super) struct Transfers(Arc<Mutex<HashMap<u64, CancellationToken>>>);

impl Transfers {
    /// Run `op` as transfer `id` and answer `reply` with its result. A
    /// cancel drops `op` at whatever it awaits: a stalled server must not
    /// hold a cancelled transfer.
    pub(super) fn spawn(
        &self,
        id: u64,
        reply: oneshot::Sender<Result<u64, String>>,
        op: impl Future<Output = Result<u64, String>> + Send + 'static,
    ) {
        let cancel = CancellationToken::new();
        self.lock().insert(id, cancel.clone());
        let transfers = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => Err("transfer cancelled".into()),
                result = op => result,
            };
            transfers.lock().remove(&id);
            let _ = reply.send(result);
        });
    }

    /// Cancel transfer `id`; one that ended, or never started, is left.
    pub(super) fn cancel(&self, id: u64) {
        if let Some(cancel) = self.lock().get(&id) {
            cancel.cancel();
        }
    }

    /// Run `serve` until it ends or `shutdown` fires, which drops it at
    /// whatever it awaits; then cancel every transfer still running.
    pub(super) async fn serve_until(
        &self,
        shutdown: &CancellationToken,
        serve: impl Future<Output = ()>,
    ) {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {}
            () = serve => {}
        }
        self.lock().values().for_each(CancellationToken::cancel);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u64, CancellationToken>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Open the regular file at `path` to send it. Nothing else is one: a
/// directory would fail at its first read, after the target was created.
/// Non-blocking, since opening a FIFO would otherwise wait for its writer
/// on a thread no cancel reaches.
pub(super) async fn open_source(
    path: &str,
) -> Result<(tokio::fs::File, std::fs::Metadata), String> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(text)?;
    let meta = file.metadata().await.map_err(text)?;
    if !meta.is_file() {
        return Err(format!("{path} is not a regular file"));
    }
    Ok((file, meta))
}

/// Copy `src` to `dst` in 32 KiB chunks, reporting progress, until EOF.
/// Returns the byte count.
pub(super) async fn copy_with_progress(
    mut src: impl AsyncRead + Unpin,
    dst: &mut (impl AsyncWrite + Unpin),
    total: u64,
    progress: &mpsc::Sender<Progress>,
) -> Result<u64, String> {
    let mut buf = vec![0u8; TRANSFER_BUF_SIZE];
    let mut transferred = 0u64;
    loop {
        let read = src.read(&mut buf).await.map_err(text)?;
        if read == 0 {
            return Ok(transferred);
        }
        dst.write_all(&buf[..read]).await.map_err(text)?;
        transferred += read as u64;
        let _ = progress.try_send(Progress { transferred, total });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    type Reply = oneshot::Receiver<Result<u64, String>>;

    /// A transfer that never ends by itself.
    fn stuck(transfers: &Transfers, id: u64) -> Reply {
        let (reply, answer) = oneshot::channel();
        transfers.spawn(id, reply, std::future::pending());
        answer
    }

    async fn result_of(answer: Reply) -> Result<u64, String> {
        timeout(Duration::from_secs(5), answer)
            .await
            .expect("the transfer never answered")
            .expect("the transfer ended unanswered")
    }

    #[tokio::test]
    async fn a_cancel_stops_the_transfer_it_names_and_no_other() {
        let transfers = Transfers::default();
        let (first, second) = (stuck(&transfers, 1), stuck(&transfers, 2));
        transfers.cancel(99);
        transfers.cancel(1);
        assert_eq!(result_of(first).await, Err("transfer cancelled".into()));
        assert!(transfers.lock().contains_key(&2), "another one was stopped");
        assert!(!transfers.lock().contains_key(&1), "a finished one is kept");
        transfers.cancel(2);
        assert!(result_of(second).await.is_err());
    }

    #[tokio::test]
    async fn a_finished_transfer_answers_and_is_forgotten() {
        let transfers = Transfers::default();
        let (reply, answer) = oneshot::channel();
        transfers.spawn(7, reply, async { Ok(42) });
        assert_eq!(result_of(answer).await, Ok(42));
        assert!(transfers.lock().is_empty());
    }

    #[tokio::test]
    async fn the_end_of_serving_cancels_every_transfer() {
        for shut_down in [false, true] {
            let transfers = Transfers::default();
            let (first, second) = (stuck(&transfers, 1), stuck(&transfers, 2));
            let shutdown = CancellationToken::new();
            if shut_down {
                shutdown.cancel();
                let serving = transfers.serve_until(&shutdown, std::future::pending());
                timeout(Duration::from_secs(5), serving)
                    .await
                    .expect("a shutdown did not end the serving");
            } else {
                transfers.serve_until(&shutdown, async {}).await;
            }
            assert!(result_of(first).await.is_err());
            assert!(result_of(second).await.is_err());
        }
    }

    /// A writer whose writes never complete, like a stalled server.
    struct StalledWriter;

    impl AsyncWrite for StalledWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn a_cancel_interrupts_a_stalled_write() {
        let transfers = Transfers::default();
        let (progress, _keep) = mpsc::channel(4);
        let (reply, answer) = oneshot::channel();
        transfers.spawn(1, reply, async move {
            copy_with_progress(&b"data"[..], &mut StalledWriter, 4, &progress).await
        });
        tokio::task::yield_now().await;
        transfers.cancel(1);
        assert_eq!(result_of(answer).await, Err("transfer cancelled".into()));
    }

    #[tokio::test]
    async fn only_a_regular_file_is_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, b"four").unwrap();
        let (_, meta) = open_source(&file.to_string_lossy()).await.expect("a file");
        assert_eq!(meta.len(), 4);

        let dir = tmp.path().to_string_lossy().into_owned();
        let err = open_source(&dir).await.expect_err("a directory opened");
        assert_eq!(err, format!("{dir} is not a regular file"));

        // A pipe nobody writes to: the open must not wait for a writer.
        let pipe = tmp.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&pipe).status();
        assert!(made.expect("mkfifo").success());
        let opened = timeout(Duration::from_secs(5), open_source(&pipe.to_string_lossy()))
            .await
            .expect("opening a pipe waited for its writer");
        assert!(opened.is_err(), "a pipe opened as a file");
    }
}
