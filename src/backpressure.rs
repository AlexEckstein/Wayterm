//! The GTK half of the try_send + idle-flush pattern (doc 02 §Backpressure).
//!
//! GTK-side producers (VTE keystrokes, SFTP sidebar commands) feed bounded
//! tokio channels and must never block the main thread. They `try_send`
//! first; whatever the channel refuses is parked in a [`PendingQueue`] and
//! re-offered from an idle callback until it drains.

use std::collections::VecDeque;

use tokio::sync::mpsc::error::TrySendError;

/// Outcome of one [`PendingQueue::drain_with`] pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrainReport {
    /// Items still parked because the channel filled up again.
    pub remaining: usize,
    /// The receiver is gone; everything still parked was dropped.
    pub closed: bool,
}

/// Items waiting for room in a bounded channel, in send order.
#[derive(Debug)]
pub struct PendingQueue<T> {
    queue: VecDeque<T>,
}

impl<T> Default for PendingQueue<T> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }
}

impl<T> PendingQueue<T> {
    pub fn push(&mut self, item: T) {
        self.queue.push_back(item);
    }

    pub fn clear(&mut self) {
        self.queue.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Offer every parked item to `send`, oldest first. Stops at the first
    /// item the channel refuses and keeps it (and everything behind it);
    /// a closed channel drops the rest.
    pub fn drain_with(
        &mut self,
        mut send: impl FnMut(T) -> Result<(), TrySendError<T>>,
    ) -> DrainReport {
        while let Some(item) = self.queue.pop_front() {
            match send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(item)) => {
                    self.queue.push_front(item);
                    break;
                }
                Err(TrySendError::Closed(_)) => {
                    self.queue.clear();
                    return DrainReport {
                        remaining: 0,
                        closed: true,
                    };
                }
            }
        }
        DrainReport {
            remaining: self.queue.len(),
            closed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(items: &[u32]) -> PendingQueue<u32> {
        let mut queue = PendingQueue::default();
        items.iter().for_each(|&item| queue.push(item));
        queue
    }

    #[test]
    fn drains_in_order() {
        let mut sent = Vec::new();
        let report = queue(&[1, 2]).drain_with(|item| {
            sent.push(item);
            Ok(())
        });
        assert_eq!(sent, [1, 2]);
        assert_eq!(report, DrainReport::default());
    }

    #[test]
    fn stops_at_a_full_channel_and_keeps_the_rest_in_order() {
        let mut queue = queue(&[1, 2, 3]);
        let mut offered = Vec::new();
        let report = queue.drain_with(|item| {
            offered.push(item);
            if item == 1 {
                Ok(())
            } else {
                Err(TrySendError::Full(item))
            }
        });
        assert_eq!(offered, [1, 2], "3 is never offered after 2 is refused");
        assert_eq!(report.remaining, 2);

        let mut sent = Vec::new();
        queue.drain_with(|item| {
            sent.push(item);
            Ok(())
        });
        assert_eq!(sent, [2, 3], "a later pass finishes in order");
        assert!(queue.is_empty());
    }

    #[test]
    fn a_closed_channel_drops_everything() {
        let mut queue = queue(&[1, 2]);
        let report = queue.drain_with(|item| Err(TrySendError::Closed(item)));
        assert_eq!(
            report,
            DrainReport {
                remaining: 0,
                closed: true
            }
        );
        assert!(queue.is_empty());
    }
}
