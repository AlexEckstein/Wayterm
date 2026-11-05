//! Keys, mouse, wheel and focus.
//!
//! Input: keys go to the input method (see [`super::im`]), then through
//! libghostty-vt's key encoder (so cursor-key and Kitty keyboard modes
//! apply) and leave as `DataOut`; Shift+PageUp/
//! PageDown/Home/End scroll the viewport as in VTE; Ctrl+Shift+Up/Down
//! jump between OSC 133 shell prompts and Ctrl+Shift+O selects a
//! command's output. Mouse events go where
//! [`route`] says: to the program through the mouse encoder while it
//! tracks the mouse (every button, and the wheel on both axes), as arrow
//! keys for the wheel under alternate scroll, else local, where a
//! primary-button drag selects (by word after a double click, by line after
//! a triple), a finished selection goes to PRIMARY, a middle click pastes
//! PRIMARY, and the wheel scrolls the viewport three rows per notch (smooth
//! steps accumulate). Shift keeps them local. Focus
//! changes are reported when the program asks. A press or a typed key ends
//! the search, as it clears the selection.

use std::cell::Cell;

use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib};

use super::links::link_at;
use super::{px, GhosttyView};
use crate::terminal::ghostty::engine::{MouseEvent, MouseModes, Scroll};
use crate::terminal::ghostty::{ffi, Shared};

/// The arrow keysyms alternate scroll sends.
const ARROW_UP: u32 = 0xff52;
const ARROW_DOWN: u32 = 0xff54;

