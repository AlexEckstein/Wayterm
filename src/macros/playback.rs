//! Playback (doc 12 §Playback): each step waits its pause, then goes to
//! the terminal through [`TerminalBackend::send_input`], the path a
//! keystroke takes. It runs on the GTK main context and holds the
//! terminal weakly, so a closed tab ends it rather than being kept alive.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use gtk4::glib;

use crate::macros::model::MacroStep;
use crate::terminal::TerminalBackend;

/// How a playback ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackEnd {
    /// Every step was sent.
    Finished,
    /// [`Playback::stop`] was called first.
    Stopped,
    /// The terminal went away first (its tab closed or reconnected).
    TerminalGone,
}

/// The end-of-playback callback, run once.
type OnDone = Rc<RefCell<Option<Box<dyn FnOnce(PlaybackEnd)>>>>;

/// A macro playing into one terminal. Dropping the handle lets it run
/// on; [`stop`](Self::stop) ends it.
pub struct Playback {
    /// The playback future.
    task: glib::JoinHandle<()>,
    /// Called once with how it ended.
    on_done: OnDone,
}

impl Playback {
    /// Play `steps` into `terminal` on `main_ctx`. `on_done` runs exactly
    /// once, however playback ends.
    pub fn start(
        main_ctx: &glib::MainContext,
        steps: Vec<MacroStep>,
        terminal: &Arc<dyn TerminalBackend>,
        on_done: impl FnOnce(PlaybackEnd) + 'static,
    ) -> Self {
        let on_done: OnDone = Rc::new(RefCell::new(Some(Box::new(on_done))));
        let terminal = Arc::downgrade(terminal);
        let done = Rc::clone(&on_done);
        let task = main_ctx.spawn_local(async move {
            let end = play(steps, terminal).await;
            finish(&done, end);
        });
        Self { task, on_done }
    }

    /// Stop before the next step. A no-op once playback has ended.
    pub fn stop(&self) {
        self.task.abort();
        finish(&self.on_done, PlaybackEnd::Stopped);
    }
}

/// Send each step after its pause, until done or the terminal is gone.
async fn play(steps: Vec<MacroStep>, terminal: Weak<dyn TerminalBackend>) -> PlaybackEnd {
    for step in steps {
        if step.delay_ms > 0 {
            glib::timeout_future(step.delay()).await;
        }
        let Some(terminal) = terminal.upgrade() else {
            return PlaybackEnd::TerminalGone;
        };
        if !step.keys.is_empty() {
            terminal.send_input(&step.keys);
        }
    }
    PlaybackEnd::Finished
}

/// Run the callback with `end`, once.
fn finish(on_done: &OnDone, end: PlaybackEnd) {
    let callback = on_done.borrow_mut().take();
    if let Some(callback) = callback {
        callback(end);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::terminal::fake::{FakeBackend, Interaction};

    fn step(delay_ms: u32, keys: &[u8]) -> MacroStep {
        MacroStep {
            delay_ms,
            keys: keys.to_vec(),
        }
    }

    /// Iterate `ctx` until `ready()`, for at most two seconds.
    fn drive(ctx: &glib::MainContext, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out");
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn sent(fake: &FakeBackend) -> Vec<Vec<u8>> {
        fake.interactions()
            .into_iter()
            .filter_map(|i| match i {
                Interaction::SendInput(bytes) => Some(bytes),
                _ => None,
            })
            .collect()
    }

    type EndCell = Rc<Cell<Option<PlaybackEnd>>>;

    fn ended() -> (EndCell, impl FnOnce(PlaybackEnd)) {
        let cell = EndCell::default();
        let set = Rc::clone(&cell);
        (cell, move |end| set.set(Some(end)))
    }

    #[test]
    fn steps_go_out_in_order_after_their_pauses() {
        let ctx = glib::MainContext::new();
        let fake = FakeBackend::new();
        let terminal: Arc<dyn TerminalBackend> = Arc::new(fake.clone());
        let (end, on_done) = ended();
        let started = Instant::now();
        let _playback = Playback::start(
            &ctx,
            vec![step(0, b"l"), step(30, b"s"), step(0, b""), step(20, b"\r")],
            &terminal,
            on_done,
        );
        drive(&ctx, || end.get().is_some());
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "pauses kept"
        );
        assert_eq!(end.get(), Some(PlaybackEnd::Finished));
        assert_eq!(sent(&fake), [b"l".to_vec(), b"s".to_vec(), b"\r".to_vec()]);
    }

    #[test]
    fn stop_ends_a_playback_mid_pause_and_reports_once() {
        let ctx = glib::MainContext::new();
        let fake = FakeBackend::new();
        let terminal: Arc<dyn TerminalBackend> = Arc::new(fake.clone());
        let (end, on_done) = ended();
        let playback = Playback::start(
            &ctx,
            vec![step(0, b"a"), step(60_000, b"b")],
            &terminal,
            on_done,
        );
        drive(&ctx, || !sent(&fake).is_empty());
        playback.stop();
        playback.stop();
        assert_eq!(end.get(), Some(PlaybackEnd::Stopped));
        for _ in 0..20 {
            ctx.iteration(false);
        }
        assert_eq!(sent(&fake), [b"a".to_vec()]);
    }

    #[test]
    fn a_dropped_terminal_ends_playback() {
        let ctx = glib::MainContext::new();
        let terminal: Arc<dyn TerminalBackend> = Arc::new(FakeBackend::new());
        let (end, on_done) = ended();
        let _playback = Playback::start(&ctx, vec![step(10, b"a")], &terminal, on_done);
        drop(terminal);
        drive(&ctx, || end.get().is_some());
        assert_eq!(end.get(), Some(PlaybackEnd::TerminalGone));
    }
}
