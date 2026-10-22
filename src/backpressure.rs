//! The GTK half of the try_send + drain-future pattern (doc 02 §Backpressure).
//!
//! GTK-side producers (VTE keystrokes, SFTP sidebar commands) feed bounded
//! tokio channels and must never block the main thread. They send through
//! an [`IdleSender`]: it `try_send`s, parks whatever the channel refuses,
//! and drains the queue from a main-context future that sleeps in
//! `reserve()` until the receiver frees a slot.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use gtk4::glib;
use tokio::sync::mpsc::{self, error::TrySendError};

/// A never-blocking sender into a bounded channel, for the GTK main thread.
/// Items the channel refuses are parked and drained, oldest first, by a
/// future on the main context that waits for capacity and ends once the
/// queue is empty; a new item always queues behind the parked ones, so
/// order holds. Nothing is dropped unless the receiver is gone. Clones
/// share the queue.
pub struct IdleSender<T> {
    /// The bounded channel.
    tx: mpsc::Sender<T>,
    /// Items the channel refused, oldest first.
    parked: Rc<RefCell<VecDeque<T>>>,
    /// Whether a drain future is running.
    drain_pending: Rc<Cell<bool>>,
    /// How often [`Self::flush`] ran, so tests can see it does not poll.
    #[cfg(test)]
    flushes: Rc<Cell<usize>>,
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
            drain_pending: Rc::clone(&self.drain_pending),
            #[cfg(test)]
            flushes: Rc::clone(&self.flushes),
        }
    }
}

impl<T: 'static> IdleSender<T> {
    /// Wrap `tx`.
    pub fn new(tx: mpsc::Sender<T>) -> Self {
        Self {
            tx,
            parked: Rc::default(),
            drain_pending: Rc::default(),
            #[cfg(test)]
            flushes: Rc::default(),
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
                if !self.drain_pending.replace(true) {
                    glib::spawn_future_local(self.clone().drain());
                }
                true
            }
        }
    }

    /// Wait for a free slot, send the oldest parked item into it, and
    /// flush the rest; repeat until the queue is empty or the receiver is
    /// gone. A `send()` between wakeups flushes from the front too, so
    /// order holds; if it emptied the queue, the permit is dropped unused.
    async fn drain(self) {
        loop {
            let Ok(permit) = self.tx.reserve().await else {
                self.parked.borrow_mut().clear();
                break;
            };
            let Some(item) = self.parked.borrow_mut().pop_front() else {
                break;
            };
            permit.send(item);
            if self.flush() != Flush::Full {
                break;
            }
        }
        self.drain_pending.set(false);
    }

    /// Offer the parked items, oldest first, until the channel refuses one.
    fn flush(&self) -> Flush {
        #[cfg(test)]
        self.flushes.set(self.flushes.get() + 1);
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
    use crate::ui::auth_dialog::tests::acquire_default_ctx;

    /// Park `items` as a refused send would, without spawning a drain.
    fn parked(capacity: usize, items: &[u32]) -> (IdleSender<u32>, mpsc::Receiver<u32>) {
        let (tx, rx) = mpsc::channel(capacity);
        let sender = IdleSender::new(tx);
        sender.parked.borrow_mut().extend(items);
        (sender, rx)
    }

    fn received(rx: &mut mpsc::Receiver<u32>) -> Vec<u32> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Dispatch whatever is ready on `ctx` without waiting, and insist it
    /// goes quiet: a busy-polling source would keep it dispatching forever.
    fn settle(ctx: &glib::MainContext) {
        let busy = (0..1000).take_while(|_| ctx.iteration(false)).count();
        assert!(busy < 1000, "the main context never went idle");
    }

    fn parked_items(sender: &IdleSender<u32>) -> Vec<u32> {
        sender.parked.borrow().iter().copied().collect()
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

    #[test]
    fn a_full_channel_sleeps_until_the_receiver_frees_a_slot() {
        let _owned = acquire_default_ctx();
        let ctx = glib::MainContext::default();
        let (sender, mut rx) = parked(1, &[]);
        assert!(sender.send(1));
        assert!(sender.send(2), "parked, not dropped");
        let flushes = sender.flushes.get();
        for _ in 0..100 {
            ctx.iteration(false);
        }
        assert_eq!(
            sender.flushes.get(),
            flushes,
            "no flush while the receiver is not draining"
        );
        assert_eq!(parked_items(&sender), [2]);

        assert_eq!(rx.try_recv().ok(), Some(1));
        settle(&ctx);
        assert_eq!(received(&mut rx), [2], "the freed slot wakes the drain");
        assert!(sender.parked.borrow().is_empty());
        assert!(!sender.drain_pending.get(), "the drain ended");
    }

    #[test]
    fn a_send_while_a_drain_is_pending_queues_behind_it() {
        let _owned = acquire_default_ctx();
        let ctx = glib::MainContext::default();
        let (sender, mut rx) = parked(2, &[]);
        for item in 1..=3 {
            assert!(sender.send(item));
        }
        settle(&ctx);
        assert!(sender.drain_pending.get());
        assert!(sender.send(4));
        assert!(sender.send(5));
        assert_eq!(parked_items(&sender), [3, 4, 5]);

        // Free a slot (the waiting drain's permit), then send before the
        // drain wakes: it must still go out in order, behind 3, 4 and 5.
        let mut got = vec![rx.try_recv().unwrap()];
        assert!(sender.send(6));
        while got.len() < 6 {
            settle(&ctx);
            got.push(rx.try_recv().expect("the drain keeps the channel fed"));
        }
        settle(&ctx);
        assert_eq!(got, [1, 2, 3, 4, 5, 6]);
        assert!(sender.parked.borrow().is_empty());
        assert!(!sender.drain_pending.get());
    }

    #[test]
    fn a_drain_whose_queue_was_emptied_drops_its_permit() {
        let _owned = acquire_default_ctx();
        let ctx = glib::MainContext::default();
        let (sender, mut rx) = parked(3, &[]);
        for item in 1..=4 {
            assert!(sender.send(item));
        }
        settle(&ctx);
        assert_eq!(received(&mut rx), [1, 2, 3]);
        // One slot is the drain's permit; send's own flush takes the rest
        // and empties the queue before the drain wakes.
        assert!(sender.send(5));
        assert!(sender.parked.borrow().is_empty());
        settle(&ctx);
        assert!(!sender.drain_pending.get(), "the drain ended");
        assert_eq!(received(&mut rx), [4, 5], "nothing sent against the permit");
        assert_eq!(sender.tx.capacity(), 3, "the unused permit was returned");
        assert!(sender.send(6));
        assert_eq!(received(&mut rx), [6]);
    }

    #[test]
    fn a_receiver_gone_while_a_drain_is_pending_drops_the_queue() {
        let _owned = acquire_default_ctx();
        let ctx = glib::MainContext::default();
        let (sender, rx) = parked(1, &[]);
        assert!(sender.send(1));
        assert!(sender.send(2));
        settle(&ctx);
        assert!(sender.drain_pending.get());
        drop(rx);
        settle(&ctx);
        assert!(sender.parked.borrow().is_empty());
        assert!(!sender.drain_pending.get(), "the drain stopped");
        assert!(!sender.send(3), "the receiver is gone");
    }
}
