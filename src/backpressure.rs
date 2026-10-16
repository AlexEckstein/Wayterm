//! The GTK half of the try_send + idle-flush pattern (doc 02 §Backpressure).
//!
//! GTK-side producers (VTE keystrokes, SFTP sidebar commands) feed bounded
//! tokio channels and must never block the main thread. They send through
//! an [`IdleSender`]: it `try_send`s, parks whatever the channel refuses,
//! and re-offers it from an idle callback until the queue drains.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use gtk4::glib;
use tokio::sync::mpsc::{self, error::TrySendError};

/// A never-blocking sender into a bounded channel, for the GTK main thread.
/// Items the channel refuses are parked and flushed, oldest first, from an
/// idle callback that removes itself once the queue drains; a new item
/// always queues behind the parked ones, so order holds. Nothing is
/// dropped unless the receiver is gone. Clones share the queue.
pub struct IdleSender<T> {
    tx: mpsc::Sender<T>,
    parked: Rc<RefCell<VecDeque<T>>>,
    flush_scheduled: Rc<Cell<bool>>,
}

/// What one flush left behind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flush {
    /// Everything parked was sent.
    Drained,
    /// The channel filled up again; the rest stays parked.
    Full,
    /// The receiver is gone; everything parked was dropped.
    Closed,
}

impl<T> Clone for IdleSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            parked: Rc::clone(&self.parked),
            flush_scheduled: Rc::clone(&self.flush_scheduled),
        }
    }
}

impl<T: 'static> IdleSender<T> {
    pub fn new(tx: mpsc::Sender<T>) -> Self {
        Self {
            tx,
            parked: Rc::default(),
            flush_scheduled: Rc::default(),
        }
    }

    /// Send `item` behind anything parked, parking what does not fit.
    /// `false` once the receiver is gone, which drops the item.
    pub fn send(&self, item: T) -> bool {
        self.parked.borrow_mut().push_back(item);
        match self.flush() {
            Flush::Drained => true,
            Flush::Closed => false,
            Flush::Full => {
                if !self.flush_scheduled.replace(true) {
                    let this = self.clone();
                    glib::idle_add_local(move || match this.flush() {
                        Flush::Full => glib::ControlFlow::Continue,
                        Flush::Drained | Flush::Closed => {
                            this.flush_scheduled.set(false);
                            glib::ControlFlow::Break
                        }
                    });
                }
                true
            }
        }
    }

    /// Offer the parked items, oldest first, until the channel refuses one.
    fn flush(&self) -> Flush {
        let mut parked = self.parked.borrow_mut();
        while let Some(item) = parked.pop_front() {
            match self.tx.try_send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(item)) => {
                    parked.push_front(item);
                    return Flush::Full;
                }
                Err(TrySendError::Closed(_)) => {
                    parked.clear();
                    return Flush::Closed;
                }
            }
        }
        Flush::Drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Park `items` as a refused send would (no idle callback in tests).
    fn parked(capacity: usize, items: &[u32]) -> (IdleSender<u32>, mpsc::Receiver<u32>) {
        let (tx, rx) = mpsc::channel(capacity);
        let sender = IdleSender::new(tx);
        sender.parked.borrow_mut().extend(items);
        (sender, rx)
    }

    fn received(rx: &mut mpsc::Receiver<u32>) -> Vec<u32> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn a_send_with_room_goes_straight_through() {
        let (sender, mut rx) = parked(4, &[]);
        assert!(sender.send(1));
        assert!(sender.send(2));
        assert_eq!(received(&mut rx), [1, 2]);
    }

    #[test]
    fn a_full_channel_parks_the_rest_until_a_flush_finds_room() {
        let (sender, mut rx) = parked(1, &[1, 2, 3]);
        assert_eq!(sender.flush(), Flush::Full);
        assert_eq!(
            received(&mut rx),
            [1],
            "2 is refused, so 3 is never offered"
        );
        assert_eq!(sender.flush(), Flush::Full);
        assert_eq!(received(&mut rx), [2]);
        assert_eq!(sender.flush(), Flush::Drained);
        assert_eq!(received(&mut rx), [3], "a later pass finishes in order");
    }

    #[test]
    fn a_closed_channel_drops_everything_parked() {
        let (sender, rx) = parked(4, &[1, 2]);
        drop(rx);
        assert_eq!(sender.flush(), Flush::Closed);
        assert!(sender.parked.borrow().is_empty());
        assert!(!sender.send(3), "the receiver is gone");
    }
}
