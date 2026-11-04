//! [`GhosttyView`]: the GTK4 widget that draws a libghostty-vt grid.
//!
//! Each frame snapshots the engine's viewport and paints it with
//! `GtkSnapshot`: one background rectangle per run of equal colors, and
//! for each run of equal style, glyphs that Pango picks and shapes but
//! does not place: each goes at its own cell, since the font's advances
//! are rarely the whole-pixel cell width and would drift off the grid.
//! A cell's whole grapheme cluster is shaped together and sits at that
//! cell. Underline and strikethrough are drawn as rectangles. Search
//! matches are lit in Ghostty's colors, the current one apart, under the
//! selection and the cursor. The cursor takes the shape the terminal asks
//! for and blinks on a timer that runs only while a blink can show; without
//! focus it is a steady hollow block.
//!
//! Input: keys go through libghostty-vt's key encoder (so cursor-key and
//! Kitty keyboard modes apply) and leave as `DataOut`; Shift+PageUp/
//! PageDown/Home/End scroll the viewport as in VTE. Mouse events go where
//! [`route`] says: to the program through the mouse encoder while it
//! tracks the mouse, as arrow keys for the wheel under alternate scroll,
//! else local, where a primary-button drag selects (by word after a double
//! click, by line after a triple), a finished selection goes to PRIMARY, a
//! middle click pastes PRIMARY, and the wheel scrolls the viewport three
//! rows per notch (smooth steps accumulate). Shift keeps them local. Focus
//! changes are reported when the program asks. A press or a typed key ends
//! the search, as it clears the selection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk, pango};

use super::engine::{
    push_cluster, Cell as GridCell, Frame, FrameInfo, MouseEvent, MouseModes, Scroll, Span,
};
use super::ffi;
use super::Shared;
use crate::terminal::TerminalEvent;

