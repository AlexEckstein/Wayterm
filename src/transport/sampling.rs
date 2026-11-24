//! Polls a local sample on tokio's blocking pool, so a slow read (a hung
//! network mount) never stalls the GTK thread.

use std::time::Duration;

use tokio::sync::mpsc;

/// Send `sample`'s result to `out` now and then every `interval`, until
/// `out`'s receiver is dropped. A sample that returns `None` is skipped.
pub(crate) async fn poll_local<T: Send + 'static>(
    interval: Duration,
    sample: fn() -> Option<T>,
    out: mpsc::Sender<T>,
) {
    loop {
        let Ok(taken) = tokio::task::spawn_blocking(sample).await else {
            return;
        };
        if let Some(item) = taken
            && out.send(item).await.is_err()
        {
            return;
        }
        tokio::select! {
            _ = out.closed() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}
