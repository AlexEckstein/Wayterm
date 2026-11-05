//! The timers behind the cursor's blink and blinking text.

use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use super::cursor::needs_blink_timer;
use super::GhosttyView;
use crate::terminal::ghostty::engine::FrameInfo;

/// How long a blinking cursor stays on, and off; Ghostty's default.
pub(super) const BLINK_INTERVAL: Duration = Duration::from_millis(600);

/// How long blinking text stays on, and off: one cycle a second, SGR 5's
/// slow blink. SGR 6 blinks the same, as libghostty-vt keeps one flag.
pub(super) const BLINK_PHASE: Duration = Duration::from_millis(500);

impl GhosttyView {
    /// Start the blink timer when the frame needs it and stop it when not;
    /// the paint after a map or a focus change is what starts it again.
    pub(super) fn sync_blink(&self, info: &FrameInfo) {
        let imp = self.imp();
        if !needs_blink_timer(info, imp.focused.get(), self.is_mapped()) {
            self.stop_blink();
        } else if imp.blink.borrow().is_none() {
            let view = self.downgrade();
            let tick = glib::timeout_add_local(BLINK_INTERVAL, move || {
                let Some(view) = view.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                view.blink_tick();
                glib::ControlFlow::Continue
            });
            imp.blink.replace(Some(tick));
        }
    }

    /// Flip the blink phase; the next paint draws it, whole, like any other
    /// change.
    pub(super) fn blink_tick(&self) {
        let off = &self.imp().blink_off;
        off.set(!off.get());
        self.queue_draw();
    }

    /// Stop the timer, leaving the cursor on.
    pub(super) fn stop_blink(&self) {
        if let Some(tick) = self.imp().blink.take() {
            tick.remove();
        }
        self.imp().blink_off.set(false);
    }

    /// Blinking text changes phase.
    pub(super) fn text_blink_tick(&self) {
        let off = &self.imp().text_blink_off;
        off.set(!off.get());
        self.queue_draw();
    }

    /// Run the timer while a mapped view shows blinking text, `blinks`
    /// saying if this frame does; otherwise stop it.
    pub(super) fn follow_blink(&self, blinks: bool) {
        if !blinks || !self.is_mapped() {
            self.stop_text_blink();
            return;
        }
        let mut timer = self.imp().text_blink_timer.borrow_mut();
        if timer.is_none() {
            let view = self.downgrade();
            *timer = Some(glib::timeout_add_local(BLINK_PHASE, move || {
                let Some(view) = view.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                view.text_blink_tick();
                glib::ControlFlow::Continue
            }));
        }
    }

    /// Stop the timer; text that blinks next starts on.
    pub(super) fn stop_text_blink(&self) {
        if let Some(timer) = self.imp().text_blink_timer.take() {
            timer.remove();
        }
        self.imp().text_blink_off.set(false);
    }
}