/// The font a view starts with, until `set_font`.
const DEFAULT_FONT: &str = "Monospace 12";
/// Rows per wheel notch.
const WHEEL_ROWS: f64 = 3.0;
/// How long a blinking cursor stays on, and off; Ghostty's default.
const BLINK_INTERVAL: Duration = Duration::from_millis(600);
/// The arrow keysyms alternate scroll sends.
const ARROW_UP: u32 = 0xff52;
const ARROW_DOWN: u32 = 0xff54;
/// Search highlight colors, Ghostty's defaults: dark text on yellow, on
/// orange for the current match.
const SEARCH_FG: [u8; 3] = [0, 0, 0];
const SEARCH_BG: [u8; 3] = [0xff, 0xe0, 0x82];
const SEARCH_CURRENT_BG: [u8; 3] = [0xf2, 0xa5, 0x7e];

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GhosttyView {
        pub(super) shared: RefCell<Option<Rc<Shared>>>,
        pub(super) frame: RefCell<Frame>,
        pub(super) font: RefCell<Option<pango::FontDescription>>,
        /// The default background's alpha; colored cells stay opaque.
        pub(super) opacity: Cell<f32>,
        /// The cell's metrics; recomputed after a font change.
        pub(super) metrics: Cell<Option<Metrics>>,
        /// The grid asked for by `set_size`: the natural size.
        pub(super) requested: Cell<(u16, u16)>,
        /// The cell size the engine last got from `fit_grid`.
        pub(super) fitted_cell: Cell<(i32, i32)>,
        /// The last grid reported as `SizeChange`, as `(cols, rows)`.
        pub(super) reported: Cell<Option<(u16, u16)>>,
        /// Wheel rows not yet scrolled; see [`super::wheel_rows`].
        pub(super) wheel_carry: Cell<f64>,
        /// Where the primary-button drag in progress goes.
        pub(super) drag: Cell<Option<super::Route>>,
        /// The last pointer position, for wheel reports.
        pub(super) pointer: Cell<(f64, f64)>,
        /// The cell of the last reported motion, so each cell is sent once.
        pub(super) motion_cell: Cell<Option<(u16, u16)>>,
        /// Whether the view has keyboard focus, from the focus controller.
        pub(super) focused: Cell<bool>,
        /// The blink's off phase; a stopped timer leaves it false.
        pub(super) blink_off: Cell<bool>,
        /// The blink timer; see [`super::needs_blink_timer`].
        pub(super) blink: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GhosttyView {
        const NAME: &'static str = "WaytermGhosttyView";
        type Type = super::GhosttyView;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for GhosttyView {}

    impl WidgetImpl for GhosttyView {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (cell_w, cell_h) = self.obj().cell_size();
            let (cols, rows) = self.requested.get();
            match orientation {
                gtk4::Orientation::Horizontal => (cell_w, cell_w * i32::from(cols), -1, -1),
                _ => (cell_h, cell_h * i32::from(rows), -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().fit_grid(width, height);
        }

        fn unmap(&self) {
            self.obj().stop_blink();
            self.parent_unmap();
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.obj().paint(snapshot);
        }
    }
}

glib::wrapper! {
    /// Draws a [`Shared`] engine's grid; see the module docs.
    pub struct GhosttyView(ObjectSubclass<imp::GhosttyView>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl GhosttyView {
    pub(super) fn new(shared: Rc<Shared>) -> Self {
        let view: Self = glib::Object::new();
        let imp = view.imp();
        imp.requested.set(shared.engine.size());
        imp.opacity.set(1.0);
        imp.shared.replace(Some(shared));
        view.set_focusable(true);
        view.set_hexpand(true);
        view.set_vexpand(true);
        view.set_overflow(gtk4::Overflow::Hidden);
        view.install_input();
        view
    }

    fn shared(&self) -> Option<Rc<Shared>> {
        self.imp().shared.borrow().clone()
    }

    /// Input on its way to the remote side, as VTE's `commit`.
    pub fn connect_input(&self, watch: impl Fn(&[u8]) + 'static) {
        if let Some(shared) = self.shared() {
            shared.watchers.input.borrow_mut().push(Rc::new(watch));
        }
    }

    /// Output was written, as VTE's `contents-changed`.
    pub fn connect_output(&self, watch: impl Fn() + 'static) {
        if let Some(shared) = self.shared() {
            shared.watchers.output.borrow_mut().push(Rc::new(watch));
        }
    }

    /// The shell reported its directory (OSC 7, 9 or 1337), as VTE's
    /// `current-directory-uri-changed`; the value is raw, a `file://` URI
    /// for OSC 7.
    pub fn connect_directory(&self, watch: impl Fn(&str) + 'static) {
        if let Some(shared) = self.shared() {
            shared.watchers.directory.borrow_mut().push(Rc::new(watch));
        }
    }

    pub(super) fn set_font(&self, font: &pango::FontDescription) {
        self.imp().font.replace(Some(font.clone()));
        self.imp().metrics.set(None);
        self.queue_resize();
        self.queue_draw();
    }

    pub(super) fn set_opacity(&self, opacity: f32) {
        self.imp().opacity.set(opacity);
        self.queue_draw();
    }

    /// `set_size`: resize the grid now and make it the natural size.
    pub(super) fn request_grid(&self, rows: u16, cols: u16) {
        self.imp().requested.set((cols, rows));
        if let Some(shared) = self.shared() {
            let (cell_w, cell_h) = self.cell_size();
            shared.engine.resize(cols, rows, px(cell_w), px(cell_h));
        }
        self.queue_resize();
        self.queue_draw();
    }

    fn font(&self) -> pango::FontDescription {
        self.imp()
            .font
            .borrow()
            .clone()
            .unwrap_or_else(|| pango::FontDescription::from_string(DEFAULT_FONT))
    }

    /// The cell, measured from the font's "M", and where its text sits.
    fn metrics(&self) -> Metrics {
        if let Some(metrics) = self.imp().metrics.get() {
            return metrics;
        }
        let font = self.font();
        let layout = self.create_pango_layout(Some("M"));
        layout.set_font_description(Some(&font));
        let (w, h) = layout.pixel_size();
        let baseline = pixels(layout.baseline());
        let font_metrics = self.pango_context().metrics(Some(&font), None);
        // Whole pixels, so the lines stay sharp.
        let line = |position, thickness| {
            (
                (baseline - pixels(position)).round(),
                pixels(thickness).round().max(1.0),
            )
        };
        let metrics = Metrics {
            width: w.max(1),
            height: h.max(1),
            baseline,
            underline: line(
                font_metrics.underline_position(),
                font_metrics.underline_thickness(),
            ),
            strike: line(
                font_metrics.strikethrough_position(),
                font_metrics.strikethrough_thickness(),
            ),
        };
        self.imp().metrics.set(Some(metrics));
        metrics
    }

    /// One cell in whole pixels.
    fn cell_size(&self) -> (i32, i32) {
        let metrics = self.metrics();
        (metrics.width, metrics.height)
    }

    /// Match the grid to the allocation and report a changed grid once.
    fn fit_grid(&self, width: i32, height: i32) {
        let Some(shared) = self.shared() else { return };
        let (cell_w, cell_h) = self.cell_size();
        let fit =
            |pixels: i32, cell: i32| u16::try_from((pixels / cell).max(1)).unwrap_or(u16::MAX);
        let grid = (fit(width, cell_w), fit(height, cell_h));
        let cell_changed = self.imp().fitted_cell.replace((cell_w, cell_h)) != (cell_w, cell_h);
        if cell_changed || shared.engine.size() != grid {
            shared.engine.resize(grid.0, grid.1, px(cell_w), px(cell_h));
        }
        if self.imp().reported.replace(Some(grid)) != Some(grid) {
            shared.events.send(TerminalEvent::SizeChange {
                rows: grid.1.into(),
                cols: grid.0.into(),
            });
        }
    }

    /// The viewport cell under a point, clamped to the grid.
    fn cell_at(&self, x: f64, y: f64) -> (u16, u16) {
        let (cell_w, cell_h) = self.cell_size();
        let (cols, rows) = self.shared().map_or((1, 1), |s| s.engine.size());
        let clamp = |v: f64, cell: i32, n: u16| {
            let index = (v / f64::from(cell)).floor().max(0.0);
            (index as u16).min(n.saturating_sub(1))
        };
        (clamp(x, cell_w, cols), clamp(y, cell_h, rows))
    }

    fn install_input(&self) {
        let keys = gtk4::EventControllerKey::new();
        let view = self.downgrade();
        keys.connect_key_pressed(move |_, keyval, _, state| {
            let handled = view.upgrade().is_some_and(|view| view.key(keyval, state));
            if handled {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
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
            if let (Some(view), Some((x, y))) = (view.upgrade(), drag.start_point()) {
                view.release(drag.current_event_state(), (x + dx, y + dy));
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

        let motion = gtk4::EventControllerMotion::new();
        let view = self.downgrade();
        motion.connect_motion(move |motion, x, y| {
            if let Some(view) = view.upgrade() {
                view.hover(motion.current_event_state(), (x, y));
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

        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let view = self.downgrade();
        wheel.connect_scroll(move |wheel, _, dy| {
            let Some(view) = view.upgrade() else {
                return glib::Propagation::Proceed;
            };
            view.wheel(dy, wheel.unit(), wheel.current_event_state());
            glib::Propagation::Stop
        });
        self.add_controller(wheel);
    }

    /// Primary button down at widget pixels `at`, `time` in milliseconds:
    /// report it, or start a selection. The whole drag goes the same way.
    fn press(&self, state: gdk::ModifierType, time: u32, at: (f64, f64)) {
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
            shared
                .engine
                .selection_press(cell, at, time, repeat_ms, repeat_px);
        }
        self.queue_draw();
    }

    /// The primary button dragged to `at`.
    fn drag_to(&self, state: gdk::ModifierType, at: (f64, f64)) {
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
    fn release(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        match self.imp().drag.take() {
            Some(Route::Report) => {
                self.report(&shared, ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, state, at);
            }
            Some(_) => {
                shared.engine.selection_release(self.cell_at(at.0, at.1));
                self.copy_primary();
            }
            None => {}
        }
    }

    /// Put the selection on PRIMARY; without one, PRIMARY keeps its text.
    pub(super) fn copy_primary(&self) {
        let text = self.shared().and_then(|s| s.engine.selection_text());
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.primary_clipboard().set_text(&text);
        }
    }

    /// Middle button down: paste PRIMARY, bracketed when the program
    /// enabled mode 2004, unless the program tracks the mouse.
    fn paste_primary(&self, state: gdk::ModifierType) {
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

    /// The pointer moved to `at` with no drag in progress: motion for a
    /// program tracking every move (mode 1003); drags report their own.
    fn hover(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let imp = self.imp();
        imp.pointer.set(at);
        let Some(shared) = self.shared() else { return };
        if imp.drag.get().is_none()
            && route(shared.engine.mouse_modes(), state, false) == Route::Report
        {
            self.report_motion(&shared, ffi::BUTTON_NONE, state, at);
        }
    }

    /// Keyboard focus entered or left: reported when the program enabled
    /// mode 1004. The cursor turns hollow, or back, and a blink starts over
    /// with it on.
    fn focus_changed(&self, gained: bool) {
        if let Some(shared) = self.shared() {
            shared.input(shared.engine.encode_focus(gained));
        }
        self.imp().focused.set(gained);
        self.stop_blink();
        self.queue_draw();
    }

    /// Start the blink timer when the frame needs it and stop it when not;
    /// the paint after a map or a focus change is what starts it again.
    fn sync_blink(&self, info: &FrameInfo) {
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
    fn blink_tick(&self) {
        let off = &self.imp().blink_off;
        off.set(!off.get());
        self.queue_draw();
    }

    /// Stop the timer, leaving the cursor on.
    fn stop_blink(&self) {
        if let Some(tick) = self.imp().blink.take() {
            tick.remove();
        }
        self.imp().blink_off.set(false);
    }

    /// One wheel event: rows of it scroll the viewport, go to the program
    /// as wheel buttons, or as arrow keys, per [`route`].
    fn wheel(&self, dy: f64, unit: gdk::ScrollUnit, state: gdk::ModifierType) {
        let Some(shared) = self.shared() else { return };
        let (_, cell_h) = self.cell_size();
        let carry = &self.imp().wheel_carry;
        let (rows, rest) = wheel_rows(carry.get(), dy, unit, cell_h);
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
                let at = self.imp().pointer.get();
                let event = self.mouse_event(ffi::MOUSE_PRESS, button, state, at);
                let report = shared.engine.encode_mouse(&event);
                // One report per chunk, as VTE sends them: the macro
                // recorder skips reports chunk by chunk (doc 12).
                for _ in 0..count {
                    shared.input(report.clone());
                }
            }
            Route::Arrows => {
                let keysym = if rows < 0 { ARROW_UP } else { ARROW_DOWN };
                shared.input(shared.engine.encode_key(keysym, 0, 0, "").repeat(count));
            }
        }
        self.queue_draw();
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
            any_pressed: u32::from(action != ffi::MOUSE_RELEASE && button == ffi::BUTTON_LEFT),
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

    /// Encode a key press; `false` lets it propagate (modifier-only keys).
    fn key(&self, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
        let Some(shared) = self.shared() else {
            return false;
        };
        if let Some(scroll) = scroll_key(keyval, state, shared.engine.size().1) {
            shared.engine.scroll(scroll);
            self.queue_draw();
            return true;
        }
        let mods = mods(state);
        let text: String = keyval
            .to_unicode()
            .filter(|c| !c.is_control())
            .map(String::from)
            .unwrap_or_default();
        let unshifted = keyval.to_lower().to_unicode().map_or(0, u32::from);
        let bytes = shared
            .engine
            .encode_key(keyval.into_glib(), unshifted, mods, &text);
        if bytes.is_empty() {
            return false;
        }
        shared.engine.clear_selection();
        shared.engine.clear_search();
        shared.input(bytes);
        self.queue_draw();
        true
    }

    fn paint(&self, snapshot: &gtk4::Snapshot) {
        let Some(shared) = self.shared() else { return };
        let mut frame = self.imp().frame.borrow_mut();
        if !shared.engine.snapshot(&mut frame) {
            return;
        }
        let info = frame.info;
        self.sync_blink(&info);
        let imp = self.imp();
        let cursor = cursor_shape(&info, imp.focused.get(), imp.blink_off.get());
        let metrics = self.metrics();
        let (cw, ch) = (metrics.width as f32, metrics.height as f32);
        snapshot.append_color(
            &rgba(info.bg, self.imp().opacity.get()),
            &graphene::Rect::new(0.0, 0.0, self.width() as f32, self.height() as f32),
        );

        let font = self.font();
        let (cols, rows) = (usize::from(info.cols), usize::from(info.rows));
        let marks = search_marks(&shared.engine.search_spans(), cols, rows);
        for row in 0..rows {
            let y = row as f32 * ch;
            let cells = painted_row(&frame, row, &marks, cursor == Some(CursorShape::Block));

            // Backgrounds: one rectangle per run of equal, non-default color.
            let mut col = 0;
            while col < cells.len() {
                let start = col;
                while col < cells.len() && cells[col].bg == cells[start].bg {
                    col += 1;
                }
                if let Some(bg) = cells[start].bg {
                    snapshot.append_color(
                        &rgba(bg, 1.0),
                        &graphene::Rect::new(start as f32 * cw, y, (col - start) as f32 * cw, ch),
                    );
                }
            }

            // Text: one run per style, each cluster at its own cell.
            let mut col = 0;
            while col < cells.len() {
                let start = col;
                while col < cells.len() && cells[col].same_style(&cells[start]) {
                    col += 1;
                }
                let (text, columns) = run_text(&cells[start..col]);
                let style = &cells[start];
                if style.flags & ffi::CELL_INVISIBLE != 0 || text.trim().is_empty() {
                    continue;
                }
                let alpha = if style.flags & ffi::CELL_FAINT != 0 {
                    0.6
                } else {
                    1.0
                };
                let color = rgba(style.fg, alpha);
                let x = start as f32 * cw;
                let baseline = graphene::Point::new(x, y + metrics.baseline);
                self.draw_glyphs(
                    snapshot,
                    &text,
                    &columns,
                    &style.font(&font),
                    &color,
                    &baseline,
                );
                let width = (col - start) as f32 * cw;
                for (flag, (top, thickness)) in [
                    (ffi::CELL_UNDERLINE, metrics.underline),
                    (ffi::CELL_STRIKE, metrics.strike),
                ] {
                    if style.flags & flag != 0 {
                        let line = graphene::Rect::new(x, y + top, width, thickness);
                        snapshot.append_color(&color, &line);
                    }
                }
            }

            // The cursor's outline, over the row's text.
            if row == usize::from(info.cursor_y) {
                let col = usize::from(info.cursor_x);
                if let (Some(shape), Some(cell)) = (cursor, cells.get(col)) {
                    let color = rgba(cursor_color(&info, cell), 1.0);
                    let origin = (col as f32 * cw, y);
                    for rect in cursor_rects(shape, origin, (cw, ch), metrics.underline.1) {
                        snapshot.append_color(&color, &rect);
                    }
                }
            }
        }
    }

    /// Draw `text` from a point on the baseline, each byte in the cell
    /// `columns` gives it (see [`run_text`]).
    fn draw_glyphs(
        &self,
        snapshot: &gtk4::Snapshot,
        text: &str,
        columns: &[usize],
        font: &pango::FontDescription,
        color: &gdk::RGBA,
        baseline: &graphene::Point,
    ) {
        self.shape(text, columns, font, |font, glyphs, _| {
            if let Some(node) = gsk::TextNode::new(font, glyphs, color, baseline) {
                snapshot.append_node(node);
            }
        });
    }

    /// Shape `text` for `each` Pango item: its font, its glyphs and each
    /// glyph's column. Pango picks and shapes the glyphs, a cluster's
    /// together; [`pin_glyphs`] places them at the cell their cluster
    /// comes from.
    fn shape(
        &self,
        text: &str,
        columns: &[usize],
        font: &pango::FontDescription,
        mut each: impl FnMut(&pango::Font, &pango::GlyphString, &[usize]),
    ) {
        let cell = self.metrics().width * pango::SCALE;
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrFontDesc::new(font));
        let length = i32::try_from(text.len()).unwrap_or(i32::MAX);
        for item in pango::itemize(&self.pango_context(), text, 0, length, &attrs, None) {
            let offset = item.offset() as usize;
            let item_text = &text[offset..offset + item.length() as usize];
            let mut glyphs = pango::GlyphString::new();
            pango::shape_full(item_text, Some(text), item.analysis(), &mut glyphs);
            let columns: Vec<usize> = glyphs
                .log_clusters()
                .iter()
                .map(|&byte| columns[offset + byte as usize])
                .collect();
            let advances: Vec<i32> = glyphs
                .glyph_info()
                .iter()
                .map(|glyph| glyph.geometry().width())
                .collect();
            let offsets = pin_glyphs(&columns, &advances, cell);
            for (glyph, x) in glyphs.glyph_info_mut().iter_mut().zip(offsets) {
                let geometry = glyph.geometry_mut();
                geometry.set_x_offset(x + geometry.x_offset());
                geometry.set_width(0);
            }
            each(&item.analysis().font(), &glyphs, &columns);
        }
    }
}

/// A run's text and, for each of its bytes, the column of the cell it
/// comes from, counted from the run's first cell. A cell adds its whole
/// cluster, an empty cell or the tail of a wide one a space, so every cell
/// keeps its column: what follows a wide cluster starts two columns on.
fn run_text(cells: &[Painted]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    for (col, cell) in cells.iter().enumerate() {
        push_cluster(&mut text, cell.cluster);
        columns.resize(text.len(), col);
    }
    (text, columns)
}

/// One viewport row's cells as painted, search marks included: per cell,
/// `Some(current)` lights it for the search, the current match apart, and
/// the selection and the cursor still draw on top. `block` says this frame
/// has a solid block cursor, the one shape that recolors its cell; the
/// others go over the cell as [`cursor_rects`].
fn painted_row<'a>(
    frame: &'a Frame,
    row: usize,
    marks: &[Option<bool>],
    block: bool,
) -> Vec<Painted<'a>> {
    let info = &frame.info;
    let cols = usize::from(info.cols);
    (0..cols)
        .filter_map(|col| frame.cell(row, col).map(|cell| (col, cell)))
        .map(|(col, cell)| {
            let cursor = block && is_cursor(info, row, col);
            let painted = Painted::new(info, cell, frame.grapheme(row, col), cursor);
            match marks.get(row * cols + col) {
                Some(&Some(current)) if !cursor && cell.flags & ffi::CELL_SELECTED == 0 => {
                    painted.searched(current)
                }
                _ => painted,
            }
        })
        .collect()
}

/// Each glyph's x, in Pango units from its run's start, that puts it at
/// its cell: `columns[i]` is the column of glyph `i`'s cluster and
/// `advances[i]` its natural width. Glyphs of one cluster (a base and its
/// marks) keep their spacing from its first glyph; no advance carries over
/// to the next cell.
fn pin_glyphs(columns: &[usize], advances: &[i32], cell: i32) -> Vec<i32> {
    let mut cluster = None;
    let mut pen = 0;
    columns
        .iter()
        .zip(advances)
        .map(|(&col, &advance)| {
            if cluster.replace(col) != Some(col) {
                pen = 0;
            }
            let x = col as i32 * cell + pen;
            pen += advance;
            x
        })
        .collect()
}

/// A cell's size in whole pixels, and where its text sits: the baseline,
/// and the underline and strikethrough as `(top, thickness)`, in pixels
/// down from the cell's top.
#[derive(Clone, Copy, Debug)]
struct Metrics {
    width: i32,
    height: i32,
    baseline: f32,
    underline: (f32, f32),
    strike: (f32, f32),
}

/// One cell as painted: colors after inverse/selection/cursor swaps.
struct Painted<'a> {
    /// The cell's grapheme cluster; empty for an empty cell and for the
    /// tail of a wide one.
    cluster: &'a [u32],
    fg: [u8; 3],
    /// `None` = the frame background already painted underneath.
    bg: Option<[u8; 3]>,
    flags: u8,
}

/// Flags that change how text is drawn (not just its colors).
const TEXT_FLAGS: u8 = ffi::CELL_BOLD
    | ffi::CELL_ITALIC
    | ffi::CELL_UNDERLINE
    | ffi::CELL_STRIKE
    | ffi::CELL_FAINT
    | ffi::CELL_INVISIBLE;

impl<'a> Painted<'a> {
    fn new(info: &FrameInfo, cell: &GridCell, cluster: &'a [u32], cursor: bool) -> Self {
        let swapped =
            (cell.flags & ffi::CELL_INVERSE != 0) ^ (cell.flags & ffi::CELL_SELECTED != 0) ^ cursor;
        let (fg, bg) = if swapped {
            let fg = if cursor && info.cursor_has_rgb != 0 {
                info.cursor_rgb
            } else {
                cell.fg
            };
            (cell.bg, Some(fg))
        } else {
            (cell.fg, (cell.has_bg != 0).then_some(cell.bg))
        };
        Self {
            cluster,
            fg,
            bg,
            flags: cell.flags & TEXT_FLAGS,
        }
    }

    /// Search highlight colors; `current` for the current match.
    fn searched(self, current: bool) -> Self {
        let bg = if current {
            SEARCH_CURRENT_BG
        } else {
            SEARCH_BG
        };
        Self {
            fg: SEARCH_FG,
            bg: Some(bg),
            ..self
        }
    }

    fn same_style(&self, other: &Self) -> bool {
        self.fg == other.fg && self.flags == other.flags
    }

    fn font(&self, base: &pango::FontDescription) -> pango::FontDescription {
        let mut font = base.clone();
        if self.flags & ffi::CELL_BOLD != 0 {
            font.set_weight(pango::Weight::Bold);
        }
        if self.flags & ffi::CELL_ITALIC != 0 {
            font.set_style(pango::Style::Italic);
        }
        font
    }
}

/// How the cursor is drawn in one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorShape {
    Block,
    Underline,
    Bar,
    Hollow,
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
fn route(modes: MouseModes, state: gdk::ModifierType, wheel: bool) -> Route {
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

/// Rows to scroll for one wheel event, and the fraction left for the next.
/// High-resolution wheels and touchpads send many steps smaller than a row,
/// so each one only adds to `carry`; reversing direction drops it.
fn wheel_rows(carry: f64, dy: f64, unit: gdk::ScrollUnit, cell_h: i32) -> (isize, f64) {
    let rows = match unit {
        gdk::ScrollUnit::Surface => dy / f64::from(cell_h.max(1)),
        _ => dy * WHEEL_ROWS,
    };
    let carry = if carry * rows < 0.0 { 0.0 } else { carry };
    let total = carry + rows;
    let whole = total.trunc();
    (whole as isize, total - whole)
}

/// Shift+PageUp/PageDown scroll a page of `rows`, Shift+Home/End to the
/// top/bottom, as VTE does; other modifiers let the key reach the remote.
fn scroll_key(keyval: gdk::Key, state: gdk::ModifierType, rows: u16) -> Option<Scroll> {
    let mods = state
        & (gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK);
    if mods != gdk::ModifierType::SHIFT_MASK {
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

/// Per viewport cell, row-major: `Some(current)` where a search match is
/// lit. Empty without spans.
fn search_marks(spans: &[Span], cols: usize, rows: usize) -> Vec<Option<bool>> {
    if spans.is_empty() {
        return Vec::new();
    }
    let mut marks = vec![None; cols * rows];
    for span in spans.iter().filter(|span| usize::from(span.row) < rows) {
        let line = usize::from(span.row) * cols;
        let end = usize::from(span.end).min(cols);
        let start = usize::from(span.start).min(end);
        for mark in &mut marks[line + start..line + end] {
            *mark = Some(span.current != 0 || mark.unwrap_or(false));
        }
    }
    marks
}

fn is_cursor(info: &FrameInfo, row: usize, col: usize) -> bool {
    info.cursor_visible != 0
        && usize::from(info.cursor_y) == row
        && usize::from(info.cursor_x) == col
}

/// How the frame's cursor is drawn, if at all: hidden by the terminal or
/// in the blink's off phase it is not, and without focus it stays up as a
/// hollow block, as in Ghostty.
fn cursor_shape(info: &FrameInfo, focused: bool, blink_off: bool) -> Option<CursorShape> {
    if info.cursor_visible == 0 {
        return None;
    }
    if !focused {
        return Some(CursorShape::Hollow);
    }
    if info.cursor_blinking != 0 && blink_off {
        return None;
    }
    Some(match info.cursor_style {
        ffi::CURSOR_UNDERLINE => CursorShape::Underline,
        ffi::CURSOR_BAR => CursorShape::Bar,
        ffi::CURSOR_BLOCK_HOLLOW => CursorShape::Hollow,
        _ => CursorShape::Block,
    })
}

/// Whether a blink can show: anywhere else the timer would only wake the
/// main loop.
fn needs_blink_timer(info: &FrameInfo, focused: bool, mapped: bool) -> bool {
    info.cursor_visible != 0 && info.cursor_blinking != 0 && focused && mapped
}

/// The color of a cursor drawn over `cell`: the terminal's cursor color,
/// else the cell's text as painted, which shows on its background.
fn cursor_color(info: &FrameInfo, cell: &Painted) -> [u8; 3] {
    if info.cursor_has_rgb != 0 {
        info.cursor_rgb
    } else {
        cell.fg
    }
}

/// The rectangles of a cursor `line` thick in the cell at `origin`; none
/// for the block, which recolors the cell instead (see [`painted_row`]).
fn cursor_rects(
    shape: CursorShape,
    (x, y): (f32, f32),
    (width, height): (f32, f32),
    line: f32,
) -> Vec<graphene::Rect> {
    let top = graphene::Rect::new(x, y, width, line);
    let bottom = graphene::Rect::new(x, y + height - line, width, line);
    let left = graphene::Rect::new(x, y, line, height);
    let right = graphene::Rect::new(x + width - line, y, line, height);
    match shape {
        CursorShape::Block => Vec::new(),
        CursorShape::Underline => vec![bottom],
        CursorShape::Bar => vec![left],
        CursorShape::Hollow => vec![top, bottom, left, right],
    }
}

fn rgba([r, g, b]: [u8; 3], alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        alpha,
    )
}

fn pixels(pango_units: i32) -> f32 {
    pango_units as f32 / pango::SCALE as f32
}

/// Non-negative pixels for the engine's size reports.
fn px(pixels: i32) -> u32 {
    u32::try_from(pixels).unwrap_or(0)
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
                let (rows, rest) = wheel_rows(carry, dy, unit, cell_h);
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
    fn touchpad_pixels_scroll_by_cell_height() {
        assert_eq!(scrolled(&[-48.0], SURFACE, 16), -3);
        assert_eq!(scrolled(&[4.0; 8], SURFACE, 16), 2);
        assert_eq!(scrolled(&[1.0], SURFACE, 0), 1, "no division by zero");
    }

    #[test]
    fn reversing_drops_the_leftover_fraction() {
        assert_eq!(scrolled(&[-0.3, 0.3], WHEEL, 16), 0);
        let (_, rest) = wheel_rows(-0.9, 0.125, WHEEL, 16);
        assert!(rest > 0.0, "{rest}");
    }

    #[test]
    fn mouse_events_route_by_tracking_alt_scroll_and_shift() {
        use super::super::engine::Tracking;
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

    /// A shown cursor's frame state, as the shim fills it.
    fn cursor_info(style: u8, blinking: bool) -> FrameInfo {
        FrameInfo {
            cursor_visible: 1,
            cursor_style: style,
            cursor_blinking: u8::from(blinking),
            ..FrameInfo::default()
        }
    }

    #[test]
    fn the_cursor_shape_follows_the_terminal_the_focus_and_the_blink() {
        use CursorShape::{Bar, Block, Hollow, Underline};
        for (style, shape) in [
            (ffi::CURSOR_BLOCK, Block),
            (ffi::CURSOR_UNDERLINE, Underline),
            (ffi::CURSOR_BAR, Bar),
            (ffi::CURSOR_BLOCK_HOLLOW, Hollow),
        ] {
            let (steady, blinking) = (cursor_info(style, false), cursor_info(style, true));
            assert_eq!(cursor_shape(&steady, true, false), Some(shape));
            assert_eq!(cursor_shape(&steady, true, true), Some(shape));
            assert_eq!(cursor_shape(&blinking, true, false), Some(shape));
            assert_eq!(cursor_shape(&blinking, true, true), None);
            // Without focus it stays up as a hollow block, whatever the phase.
            assert_eq!(cursor_shape(&steady, false, false), Some(Hollow));
            assert_eq!(cursor_shape(&blinking, false, true), Some(Hollow));
        }
        let hidden = FrameInfo {
            cursor_visible: 0,
            ..cursor_info(ffi::CURSOR_BAR, false)
        };
        assert_eq!(cursor_shape(&hidden, true, false), None);
        assert_eq!(cursor_shape(&hidden, false, false), None);
    }

    #[test]
    fn the_blink_timer_runs_only_while_a_blink_can_show() {
        let blinking = cursor_info(ffi::CURSOR_BLOCK, true);
        assert!(needs_blink_timer(&blinking, true, true));
        assert!(!needs_blink_timer(&blinking, false, true), "unfocused");
        assert!(!needs_blink_timer(&blinking, true, false), "unmapped");
        let steady = cursor_info(ffi::CURSOR_BLOCK, false);
        assert!(!needs_blink_timer(&steady, true, true));
        let hidden = FrameInfo {
            cursor_visible: 0,
            ..blinking
        };
        assert!(!needs_blink_timer(&hidden, true, true));
    }

    #[test]
    fn cursor_shapes_sit_on_the_edges_of_their_cell() {
        let rects = |shape| {
            cursor_rects(shape, (20.0, 40.0), (10.0, 18.0), 2.0)
                .iter()
                .map(|r| (r.x(), r.y(), r.width(), r.height()))
                .collect::<Vec<_>>()
        };
        assert_eq!(rects(CursorShape::Block), [], "it recolors the cell");
        assert_eq!(rects(CursorShape::Underline), [(20.0, 56.0, 10.0, 2.0)]);
        assert_eq!(rects(CursorShape::Bar), [(20.0, 40.0, 2.0, 18.0)]);
        assert_eq!(
            rects(CursorShape::Hollow),
            [
                (20.0, 40.0, 10.0, 2.0),
                (20.0, 56.0, 10.0, 2.0),
                (20.0, 40.0, 2.0, 18.0),
                (28.0, 40.0, 2.0, 18.0),
            ]
        );
    }

    /// Only a solid block takes over its cell: any other frame paints the
    /// cell as usual, search mark included, for the cursor to go on top.
    #[test]
    fn only_a_solid_block_recolors_the_cell_under_the_cursor() {
        let engine = super::super::engine::Engine::new(4, 1).expect("engine");
        engine.write(b"ab\x1b[D");
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let cell = *frame.cell(0, 1).unwrap();
        let marks = [None, Some(true), None, None];

        let block = &painted_row(&frame, 0, &marks, true)[1];
        assert_eq!((block.fg, block.bg), (cell.bg, Some(cell.fg)));
        let plain = &painted_row(&frame, 0, &[], false)[1];
        assert_eq!((plain.fg, plain.bg), (cell.fg, None));
        let marked = &painted_row(&frame, 0, &marks, false)[1];
        assert_eq!((marked.fg, marked.bg), (SEARCH_FG, Some(SEARCH_CURRENT_BG)));
    }

    /// An outline takes the cursor color, else the text color its cell is
    /// painted in, so it shows on an inverse cell too.
    #[test]
    fn an_outline_cursor_shows_on_any_cell() {
        let engine = super::super::engine::Engine::new(4, 1).expect("engine");
        engine.write(b"\x1b[7;31;42ma\x1b[0m\x1b[D");
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let cells = painted_row(&frame, 0, &[], false);
        let cell = frame.cell(0, 0).unwrap();
        assert_eq!(cells[0].bg, Some(cell.fg));
        assert_eq!(cursor_color(&frame.info, &cells[0]), cell.bg);

        let info = FrameInfo {
            cursor_rgb: [1, 2, 3],
            cursor_has_rgb: 1,
            ..frame.info
        };
        assert_eq!(cursor_color(&info, &cells[0]), [1, 2, 3]);
    }

    #[test]
    fn search_spans_mark_their_cells() {
        let span = |row, start, end, current| Span {
            row,
            start,
            end,
            current,
        };
        assert_eq!(search_marks(&[], 4, 2), []);
        let spans = [
            span(0, 1, 3, 0),
            span(1, 0, 2, 1),
            // Clipped to the grid; off it, or empty, nothing.
            span(1, 3, 9, 0),
            span(5, 0, 4, 1),
            span(0, 3, 2, 1),
        ];
        let (lit, current) = (Some(false), Some(true));
        assert_eq!(
            search_marks(&spans, 4, 2),
            [None, lit, lit, None, current, current, None, lit]
        );
        // The current match wins where matches overlap, in either order.
        for spans in [
            [span(0, 0, 2, 0), span(0, 1, 2, 1)],
            [span(0, 1, 2, 1), span(0, 0, 2, 0)],
        ] {
            assert_eq!(search_marks(&spans, 2, 1), [lit, current]);
        }
    }

    #[test]
    fn a_run_carries_each_cells_whole_cluster_at_its_column() {
        let engine = super::super::engine::Engine::new(10, 1).expect("engine");
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        // Mode 2027 makes the family one wide cell.
        engine.write(format!("\x1b[?2027hx{family}e\u{301}y").as_bytes());
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let (text, columns) = run_text(&painted_row(&frame, 0, &[], true)[..5]);
        // The family's tail is a space, so é and y keep their columns.
        let cells = [("x", 0), (family, 1), (" ", 2), ("e\u{301}", 3), ("y", 4)];
        assert_eq!(text, cells.map(|(cluster, _)| cluster).concat());
        let expected: Vec<usize> = cells
            .iter()
            .flat_map(|&(cluster, col)| std::iter::repeat_n(col, cluster.len()))
            .collect();
        assert_eq!(columns, expected);
    }

    #[test]
    fn glyphs_sit_at_their_cells_whatever_the_font_advances() {
        // 9.6-pixel glyphs on a 10-pixel grid.
        let advance = 96 * pango::SCALE / 10;
        let cell = 10 * pango::SCALE;
        assert_eq!(
            pin_glyphs(&[0, 1, 2, 70], &[advance; 4], cell),
            vec![0, cell, 2 * cell, 70 * cell]
        );
        // Right-to-left items list their glyphs in visual order.
        assert_eq!(
            pin_glyphs(&[2, 1, 0], &[advance; 3], cell),
            vec![2 * cell, cell, 0]
        );
        // A ligature covers two cells; the next glyph starts after them.
        assert_eq!(
            pin_glyphs(&[0, 2], &[2 * advance, advance], cell),
            vec![0, 2 * cell]
        );
        // A base and its mark share a cluster: the mark keeps its place.
        assert_eq!(
            pin_glyphs(&[0, 0, 1], &[advance, 0, advance], cell),
            vec![0, advance, cell]
        );
    }
}

/// The widget check needs a display. It is its own test, not part of
/// `ui::gtk_tests`, and must not share a run with it (GTK binds to the
/// first thread that initializes it):
/// `env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test
/// --features backend-ghostty -- --ignored ghostty_display`.
#[cfg(test)]
mod ghostty_display {
    use super::*;
    use crate::macros::recorder::is_pointer_or_focus_report;
    use crate::terminal::{
        create_terminal, install_backend_preference, BackendKind, GhosttyBackend, TerminalBackend,
        TRANSLUCENT_CLASS,
    };
    use crate::ui::translucency::gtk_tests::alpha_at;
    use std::ops::Range;
    use tokio::sync::mpsc;

    /// The view paints a frame, reports its grid once allocated, and turns
    /// a key press into `DataOut`.
    #[test]
    #[ignore = "needs a display; run under xvfb-run"]
    fn view_draws_sizes_and_types() {
        gtk4::init().expect("gtk init");
        let (tx, mut rx) = mpsc::channel(16);
        let backend = GhosttyBackend::new(tx).expect("engine");
        let window = gtk4::Window::new();
        window.set_default_size(400, 200);
        window.set_child(Some(&backend.widget()));
        window.present();
        backend.write(b"\x1b[1mhello\x1b[0m");
        let context = glib::MainContext::default();
        for _ in 0..50 {
            context.iteration(false);
        }

        let view = backend.widget().downcast::<GhosttyView>().unwrap();
        assert!(view.width() > 0 && view.height() > 0);
        let events: Vec<TerminalEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events.iter().any(
                |e| matches!(e, TerminalEvent::SizeChange { rows, cols } if *rows > 0 && *cols > 0)
            ),
            "{events:?}"
        );
        assert_eq!(view.imp().frame.borrow().row_text(0).trim_end(), "hello");

        // Opacity fades the default background only.
        backend.write(b"\r\n\x1b[41m  \x1b[0m");
        backend.set_opacity(0.5);
        assert!(view.has_css_class(TRANSLUCENT_CLASS));
        // The paintable reads the last frame drawn: let the clock tick.
        for _ in 0..50 {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let (cell_w, cell_h) = view.cell_size();
        let faded = alpha_at(&view, view.width() - 2, view.height() - 2);
        assert!((120..=135).contains(&faded), "default background {faded}");
        assert_eq!(alpha_at(&view, cell_w / 2, cell_h + cell_h / 2), 255);
        backend.set_opacity(1.0);
        assert!(!view.has_css_class(TRANSLUCENT_CLASS));

        // A new cell size reaches the engine with the grid unchanged.
        backend.write(b"\x1b[?2048h");
        while rx.try_recv().is_ok() {}
        let (cols, rows) = view.shared().unwrap().engine.size();
        backend.set_font(&pango::FontDescription::from_string("Monospace 20"));
        let (cell_w, cell_h) = view.cell_size();
        let (width, height) = (i32::from(cols) * cell_w, i32::from(rows) * cell_h);
        view.fit_grid(width, height);
        backend.write(b"");
        let report = format!("\x1b[48;{rows};{cols};{height};{width}t").into_bytes();
        assert_eq!(rx.try_recv(), Ok(TerminalEvent::DataOut(report)));
        backend.write(b"\x1b[?2048l");
        backend.set_font(&pango::FontDescription::from_string(DEFAULT_FONT));
        view.fit_grid(view.width(), view.height());
        while rx.try_recv().is_ok() {}

        assert!(view.key(gdk::Key::a, gdk::ModifierType::empty()));
        assert_eq!(
            rx.try_recv().unwrap(),
            TerminalEvent::DataOut(b"a".to_vec())
        );
        assert!(!view.key(gdk::Key::Shift_L, gdk::ModifierType::empty()));

        // Shift+PageUp scrolls the viewport and sends nothing.
        for line in 0..200 {
            backend.write(format!("line {line}\r\n").as_bytes());
        }
        let top_row = || {
            let mut frame = Frame::default();
            assert!(view.shared().unwrap().engine.snapshot(&mut frame));
            frame.row_text(0)
        };
        let bottom = top_row();
        assert!(view.key(gdk::Key::Page_Up, gdk::ModifierType::SHIFT_MASK));
        assert_ne!(top_row(), bottom);
        assert!(view.key(gdk::Key::End, gdk::ModifierType::SHIFT_MASK));
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|e| matches!(e, TerminalEvent::DataOut(_))),
            "scroll keys stay local"
        );
        assert_eq!(top_row(), bottom);

        backend.send_input(b"ls\r");
        assert_eq!(
            rx.try_recv().unwrap(),
            TerminalEvent::DataOut(b"ls\r".to_vec())
        );
        mouse_routing(&backend, &view, &mut rx);
        glyphs_on_the_grid(&backend, &view);
        cursor_shapes(&backend, &view);
        clusters_shape_whole(&backend, &view);
        shell_watchers(&backend, &view);
        search_highlight(&backend, &view);
        primary_selection(&backend, &view, &mut rx);
        window.close();

        // The factory follows the installed preference; VTE is the default.
        install_backend_preference(BackendKind::Ghostty);
        assert_eq!(create_terminal().backend.name(), "ghostty");
        install_backend_preference(BackendKind::Vte);
        assert_eq!(create_terminal().backend.name(), "vte");
    }

    /// Buttons, the wheel and focus changes go local or to the program as
    /// its modes say; called from the test above, which owns GTK.
    fn mouse_routing(
        backend: &GhosttyBackend,
        view: &GhosttyView,
        rx: &mut mpsc::Receiver<TerminalEvent>,
    ) {
        let mut chunks = || -> Vec<Vec<u8>> {
            std::iter::from_fn(|| rx.try_recv().ok())
                .filter_map(|e| match e {
                    TerminalEvent::DataOut(bytes) => Some(bytes),
                    _ => None,
                })
                .collect()
        };
        let none = gdk::ModifierType::empty();
        let shift = gdk::ModifierType::SHIFT_MASK;
        let (cell_w, cell_h) = view.cell_size();
        let at = |col: i32, row: i32| {
            (
                f64::from(col * cell_w + cell_w / 2),
                f64::from(row * cell_h + cell_h / 2),
            )
        };
        let engine = || view.shared().unwrap().engine.selection_text();
        let top_row = || {
            let mut frame = Frame::default();
            assert!(view.shared().unwrap().engine.snapshot(&mut frame));
            frame.row_text(0)
        };
        backend.reset();
        for line in 0..200 {
            backend.write(format!("line {line}\r\n").as_bytes());
        }
        chunks();

        // No tracking: a drag selects and the wheel scrolls, sending nothing.
        view.press(none, 1000, at(0, 0));
        view.drag_to(none, at(2, 0));
        view.release(none, at(2, 0));
        assert!(engine().is_some_and(|text| !text.is_empty()));
        let bottom = top_row();
        view.wheel(-1.0, gdk::ScrollUnit::Wheel, none);
        assert_ne!(top_row(), bottom);
        assert_eq!(chunks().concat(), b"");
        backend.scroll_to_bottom();

        // Tracking with SGR: press, drag and release are reported, nothing
        // is selected, and the wheel is three wheel-up reports.
        backend.write(b"\x1b[?1002h\x1b[?1006h");
        view.press(none, 2000, at(1, 1));
        view.drag_to(none, at(1, 1));
        view.drag_to(none, at(3, 1));
        view.release(none, at(3, 1));
        assert_eq!(engine(), None);
        let reports = chunks();
        assert_eq!(reports.concat(), b"\x1b[<0;2;2M\x1b[<32;4;2M\x1b[<0;4;2m");
        // Each report is a chunk of its own, which macro recording skips.
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(|r| is_pointer_or_focus_report(r)));
        view.hover(none, at(0, 0));
        assert_eq!(chunks().concat(), b"", "1002 reports no bare motion");
        view.wheel(-1.0, gdk::ScrollUnit::Wheel, none);
        assert_eq!(
            chunks(),
            vec![b"\x1b[<64;1;1M".to_vec(); 3],
            "a report per chunk"
        );
        assert_eq!(top_row(), bottom, "the viewport stays put");

        // Shift keeps the drag and the wheel local.
        view.press(shift, 3000, at(0, 0));
        view.drag_to(shift, at(2, 0));
        view.release(shift, at(2, 0));
        assert!(engine().is_some_and(|text| !text.is_empty()));
        view.wheel(-1.0, gdk::ScrollUnit::Wheel, shift);
        assert_ne!(top_row(), bottom);
        assert_eq!(chunks().concat(), b"");
        backend.scroll_to_bottom();

        // Alternate screen, no tracking: the wheel sends arrow keys.
        backend.write(b"\x1b[?1002l\x1b[?1006l\x1b[?1049h");
        view.wheel(1.0, gdk::ScrollUnit::Wheel, none);
        assert_eq!(chunks().concat(), b"\x1b[B".repeat(3));
        backend.write(b"\x1b[?1007l");
        view.wheel(1.0, gdk::ScrollUnit::Wheel, none);
        assert_eq!(chunks().concat(), b"", "no alternate scroll");
        backend.write(b"\x1b[?1049l");

        // Focus changes only with mode 1004.
        view.focus_changed(true);
        assert_eq!(chunks().concat(), b"");
        backend.write(b"\x1b[?1004h");
        view.focus_changed(false);
        view.focus_changed(true);
        let reports = chunks();
        assert_eq!(reports.concat(), b"\x1b[O\x1b[I");
        assert!(reports.iter().all(|r| is_pointer_or_focus_report(r)));
    }

    /// The hooks the tab follows the shell by (doc 07 §Following the
    /// shell) see input, a reported directory, and then the output.
    fn shell_watchers(backend: &GhosttyBackend, view: &GhosttyView) {
        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = Rc::clone(&seen);
        view.connect_input(move |data| {
            log.borrow_mut()
                .push(format!("input {}", String::from_utf8_lossy(data)))
        });
        let log = Rc::clone(&seen);
        view.connect_output(move || log.borrow_mut().push("output".into()));
        let log = Rc::clone(&seen);
        view.connect_directory(move |uri| log.borrow_mut().push(format!("directory {uri}")));

        backend.send_input(b"cd /tmp\r");
        assert!(view.key(gdk::Key::a, gdk::ModifierType::empty()));
        backend.write(b"\x1b]7;file://box/tmp\x07");
        assert_eq!(
            *seen.borrow(),
            [
                "input cd /tmp\r",
                "input a",
                "directory file://box/tmp",
                "output"
            ]
        );
    }

    /// Glyphs, the cursor and text lines sit on the cell grid, however far
    /// the font's advances are from the whole-pixel cell width.
    fn glyphs_on_the_grid(backend: &GhosttyBackend, view: &GhosttyView) {
        let metrics = view.metrics();
        let (cw, ch) = (metrics.width as usize, metrics.height as usize);
        backend.reset();
        let (cols, _) = view.shared().unwrap().engine.size();
        let n = usize::from(cols) - 2;
        backend.write("M".repeat(n).as_bytes());
        let inked = render(view);
        for col in 0..n {
            assert!(
                inked(col * cw..(col + 1) * cw, 0..ch),
                "M {col} left its cell"
            );
        }
        // The cursor block comes right after the last M, then nothing.
        assert!(inked(n * cw..(n + 1) * cw, 0..ch));
        assert!(!inked((n + 1) * cw..(n + 2) * cw, 0..ch));

        // Lines cross the blank cells of their runs: cells 1-2 underlined,
        // 6-7 struck through.
        backend.write(b"\r\n\x1b[4ma  b\x1b[24m \x1b[9m_  _\x1b[0m");
        let inked = render(view);
        for (top, cells) in [(metrics.underline.0, [1, 2]), (metrics.strike.0, [6, 7])] {
            let y = ch + top as usize;
            for col in cells {
                let x = col * cw + cw / 2;
                assert!(inked(x..x + 1, y..y + 1), "no line in cell {col}");
            }
        }
    }

    /// The cursor's shapes on an empty cell, the hollow block of an unfocused
    /// view, and the blink: painted whole, on a timer that runs only while
    /// the cursor blinks in a focused, mapped view.
    fn cursor_shapes(backend: &GhosttyBackend, view: &GhosttyView) {
        let metrics = view.metrics();
        let (cw, ch) = (metrics.width as usize, metrics.height as usize);
        let line = metrics.underline.1 as usize;
        let (x, y) = (cw / 2, ch / 2);
        // Which of the cursor cell's middle and its four edges have ink.
        let shape = || {
            let inked = render(view);
            [
                inked(x..x + 1, y..y + 1),
                inked(x..x + 1, 0..line),
                inked(x..x + 1, ch - line..ch),
                inked(0..line, y..y + 1),
                inked(cw - line..cw, y..y + 1),
            ]
        };
        const BLOCK: [bool; 5] = [true; 5];
        const NOTHING: [bool; 5] = [false; 5];
        const UNDERLINE: [bool; 5] = [false, false, true, false, false];
        const BAR: [bool; 5] = [false, false, false, true, false];
        const HOLLOW: [bool; 5] = [false, true, true, true, true];
        let timer = || view.imp().blink.borrow().is_some();

        backend.reset();
        view.focus_changed(true);
        assert_eq!(shape(), BLOCK, "the default");
        for (decscusr, expected) in [(2, BLOCK), (4, UNDERLINE), (6, BAR)] {
            backend.write(format!("\x1b[{decscusr} q").as_bytes());
            assert_eq!(shape(), expected, "DECSCUSR {decscusr}");
            assert!(!timer(), "a steady cursor needs no timer");
            view.focus_changed(false);
            assert_eq!(shape(), HOLLOW, "unfocused after DECSCUSR {decscusr}");
            view.focus_changed(true);
        }

        // Each phase is one whole frame; DECSCUSR's blink variants and mode
        // 12 both start it.
        for (start, on) in [("\x1b[1 q", BLOCK), ("\x1b[3 q", UNDERLINE)] {
            backend.write(start.as_bytes());
            assert_eq!(shape(), on);
            assert!(timer());
            view.blink_tick();
            assert_eq!(shape(), NOTHING);
            view.blink_tick();
            assert_eq!(shape(), on);
        }
        backend.write(b"\x1b[6 q\x1b[?12h");
        assert_eq!(shape(), BAR);
        assert!(timer());

        // The real timer flips the phase and redraws.
        let context = glib::MainContext::default();
        let start = std::time::Instant::now();
        while !view.imp().blink_off.get() && start.elapsed() < 3 * BLINK_INTERVAL {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(view.imp().blink_off.get(), "the timer never ticked");
        assert_eq!(shape(), NOTHING);

        // Focus lost in the off phase: hollow at once, no timer; focus back
        // starts over with the cursor on.
        view.focus_changed(false);
        assert_eq!(shape(), HOLLOW);
        assert!(!timer());
        view.focus_changed(true);
        assert_eq!(shape(), BAR);
        assert!(timer());

        // The blink's end stops the timer, in the off phase too.
        view.blink_tick();
        backend.write(b"\x1b[?12l");
        assert_eq!(shape(), BAR);
        assert!(!timer());
        assert!(!view.imp().blink_off.get());

        // So does hiding the cursor, until it is back.
        backend.write(b"\x1b[1 q");
        assert_eq!(shape(), BLOCK);
        assert!(timer());
        backend.write(b"\x1b[?25l");
        assert_eq!(shape(), NOTHING);
        assert!(!timer());
        backend.write(b"\x1b[?25h");
        assert_eq!(shape(), BLOCK);
        assert!(timer());

        // Unmapped, nothing ticks; the paint after the map starts it again.
        view.blink_tick();
        view.set_visible(false);
        assert!(!view.is_mapped());
        assert!(!timer());
        assert!(!view.imp().blink_off.get());
        view.paint(&gtk4::Snapshot::new());
        assert!(!timer());
        view.set_visible(true);
        for _ in 0..50 {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(view.is_mapped());
        assert!(timer());

        backend.reset();
        assert_eq!(shape(), BLOCK);
        assert!(!timer());
    }

    /// Double and triple clicks select a word and a line, a finished
    /// selection reaches PRIMARY, and a middle click pastes it unless the
    /// program tracks the mouse.
    fn primary_selection(
        backend: &GhosttyBackend,
        view: &GhosttyView,
        rx: &mut mpsc::Receiver<TerminalEvent>,
    ) {
        let none = gdk::ModifierType::empty();
        let shift = gdk::ModifierType::SHIFT_MASK;
        let (cell_w, cell_h) = view.cell_size();
        let at = |col: i32, row: i32| {
            (
                f64::from(col * cell_w + cell_w / 2),
                f64::from(row * cell_h + cell_h / 2),
            )
        };
        let click = |state, time, col| {
            view.press(state, time, at(col, 0));
            view.release(state, at(col, 0));
        };
        while rx.try_recv().is_ok() {}
        let context = glib::MainContext::default();
        let primary = || {
            let read = view.primary_clipboard().read_text_future();
            context.block_on(read).unwrap().map(String::from)
        };
        let mut pasted = |state| {
            view.paste_primary(state);
            let mut sent = Vec::new();
            for _ in 0..100 {
                while context.iteration(false) {}
                while let Ok(event) = rx.try_recv() {
                    if let TerminalEvent::DataOut(bytes) = event {
                        sent.extend(bytes);
                    }
                }
                if !sent.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            sent
        };

        backend.reset();
        backend.write(b"alpha beta gamma");
        view.primary_clipboard().set_text("before");
        click(none, 10_000, 7);
        assert_eq!(
            primary().as_deref(),
            Some("before"),
            "a click selects nothing"
        );
        click(none, 10_100, 7);
        assert_eq!(backend.selected_text().as_deref(), Some("beta"));
        assert_eq!(primary().as_deref(), Some("beta"));
        click(none, 10_200, 7);
        assert_eq!(primary().as_deref(), Some("alpha beta gamma"));

        // A drag reaches PRIMARY on release, not along the way.
        view.primary_clipboard().set_text("before");
        view.press(none, 20_000, at(0, 0));
        view.drag_to(none, at(4, 0));
        assert_eq!(primary().as_deref(), Some("before"));
        view.release(none, at(4, 0));
        assert!(primary().is_some_and(|text| text.starts_with("alph")));

        backend.select_all();
        assert!(primary().is_some_and(|text| text.contains("alpha beta gamma")));

        view.primary_clipboard().set_text("a\nb");
        assert_eq!(pasted(none), b"a\rb");
        backend.write(b"\x1b[?2004h");
        assert_eq!(pasted(none), b"\x1b[200~a\nb\x1b[201~");
        backend.write(b"\x1b[?2004l\x1b[?1000h");
        assert_eq!(pasted(none), b"", "the program owns the mouse");
        assert_eq!(pasted(shift), b"a\rb", "Shift keeps it local");

        // A reported press ends a click sequence, and Shift still selects.
        click(shift, 30_000, 7);
        click(none, 30_100, 7);
        click(shift, 30_200, 7);
        assert_eq!(backend.selected_text(), None);
        click(shift, 30_300, 7);
        assert_eq!(backend.selected_text().as_deref(), Some("beta"));
        backend.write(b"\x1b[?1000l");
        while rx.try_recv().is_ok() {}
    }

    /// Search lights its matches and the current one apart, under the
    /// selection; an empty query or a press puts them out. The popover the
    /// search actions open sits on this widget as it does on VTE's.
    fn search_highlight(backend: &GhosttyBackend, view: &GhosttyView) {
        let features = backend.features();
        assert!(features.search && features.scrollback_search);
        let (cw, _) = view.cell_size();
        let cell = |col: usize| col * cw as usize + 1;
        let background = || {
            let at = pixels(view);
            at(view.width() as usize - 1, view.height() as usize - 1)
        };
        // The cell's top left: its background, never a glyph.
        let color = |col| pixels(view)(cell(col), 1);
        let opaque = |[r, g, b]: [u8; 3]| [b, g, r, 255];

        backend.reset();
        backend.write(b"one two one");
        assert_eq!(color(0), background(), "no search, no highlight");
        backend.search("one", false);
        assert_eq!(color(0), opaque(SEARCH_CURRENT_BG));
        assert_eq!(color(4), background());
        assert_eq!(color(8), opaque(SEARCH_BG));
        // Forward again: the current match moves, the first stays lit.
        backend.search("one", false);
        assert_eq!(color(0), opaque(SEARCH_BG));
        assert_eq!(color(8), opaque(SEARCH_CURRENT_BG));

        let engine = || view.shared().unwrap().engine.selection_text();
        // On the engine: a press through the view would end the search.
        let shared = view.shared().unwrap();
        shared.engine.selection_cancel();
        assert!(shared
            .engine
            .selection_press((0, 0), (1.0, 1.0), 0, 400, 1.0));
        let end = (f64::from(3 * cw - 1), 1.0);
        assert!(shared
            .engine
            .selection_drag((2, 0), end, cw as u32, view.height() as u32));
        assert_ne!(color(0), opaque(SEARCH_BG), "the selection is on top");
        assert_eq!(engine().as_deref(), Some("one"));
        view.shared().unwrap().engine.clear_selection();
        assert_eq!(color(0), opaque(SEARCH_BG));

        backend.search("", false);
        assert_eq!(color(0), background());
        assert_eq!(color(8), background());

        backend.search("one", false);
        assert_eq!(color(0), opaque(SEARCH_CURRENT_BG));
        view.press(gdk::ModifierType::empty(), 5000, (1.0, 1.0));
        view.release(gdk::ModifierType::empty(), (1.0, 1.0));
        assert_eq!(color(0), background());

        backend.search("one", false);
        assert!(view.key(gdk::Key::a, gdk::ModifierType::empty()));
        assert_eq!(color(0), background());

        let popover = gtk4::Popover::new();
        popover.set_child(Some(&gtk4::Entry::new()));
        popover.set_parent(view);
        popover.popup();
        let context = glib::MainContext::default();
        for _ in 0..50 {
            context.iteration(false);
        }
        assert!(popover.is_visible());
        popover.popdown();
        popover.unparent();
    }

    /// A cell's whole cluster reaches Pango: a ZWJ family shapes to other
    /// glyphs than its first emoji alone (the family glyph with an emoji
    /// font, a glyph per codepoint without one), all at the family's cell,
    /// and the cells after it stay on the grid. Likewise a decomposed é.
    fn clusters_shape_whole(backend: &GhosttyBackend, view: &GhosttyView) {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        backend.reset();
        // Mode 2027 makes the family one wide cell.
        backend.write(b"\x1b[?2027h");
        for row in [family, "\u{1f468}", "e\u{301}", "e"] {
            backend.write(format!("x{row}y\r\n").as_bytes());
        }
        let mut frame = Frame::default();
        assert!(view.shared().unwrap().engine.snapshot(&mut frame));
        // Each glyph of a row's first four cells: (glyph, column, x).
        let shaped = |row: usize| {
            let (text, columns) = run_text(&painted_row(&frame, row, &[], true)[..4]);
            let mut glyphs = Vec::new();
            view.shape(&text, &columns, &view.font(), |_, string, columns| {
                for (glyph, &col) in string.glyph_info().iter().zip(columns) {
                    glyphs.push((glyph.glyph(), col, glyph.geometry().x_offset()));
                }
            });
            glyphs
        };
        let at = |glyphs: &[(u32, usize, i32)], col: usize| -> Vec<u32> {
            glyphs
                .iter()
                .filter(|&&(_, c, _)| c == col)
                .map(|&(glyph, ..)| glyph)
                .collect()
        };

        let (families, man) = (shaped(0), shaped(1));
        let cluster = at(&families, 1);
        assert_ne!(cluster, at(&man, 1), "only the first emoji was shaped");
        assert!(
            cluster.len() == 1 || cluster.len() == family.chars().count(),
            "{cluster:?}"
        );
        // x at 0, the family at 1, its tail at 2, y at 3, each on its cell.
        let cell = view.metrics().width * pango::SCALE;
        let mut columns: Vec<usize> = families.iter().map(|&(_, col, _)| col).collect();
        columns.dedup();
        assert_eq!(columns, [0, 1, 2, 3]);
        for &(_, col, x) in families.iter().filter(|&&(_, col, _)| col != 1) {
            assert_eq!(x, col as i32 * cell, "column {col}");
        }

        let (decomposed, plain) = (shaped(2), shaped(3));
        assert_ne!(at(&decomposed, 1), at(&plain, 1), "the accent was dropped");
        assert_eq!(at(&decomposed, 2).len(), 1, "y follows in the next cell");
    }

    /// Paint the view now; the result tells whether any pixel in a box
    /// differs from the background.
    fn render(view: &GhosttyView) -> impl Fn(Range<usize>, Range<usize>) -> bool {
        let (width, height) = (view.width() as usize, view.height() as usize);
        let at = pixels(view);
        let background = at(width - 1, height - 1);
        move |xs, ys| {
            xs.flat_map(|x| ys.clone().map(move |y| (x, y)))
                .any(|(x, y)| at(x, y) != background)
        }
    }

    /// Paint the view now; the result reads one pixel, as GDK stores it:
    /// blue, green, red, alpha.
    fn pixels(view: &GhosttyView) -> impl Fn(usize, usize) -> [u8; 4] {
        let (width, height) = (view.width(), view.height());
        let snapshot = gtk4::Snapshot::new();
        view.paint(&snapshot);
        let renderer = view.native().and_then(|n| n.renderer()).unwrap();
        let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
        let texture = renderer.render_texture(snapshot.to_node().unwrap(), Some(&bounds));
        let stride = width as usize * 4;
        let mut pixels = vec![0; stride * height as usize];
        texture.download(&mut pixels, stride);
        move |x, y| {
            let i = y * stride + x * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        }
    }
}
