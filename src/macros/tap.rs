//! [`InputTap`] (doc 12 §Recording): a relay spliced between a terminal's
//! event channel and its transport pump. Every event passes through
//! unchanged and in order; while a recording is armed, each `DataOut` is
//! also timestamped into its [`Recorder`]. The relay sees whatever a
//! backend sends — keystrokes, pastes, replayed macros — so recording
//! needs nothing from the backend itself.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gtk4::glib;
use tokio::sync::mpsc;

use crate::macros::model::MacroStep;
use crate::macros::recorder::Recorder;
use crate::terminal::{TerminalEvent, TerminalHandle};

/// The relay's channel to the pump: the terminal channel's capacity.
const RELAY_CAPACITY: usize = 256;

/// One terminal's recording switch. Clones share the recording.
#[derive(Clone, Default)]
pub struct InputTap {
    /// The armed recording, if any.
    recording: Rc<RefCell<Option<Recorder>>>,
}

impl InputTap {
    /// Splice a tap into `terminal`'s event path; the returned handle goes
    /// to the pump in its place. The relay runs on `main_ctx` and awaits
    /// room in the pump's channel, so backpressure still reaches the
    /// backend's queue (doc 02 §Backpressure). It ends with the pump.
    pub fn wrap(terminal: TerminalHandle, main_ctx: &glib::MainContext) -> (TerminalHandle, Self) {
        let TerminalHandle {
            backend,
            events: mut upstream,
        } = terminal;
        let (downstream, events) = mpsc::channel(RELAY_CAPACITY);
        let tap = Self::default();
        let recording = Rc::clone(&tap.recording);
        main_ctx.spawn_local(async move {
            while let Some(event) = upstream.recv().await {
                if let TerminalEvent::DataOut(data) = &event {
                    if let Some(recorder) = recording.borrow_mut().as_mut() {
                        recorder.record(data, Instant::now());
                    }
                }
                if downstream.send(event).await.is_err() {
                    break;
                }
            }
        });
        (TerminalHandle { backend, events }, tap)
    }

    /// Start a fresh recording, dropping any in progress.
    pub fn start_recording(&self) {
        *self.recording.borrow_mut() = Some(Recorder::new());
    }

    /// Whether a recording is armed.
    pub fn is_recording(&self) -> bool {
        self.recording.borrow().is_some()
    }

    /// Stop recording and return the steps; `None` when not recording.
    pub fn stop_recording(&self) -> Option<Vec<MacroStep>> {
        self.recording.borrow_mut().take().map(Recorder::finish)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::terminal::FakeBackend;

    fn tapped(ctx: &glib::MainContext) -> (mpsc::Sender<TerminalEvent>, TerminalHandle, InputTap) {
        let (tx, events) = mpsc::channel(8);
        let terminal = TerminalHandle {
            backend: Arc::new(FakeBackend::new()),
            events,
        };
        let (handle, tap) = InputTap::wrap(terminal, ctx);
        (tx, handle, tap)
    }

    fn spin(ctx: &glib::MainContext) {
        for _ in 0..20 {
            ctx.iteration(false);
        }
    }

    fn data(bytes: &[u8]) -> TerminalEvent {
        TerminalEvent::DataOut(bytes.to_vec())
    }

    #[test]
    fn events_pass_through_in_order_and_only_armed_input_is_recorded() {
        let ctx = glib::MainContext::new();
        let (tx, mut handle, tap) = tapped(&ctx);
        assert!(!tap.is_recording());

        tx.try_send(data(b"before")).unwrap();
        spin(&ctx);
        tap.start_recording();
        assert!(tap.is_recording());
        tx.try_send(data(b"l")).unwrap();
        tx.try_send(TerminalEvent::SizeChange { rows: 24, cols: 80 })
            .unwrap();
        tx.try_send(data(b"s")).unwrap();
        spin(&ctx);
        let steps = tap.stop_recording().expect("was recording");
        tx.try_send(data(b"after")).unwrap();
        spin(&ctx);

        let keys: Vec<u8> = steps.iter().flat_map(|s| s.keys.clone()).collect();
        assert_eq!(keys, b"ls", "only what was typed while armed");
        assert_eq!(tap.stop_recording(), None);

        let mut forwarded = Vec::new();
        while let Ok(event) = handle.events.try_recv() {
            forwarded.push(event);
        }
        assert_eq!(
            forwarded,
            [
                data(b"before"),
                data(b"l"),
                TerminalEvent::SizeChange { rows: 24, cols: 80 },
                data(b"s"),
                data(b"after"),
            ]
        );
    }

    #[test]
    fn the_relay_ends_with_the_pump() {
        let ctx = glib::MainContext::new();
        let (tx, handle, _tap) = tapped(&ctx);
        drop(handle);
        tx.try_send(data(b"x")).unwrap();
        spin(&ctx);
        assert!(tx.is_closed(), "the backend's sends now fail fast");
    }
}