impl GhosttyView {
    pub(super) fn install_input(&self) {
        let keys = gtk4::EventControllerKey::new();
        let view = self.downgrade();
        keys.connect_key_pressed(move |keys, keyval, _, state| {
            let handled = view
                .upgrade()
                .is_some_and(|view| view.key_event(keyval, state, || view.im_filter(keys)));
            if handled {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        let view = self.downgrade();
        keys.connect_key_released(move |keys, _, _, _| {
            if let Some(view) = view.upgrade() {
                view.im_filter(keys);
            }
        });
        self.add_controller(keys);

        let drag = gtk4::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let view = self.downgrade();
        drag.connect_drag_begin(move |drag, x, y| {
            if let Some(view) = view.upgrade() {
                view.grab_focus();
                view.press(
                    drag.current_event_state(),
                    drag.current_event_time(),
                    (x, y),
                );
            }
        });
        let view = self.downgrade();
        drag.connect_drag_update(move |drag, dx, dy| {
            if let (Some(view), Some((x, y))) = (view.upgrade(), drag.start_point()) {
                view.drag_to(drag.current_event_state(), (x + dx, y + dy));
            }
        });
        let view = self.downgrade();
        drag.connect_drag_end(move |drag, dx, dy| {
            if let Some(view) = view.upgrade() {
                let at = view.drag_end_point(drag, dx, dy);
                view.release(drag.current_event_state(), at);
            }
        });
        self.add_controller(drag);

        let middle = gtk4::GestureClick::new();
        middle.set_button(gdk::BUTTON_MIDDLE);
        let view = self.downgrade();
        middle.connect_pressed(move |middle, _, _, _| {
            if let Some(view) = view.upgrade() {
                view.grab_focus();
                view.paste_primary(middle.current_event_state());
            }
        });
        self.add_controller(middle);

        for (gdk_button, button) in [
            (gdk::BUTTON_MIDDLE, ffi::BUTTON_MIDDLE),
            (gdk::BUTTON_SECONDARY, ffi::BUTTON_RIGHT),
        ] {
            let drag = gtk4::GestureDrag::new();
            drag.set_button(gdk_button);
            let view = self.downgrade();
            drag.connect_drag_begin(move |drag, x, y| {
                if let Some(view) = view.upgrade() {
                    view.button_press(button, drag.current_event_state(), (x, y));
                }
            });
            let view = self.downgrade();
            drag.connect_drag_update(move |drag, dx, dy| {
                if let (Some(view), Some((x, y))) = (view.upgrade(), drag.start_point()) {
                    view.button_drag(button, drag.current_event_state(), (x + dx, y + dy));
                }
            });
            let view = self.downgrade();
            drag.connect_drag_end(move |drag, dx, dy| {
                if let Some(view) = view.upgrade() {
                    let at = view.drag_end_point(drag, dx, dy);
                    view.button_release(button, drag.current_event_state(), at);
                }
            });
            self.add_controller(drag);
        }

        let motion = gtk4::EventControllerMotion::new();
        let view = self.downgrade();
        motion.connect_motion(move |motion, x, y| {
            if let Some(view) = view.upgrade() {
                view.hover(motion.current_event_state(), (x, y));
            }
        });
        let view = self.downgrade();
        motion.connect_enter(move |motion, x, y| {
            if let Some(view) = view.upgrade() {
                view.hover(motion.current_event_state(), (x, y));
            }
        });
        let view = self.downgrade();
        motion.connect_leave(move |_| {
            if let Some(view) = view.upgrade() {
                view.pointer_left();
            }
        });
        self.add_controller(motion);

        let focus = gtk4::EventControllerFocus::new();
        let view = self.downgrade();
        focus.connect_enter(move |_| {
            if let Some(view) = view.upgrade() {
                view.focus_changed(true);
            }
        });
        let view = self.downgrade();
        focus.connect_leave(move |_| {
            if let Some(view) = view.upgrade() {
                view.focus_changed(false);
            }
        });
        self.add_controller(focus);

        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        let view = self.downgrade();
        wheel.connect_scroll(move |wheel, dx, dy| {
            let Some(view) = view.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let (unit, state) = (wheel.unit(), wheel.current_event_state());
            let reported = view.wheel_sideways(dx, unit, state);
            view.wheel(dy, unit, state);
            if reported || dy != 0.0 {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.add_controller(wheel);
    }

    /// Primary button down at widget pixels `at`, `time` in milliseconds:
    /// report it, or start a selection. The whole drag goes the same way.
    pub(super) fn press(&self, state: gdk::ModifierType, time: u32, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        let imp = self.imp();
        let route = route(shared.engine.mouse_modes(), state, false);
        imp.drag.set(Some(route));
        imp.pointer.set(at);
        shared.engine.clear_selection();
        shared.engine.clear_search();
        if route == Route::Report {
            shared.engine.selection_cancel();
            self.report(&shared, ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, state, at);
        } else {
            let repeat_ms = self.settings().gtk_double_click_time().unsigned_abs();
            let repeat_px = f64::from(self.cell_size().0);
            let cell = self.cell_at(at.0, at.1);
            let link = state
                .contains(gdk::ModifierType::CONTROL_MASK)
                .then(|| link_at(&imp.frame.borrow(), cell.1.into(), cell.0.into()));
            imp.pressed_link.replace(link.flatten());
            shared
                .engine
                .selection_press(cell, at, time, repeat_ms, repeat_px);
        }
        self.queue_draw();
    }

    /// The primary button dragged to `at`.
    pub(super) fn drag_to(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        let imp = self.imp();
        imp.pointer.set(at);
        match imp.drag.get() {
            Some(Route::Report) => self.report_motion(&shared, ffi::BUTTON_LEFT, state, at),
            Some(_) => {
                let cell = self.cell_at(at.0, at.1);
                let cell_w = px(self.cell_size().0);
                shared
                    .engine
                    .selection_drag(cell, at, cell_w, px(self.height()));
                self.queue_draw();
            }
            None => {}
        }
    }

    /// The primary button released at `at`.
    pub(super) fn release(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        match self.imp().drag.take() {
            Some(Route::Report) => {
                self.report(&shared, ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, state, at);
            }
            Some(_) => {
                let cell = self.cell_at(at.0, at.1);
                shared.engine.selection_release(cell);
                self.copy_primary();
                self.open_pressed_link(&shared, cell);
            }
            None => {}
        }
    }

    /// Where a drag ended. A second button down resets the gesture, which
    /// ends the drag with its start point already gone: the pointer's then.
    fn drag_end_point(&self, drag: &gtk4::GestureDrag, dx: f64, dy: f64) -> (f64, f64) {
        drag.start_point()
            .map_or(self.imp().pointer.get(), |(x, y)| (x + dx, y + dy))
    }

    /// Where the middle or right button's drag in progress goes.
    fn button_route(&self, button: u32) -> &Cell<Option<Route>> {
        let imp = self.imp();
        if button == ffi::BUTTON_MIDDLE {
            &imp.middle_drag
        } else {
            &imp.right_drag
        }
    }

    /// Whether any button is down.
    pub(super) fn button_held(&self) -> bool {
        let imp = self.imp();
        [&imp.drag, &imp.middle_drag, &imp.right_drag]
            .iter()
            .any(|drag| drag.get().is_some())
    }

    /// Middle or right button down at `at`: report it, as for the primary.
    /// The whole drag goes the same way; a local one does nothing here.
    pub(super) fn button_press(&self, button: u32, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        let route = route(shared.engine.mouse_modes(), state, false);
        self.button_route(button).set(Some(route));
        self.imp().pointer.set(at);
        if route == Route::Report {
            self.grab_focus();
            shared.engine.clear_selection();
            shared.engine.clear_search();
            shared.engine.selection_cancel();
            self.report(&shared, ffi::MOUSE_PRESS, button, state, at);
        }
    }

    /// The middle or right button dragged to `at`.
    pub(super) fn button_drag(&self, button: u32, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        self.imp().pointer.set(at);
        if self.button_route(button).get() == Some(Route::Report) {
            self.report_motion(&shared, button, state, at);
        }
    }

    /// The middle or right button released at `at`.
    pub(super) fn button_release(&self, button: u32, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        if self.button_route(button).take() == Some(Route::Report) {
            self.report(&shared, ffi::MOUSE_RELEASE, button, state, at);
        }
    }

    /// Put the selection on PRIMARY; without one, PRIMARY keeps its text.
    pub(in crate::terminal::ghostty) fn copy_primary(&self) {
        let text = self.shared().and_then(|s| s.engine.selection_text());
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.primary_clipboard().set_text(&text);
        }
    }

    /// Middle button down: paste PRIMARY, bracketed when the program
    /// enabled mode 2004, unless the program tracks the mouse.
    pub(super) fn paste_primary(&self, state: gdk::ModifierType) {
        let Some(shared) = self.shared() else { return };
        if route(shared.engine.mouse_modes(), state, false) != Route::Local {
            return;
        }
        let clipboard = self.primary_clipboard();
        let view = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Ok(Some(text)) = clipboard.read_text_future().await else {
                return;
            };
            if let Some(view) = view.upgrade() {
                if let Some(shared) = view.shared() {
                    shared.input(shared.engine.encode_paste(&text));
                    view.queue_draw();
                }
            }
        });
    }

    /// The pointer moved to `at`: the link under it lights up, and with no
    /// drag in progress a program tracking every move (mode 1003) gets the
    /// motion; drags report their own.
    pub(super) fn hover(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let imp = self.imp();
        imp.pointer.set(at);
        imp.hover.set(Some(state));
        let Some(shared) = self.shared() else { return };
        if !self.button_held() && route(shared.engine.mouse_modes(), state, false) == Route::Report
        {
            self.report_motion(&shared, ffi::BUTTON_NONE, state, at);
        }
        let link = self.link_under(&shared, &imp.frame.borrow());
        if self.show_link(link) {
            self.queue_draw();
        }
    }

    /// The pointer left the widget.
    pub(super) fn pointer_left(&self) {
        self.imp().hover.set(None);
        if self.show_link(None) {
            self.queue_draw();
        }
    }

    /// Keyboard focus entered or left: reported when the program enabled
    /// mode 1004. The cursor turns hollow, or back, and a blink starts over
    /// with it on.
    pub(super) fn focus_changed(&self, gained: bool) {
        self.im_focus(gained);
        if let Some(shared) = self.shared() {
            shared.input(shared.engine.encode_focus(gained));
        }
        self.imp().focused.set(gained);
        self.stop_blink();
        self.queue_draw();
    }

    /// One wheel event: rows of it scroll the viewport, go to the program
    /// as wheel buttons, or as arrow keys, per [`route`].
    pub(super) fn wheel(&self, dy: f64, unit: gdk::ScrollUnit, state: gdk::ModifierType) {
        let Some(shared) = self.shared() else { return };
        let (_, cell_h) = self.cell_size();
        let carry = &self.imp().wheel_carry;
        let notch = self.imp().options.borrow().wheel_rows;
        let (rows, rest) = wheel_rows(carry.get(), dy, unit, cell_h, notch);
        carry.set(rest);
        if rows == 0 {
            return;
        }
        let count = rows.unsigned_abs();
        match route(shared.engine.mouse_modes(), state, true) {
            Route::Local => shared.engine.scroll(Scroll::By(rows)),
            Route::Report => {
                let button = if rows < 0 {
                    ffi::BUTTON_WHEEL_UP
                } else {
                    ffi::BUTTON_WHEEL_DOWN
                };
                self.report_wheel(&shared, button, count, state);
            }
            Route::Arrows => {
                let keysym = if rows < 0 { ARROW_UP } else { ARROW_DOWN };
                shared.input(shared.engine.encode_key(keysym, 0, 0, "").repeat(count));
            }
        }
        self.queue_draw();
    }

    /// One sideways wheel event: columns of it go to a tracking program as
    /// wheel buttons. Nothing local scrolls sideways, so `false` leaves the
    /// event to whatever sits above the view.
    pub(super) fn wheel_sideways(
        &self,
        dx: f64,
        unit: gdk::ScrollUnit,
        state: gdk::ModifierType,
    ) -> bool {
        let Some(shared) = self.shared() else {
            return false;
        };
        if route(shared.engine.mouse_modes(), state, true) != Route::Report {
            return false;
        }
        let (cell_w, _) = self.cell_size();
        let carry = &self.imp().wheel_carry_x;
        let notch = self.imp().options.borrow().wheel_rows;
        let (cols, rest) = wheel_rows(carry.get(), dx, unit, cell_w, notch);
        carry.set(rest);
        let button = if cols < 0 {
            ffi::BUTTON_WHEEL_LEFT
        } else {
            ffi::BUTTON_WHEEL_RIGHT
        };
        self.report_wheel(&shared, button, cols.unsigned_abs(), state);
        true
    }

    /// Send `count` presses of a wheel button to the program.
    fn report_wheel(&self, shared: &Shared, button: u32, count: usize, state: gdk::ModifierType) {
        let at = self.imp().pointer.get();
        let event = self.mouse_event(ffi::MOUSE_PRESS, button, state, at);
        let report = shared.engine.encode_mouse(&event);
        // One report per chunk, as VTE sends them: the macro recorder
        // skips reports chunk by chunk (doc 12).
        for _ in 0..count {
            shared.input(report.clone());
        }
    }

    /// A mouse event at widget pixels `at`, with the grid's geometry.
    fn mouse_event(
        &self,
        action: u32,
        button: u32,
        state: gdk::ModifierType,
        at: (f64, f64),
    ) -> MouseEvent {
        let (cell_w, cell_h) = self.cell_size();
        MouseEvent {
            action,
            button,
            mods: mods(state),
            x: at.0 as f32,
            y: at.1 as f32,
            width: px(self.width()),
            height: px(self.height()),
            cell_w: px(cell_w),
            cell_h: px(cell_h),
            any_pressed: u32::from(self.button_held()),
        }
    }

    /// Send a button press or release to the program.
    fn report(
        &self,
        shared: &Shared,
        action: u32,
        button: u32,
        state: gdk::ModifierType,
        at: (f64, f64),
    ) {
        self.imp().motion_cell.set(Some(self.cell_at(at.0, at.1)));
        let event = self.mouse_event(action, button, state, at);
        shared.input(shared.engine.encode_mouse(&event));
        self.queue_draw();
    }

    /// Send motion to the program once per cell entered.
    fn report_motion(
        &self,
        shared: &Shared,
        button: u32,
        state: gdk::ModifierType,
        at: (f64, f64),
    ) {
        let cell = Some(self.cell_at(at.0, at.1));
        if self.imp().motion_cell.replace(cell) != cell {
            let event = self.mouse_event(ffi::MOUSE_MOTION, button, state, at);
            shared.input(shared.engine.encode_mouse(&event));
        }
    }

    /// Encode a key press with the text its keyval stands for.
    pub(super) fn key(&self, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
        let text: String = keyval
            .to_unicode()
            .filter(|c| !c.is_control())
            .map(String::from)
            .unwrap_or_default();
        self.key_with_text(keyval, state, &text)
    }

    /// Encode a key press producing `text`; `false` lets it propagate
    /// (modifier-only keys).
    pub(super) fn key_with_text(
        &self,
        keyval: gdk::Key,
        state: gdk::ModifierType,
        text: &str,
    ) -> bool {
        let Some(shared) = self.shared() else {
            return false;
        };
        if let Some(scroll) = scroll_key(keyval, state, shared.engine.size().1) {
            shared.engine.scroll(scroll);
            self.queue_draw();
            return true;
        }
        if select_output_key(keyval, state) {
            if shared.engine.select_output() {
                self.copy_primary();
                self.queue_draw();
            }
            return true;
        }
        let mods = mods(state);
        let unshifted = keyval.to_lower().to_unicode().map_or(0, u32::from);
        let bytes = shared
            .engine
            .encode_key(keyval.into_glib(), unshifted, mods, text);
        if bytes.is_empty() {
            return false;
        }
        shared.engine.clear_selection();
        shared.engine.clear_search();
        shared.input(bytes);
        self.queue_draw();
        true
    }
}

/// Where a mouse event goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Selection and viewport scrolling.
    Local,
    /// The program, through the mouse encoder.
    Report,
    /// The program, as arrow keys (alternate scroll).
    Arrows,
}

/// Decide where a mouse event goes. Shift keeps it local, as the escape
/// hatch from a program grabbing the mouse; otherwise a tracking program
/// gets it, and with no tracking the wheel on the alternate screen under
/// mode 1007 becomes arrow keys.
pub(super) fn route(modes: MouseModes, state: gdk::ModifierType, wheel: bool) -> Route {
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        Route::Local
    } else if modes.tracking.is_some() {
        Route::Report
    } else if wheel && modes.alt_scroll && modes.alt_screen {
        Route::Arrows
    } else {
        Route::Local
    }
}

/// GDK modifiers as the encoders' `MOD_*` bits.
fn mods(state: gdk::ModifierType) -> u32 {
    [
        (gdk::ModifierType::SHIFT_MASK, ffi::MOD_SHIFT),
        (gdk::ModifierType::CONTROL_MASK, ffi::MOD_CTRL),
        (gdk::ModifierType::ALT_MASK, ffi::MOD_ALT),
        (gdk::ModifierType::SUPER_MASK, ffi::MOD_SUPER),
    ]
    .into_iter()
    .filter(|&(mask, _)| state.contains(mask))
    .fold(0, |mods, (_, bit)| mods | bit)
}

/// Rows to scroll for one wheel event, `notch` rows to a notch, and the
/// fraction left for the next; sideways, columns of `cell` pixels. High-resolution wheels and touchpads
/// send many steps smaller than a row, so each one only adds to `carry`;
/// reversing direction drops it.
fn wheel_rows(
    carry: f64,
    delta: f64,
    unit: gdk::ScrollUnit,
    cell: i32,
    notch: f64,
) -> (isize, f64) {
    let rows = match unit {
        gdk::ScrollUnit::Surface => delta / f64::from(cell.max(1)),
        _ => delta * notch,
    };
    let carry = if carry * rows < 0.0 { 0.0 } else { carry };
    let total = carry + rows;
    let whole = total.trunc();
    (whole as isize, total - whole)
}

/// Shift+PageUp/PageDown scroll a page of `rows`, Shift+Home/End to the
/// top/bottom, as VTE does; other modifiers let the key reach the remote.
/// The modifiers a local key is told apart by; Caps Lock does not count.
fn key_mods(state: gdk::ModifierType) -> gdk::ModifierType {
    state
        & (gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK)
}

fn ctrl_shift(state: gdk::ModifierType) -> bool {
    key_mods(state) == gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
}

fn scroll_key(keyval: gdk::Key, state: gdk::ModifierType, rows: u16) -> Option<Scroll> {
    if ctrl_shift(state) {
        return match keyval {
            gdk::Key::Up => Some(Scroll::Prompts(-1)),
            gdk::Key::Down => Some(Scroll::Prompts(1)),
            _ => None,
        };
    }
    if key_mods(state) != gdk::ModifierType::SHIFT_MASK {
        return None;
    }
    let page = isize::try_from(rows.max(1)).unwrap_or(isize::MAX);
    match keyval {
        gdk::Key::Page_Up => Some(Scroll::By(-page)),
        gdk::Key::Page_Down => Some(Scroll::By(page)),
        gdk::Key::Home => Some(Scroll::Top),
        gdk::Key::End => Some(Scroll::Bottom),
        _ => None,
    }
}

/// Ctrl+Shift+O selects a command's output.
fn select_output_key(keyval: gdk::Key, state: gdk::ModifierType) -> bool {
    ctrl_shift(state) && matches!(keyval, gdk::Key::O | gdk::Key::o)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHEEL: gdk::ScrollUnit = gdk::ScrollUnit::Wheel;
    const SURFACE: gdk::ScrollUnit = gdk::ScrollUnit::Surface;

    /// Feed `steps` through `wheel_rows`; the total rows scrolled.
    fn scrolled(steps: &[f64], unit: gdk::ScrollUnit, cell_h: i32) -> isize {
        let mut carry = 0.0;
        steps
            .iter()
            .map(|&dy| {
                let (rows, rest) = wheel_rows(carry, dy, unit, cell_h, 3.0);
                carry = rest;
                rows
            })
            .sum()
    }

    #[test]
    fn a_notch_scrolls_three_rows_however_it_is_split() {
        assert_eq!(scrolled(&[-1.0], WHEEL, 16), -3);
        assert_eq!(scrolled(&[1.0], WHEEL, 16), 3);
        // A high-resolution wheel: eight eighth-notches per notch.
        assert_eq!(scrolled(&[-0.125; 8], WHEEL, 16), -3);
        assert_eq!(scrolled(&[0.125; 16], WHEEL, 16), 6);
    }

    #[test]
    fn a_notch_is_as_many_rows_as_asked_and_pixels_stay_cells() {
        assert_eq!(wheel_rows(0.0, 1.0, WHEEL, 16, 5.0), (5, 0.0));
        assert_eq!(wheel_rows(0.0, 32.0, SURFACE, 16, 5.0), (2, 0.0));
    }

    #[test]
    fn touchpad_pixels_scroll_by_cell_height() {
        assert_eq!(scrolled(&[-48.0], SURFACE, 16), -3);
        assert_eq!(scrolled(&[4.0; 8], SURFACE, 16), 2);
        assert_eq!(scrolled(&[1.0], SURFACE, 0), 1, "no division by zero");
    }

    #[test]
    fn reversing_drops_the_leftover_fraction() {
        assert_eq!(scrolled(&[-0.3, 0.3], WHEEL, 16), 0);
        let (_, rest) = wheel_rows(-0.9, 0.125, WHEEL, 16, 3.0);
        assert!(rest > 0.0, "{rest}");
    }

    #[test]
    fn mouse_events_route_by_tracking_alt_scroll_and_shift() {
        use crate::terminal::ghostty::engine::Tracking;
        let none = gdk::ModifierType::empty();
        let shift = gdk::ModifierType::SHIFT_MASK;
        let modes = |tracking, alt_scroll, alt_screen| MouseModes {
            tracking,
            alt_scroll,
            alt_screen,
        };
        let plain = modes(None, false, false);
        let pager = modes(None, true, true);
        let tracking = [
            Tracking::X10,
            Tracking::Normal,
            Tracking::Button,
            Tracking::Any,
        ];

        // No tracking: everything local, except the wheel on the alternate
        // screen under mode 1007.
        for wheel in [false, true] {
            assert_eq!(route(plain, none, wheel), Route::Local);
            assert_eq!(route(modes(None, true, false), none, wheel), Route::Local);
            assert_eq!(route(modes(None, false, true), none, wheel), Route::Local);
        }
        assert_eq!(route(pager, none, true), Route::Arrows);
        assert_eq!(route(pager, none, false), Route::Local);

        // Any tracking mode reports buttons and the wheel, alternate
        // scroll or not.
        for mode in tracking {
            for (alt_scroll, alt_screen) in [(false, false), (true, true)] {
                let modes = modes(Some(mode), alt_scroll, alt_screen);
                assert_eq!(route(modes, none, false), Route::Report, "{mode:?}");
                assert_eq!(route(modes, none, true), Route::Report, "{mode:?}");
                // Ctrl does not bypass.
                let ctrl = gdk::ModifierType::CONTROL_MASK;
                assert_eq!(route(modes, ctrl, true), Route::Report, "{mode:?}");
            }
        }

        // Shift keeps every event local.
        for tracking in [None, Some(Tracking::Normal), Some(Tracking::Any)] {
            for wheel in [false, true] {
                let modes = modes(tracking, true, true);
                assert_eq!(route(modes, shift, wheel), Route::Local, "{tracking:?}");
            }
        }
    }

    #[test]
    fn gdk_modifiers_map_to_encoder_bits() {
        assert_eq!(mods(gdk::ModifierType::empty()), 0);
        let state = gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::LOCK_MASK;
        assert_eq!(mods(state), ffi::MOD_SHIFT | ffi::MOD_ALT);
        assert_eq!(
            mods(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SUPER_MASK),
            ffi::MOD_CTRL | ffi::MOD_SUPER
        );
    }

    #[test]
    fn shift_page_keys_scroll_and_others_pass_through() {
        let shift = gdk::ModifierType::SHIFT_MASK;
        let page = |key| scroll_key(key, shift, 24);
        assert_eq!(page(gdk::Key::Page_Up), Some(Scroll::By(-24)));
        assert_eq!(page(gdk::Key::Page_Down), Some(Scroll::By(24)));
        assert_eq!(page(gdk::Key::Home), Some(Scroll::Top));
        assert_eq!(page(gdk::Key::End), Some(Scroll::Bottom));
        assert_eq!(page(gdk::Key::A), None);
        // Caps Lock does not count; no Shift or more modifiers go remote.
        let caps = shift | gdk::ModifierType::LOCK_MASK;
        assert!(scroll_key(gdk::Key::Page_Up, caps, 24).is_some());
        assert_eq!(
            scroll_key(gdk::Key::Page_Up, gdk::ModifierType::empty(), 24),
            None
        );
        let ctrl_shift = shift | gdk::ModifierType::CONTROL_MASK;
        assert_eq!(scroll_key(gdk::Key::Page_Up, ctrl_shift, 24), None);
    }

    #[test]
    fn ctrl_shift_keys_reach_prompts_and_others_pass_through() {
        let ctrl = gdk::ModifierType::CONTROL_MASK;
        let shift = gdk::ModifierType::SHIFT_MASK;
        let alt = gdk::ModifierType::ALT_MASK;
        let lock = gdk::ModifierType::LOCK_MASK;
        let up = |state| scroll_key(gdk::Key::Up, state, 24);
        assert_eq!(up(ctrl | shift), Some(Scroll::Prompts(-1)));
        assert_eq!(up(ctrl | shift | lock), Some(Scroll::Prompts(-1)));
        assert_eq!(
            scroll_key(gdk::Key::Down, ctrl | shift, 24),
            Some(Scroll::Prompts(1))
        );
        for state in [gdk::ModifierType::empty(), shift, ctrl, ctrl | shift | alt] {
            assert_eq!(up(state), None, "{state:?}");
        }

        assert!(select_output_key(gdk::Key::O, ctrl | shift));
        // Caps Lock turns the letter back to lower case.
        assert!(select_output_key(gdk::Key::o, ctrl | shift | lock));
        assert!(!select_output_key(gdk::Key::O, shift));
        assert!(!select_output_key(gdk::Key::o, ctrl));
        assert!(!select_output_key(gdk::Key::O, ctrl | shift | alt));
        assert!(!select_output_key(gdk::Key::P, ctrl | shift));
    }
}
