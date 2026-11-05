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
//! selection and the cursor. Kitty images come from the same snapshot,
//! each in its layer: over the default background, over the cell
//! backgrounds, or over the text.
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
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk, pango};

use super::engine::{
    push_cluster, Cell as GridCell, Frame, FrameInfo, MouseEvent, MouseModes, Pixels, Placement,
    Scroll, Span,
};
use super::ffi;
use super::Shared;
use crate::terminal::TerminalEvent;

/// The font a view starts with, until `set_font`.
const DEFAULT_FONT: &str = "Monospace 12";
/// Rows per wheel notch.
const WHEEL_ROWS: f64 = 3.0;
/// The arrow keysyms alternate scroll sends.
const ARROW_UP: u32 = 0xff52;
const ARROW_DOWN: u32 = 0xff54;
/// Search highlight colors, Ghostty's defaults: dark text on yellow, on
/// orange for the current match.
const SEARCH_FG: [u8; 3] = [0, 0, 0];
const SEARCH_BG: [u8; 3] = [0xff, 0xe0, 0x82];
const SEARCH_CURRENT_BG: [u8; 3] = [0xf2, 0xa5, 0x7e];
/// The character of a Kitty placeholder cell; the image takes its place.
const KITTY_PLACEHOLDER: u32 = 0x10_eeee;
/// The shortest wait for an animation's next frame, in milliseconds.
const MIN_ANIMATION_DELAY: u64 = 8;

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
        /// The images the last frame drew, by ID and pixel generation.
        pub(super) textures: RefCell<HashMap<(u32, u64), gdk::Texture>>,
        /// A redraw for an animation's next frame is on its way.
        pub(super) animating: Cell<bool>,
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
    /// mode 1004.
    fn focus_changed(&self, gained: bool) {
        if let Some(shared) = self.shared() {
            shared.input(shared.engine.encode_focus(gained));
        }
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
        let now_ms = glib::monotonic_time().unsigned_abs() / 1000;
        let next_ms = shared.engine.tick_animations(now_ms);
        let mut frame = self.imp().frame.borrow_mut();
        if !shared.engine.snapshot(&mut frame) {
            return;
        }
        if let Some(next_ms) = next_ms {
            self.redraw_after(next_ms.saturating_sub(now_ms));
        }
        self.update_textures(&shared, &frame);
        let info = frame.info;
        let metrics = self.metrics();
        let (cw, ch) = (metrics.width as f32, metrics.height as f32);
        snapshot.append_color(
            &rgba(info.bg, self.imp().opacity.get()),
            &graphene::Rect::new(0.0, 0.0, self.width() as f32, self.height() as f32),
        );

        let font = self.font();
        let (cols, rows) = (usize::from(info.cols), usize::from(info.rows));
        let marks = search_marks(&shared.engine.search_spans(), cols, rows);
        let painted: Vec<Vec<Painted>> = (0..rows)
            .map(|row| painted_row(&frame, row, &marks))
            .collect();

        self.paint_images(snapshot, &frame, ffi::LAYER_BELOW_BG);
        for (row, cells) in painted.iter().enumerate() {
            paint_backgrounds(snapshot, cells, row as f32 * ch, (cw, ch));
        }
        self.paint_images(snapshot, &frame, ffi::LAYER_BELOW_TEXT);
        for (row, cells) in painted.iter().enumerate() {
            self.paint_text(snapshot, cells, row as f32 * ch, &metrics, &font);
        }
        self.paint_images(snapshot, &frame, ffi::LAYER_ABOVE_TEXT);
    }

    /// Redraw in `delay_ms`, for an animation's next frame. It asks for one
    /// frame, which asks for the next: a view off screen draws none, so
    /// its animations wait until it shows again.
    fn redraw_after(&self, delay_ms: u64) {
        if self.imp().animating.replace(true) {
            return;
        }
        let view = self.downgrade();
        let delay = Duration::from_millis(delay_ms.max(MIN_ANIMATION_DELAY));
        glib::timeout_add_local_once(delay, move || {
            if let Some(view) = view.upgrade() {
                view.imp().animating.set(false);
                view.queue_draw();
            }
        });
    }

    /// Hold the textures this frame draws and no others: an image's pixels
    /// are read only when its generation changes, and what is deleted or
    /// off screen is let go.
    fn update_textures(&self, shared: &Shared, frame: &Frame) {
        let mut textures = self.imp().textures.borrow_mut();
        let mut last = std::mem::take(&mut *textures);
        for placement in &frame.placements {
            let key = (placement.image_id, placement.generation);
            if textures.contains_key(&key) {
                continue;
            }
            let texture = last.remove(&key).or_else(|| {
                let pixels = shared.engine.image_pixels(placement.image_id)?;
                texture(pixels)
            });
            if let Some(texture) = texture {
                textures.insert(key, texture);
            }
        }
    }

    /// Draw one layer's images, clipped to the grid. `frame` holds them in
    /// drawing order.
    fn paint_images(&self, snapshot: &gtk4::Snapshot, frame: &Frame, layer: u8) {
        let textures = self.imp().textures.borrow();
        let (cell_w, cell_h) = self.cell_size();
        let grid = graphene::Rect::new(
            0.0,
            0.0,
            (i32::from(frame.info.cols) * cell_w) as f32,
            (i32::from(frame.info.rows) * cell_h) as f32,
        );
        for placement in frame.placements.iter().filter(|p| p.layer == layer) {
            let Some(texture) = textures.get(&(placement.image_id, placement.generation)) else {
                continue;
            };
            let size = (texture.width() as f32, texture.height() as f32);
            let Some((target, whole)) = image_rects(placement, (cell_w, cell_h), size) else {
                continue;
            };
            let Some(clip) = target.intersection(&grid) else {
                continue;
            };
            snapshot.push_clip(&clip);
            snapshot.append_texture(texture, &whole);
            snapshot.pop();
        }
    }

    /// One row's text at `y`: one run per style, each cluster at its own
    /// cell.
    fn paint_text(
        &self,
        snapshot: &gtk4::Snapshot,
        cells: &[Painted],
        y: f32,
        metrics: &Metrics,
        font: &pango::FontDescription,
    ) {
        let cw = metrics.width as f32;
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
                &style.font(font),
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

/// One row's backgrounds at `y`: one rectangle per run of equal,
/// non-default color.
fn paint_backgrounds(snapshot: &gtk4::Snapshot, cells: &[Painted], y: f32, (cw, ch): (f32, f32)) {
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
}

/// An image's pixels as a texture. Kitty's alpha is straight, as these
/// formats take it. `None` if they do not add up to the image's size.
fn texture(pixels: Pixels) -> Option<gdk::Texture> {
    let (format, bytes) = match pixels.format {
        ffi::FORMAT_RGB => (gdk::MemoryFormat::R8g8b8, 3),
        ffi::FORMAT_RGBA => (gdk::MemoryFormat::R8g8b8a8, 4),
        ffi::FORMAT_GRAY_ALPHA => (gdk::MemoryFormat::G8a8, 2),
        ffi::FORMAT_GRAY => (gdk::MemoryFormat::G8, 1),
        _ => return None,
    };
    let stride = pixels.width as usize * bytes;
    let width = i32::try_from(pixels.width).ok().filter(|&w| w > 0)?;
    let height = i32::try_from(pixels.height).ok().filter(|&h| h > 0)?;
    if pixels.data.len() != stride * pixels.height as usize {
        return None;
    }
    let data = glib::Bytes::from_owned(pixels.data);
    Some(gdk::MemoryTexture::new(width, height, format, &data, stride).upcast())
}

/// Where a placement goes, in widget pixels: its target rectangle, and
/// the rectangle the whole `image` scales to so that the source rectangle
/// lands on the target. Drawing the whole image clipped to the target
/// crops it with no copy. `None` for a placement with nothing to show.
fn image_rects(
    placement: &Placement,
    (cell_w, cell_h): (i32, i32),
    (image_w, image_h): (f32, f32),
) -> Option<(graphene::Rect, graphene::Rect)> {
    if placement.source_width == 0 || placement.source_height == 0 {
        return None;
    }
    let x = (i64::from(placement.col) * i64::from(cell_w) + i64::from(placement.offset_x)) as f32;
    let y = (i64::from(placement.row) * i64::from(cell_h) + i64::from(placement.offset_y)) as f32;
    let (width, height) = (placement.width as f32, placement.height as f32);
    let scale_x = width / placement.source_width as f32;
    let scale_y = height / placement.source_height as f32;
    let whole = graphene::Rect::new(
        x - placement.source_x as f32 * scale_x,
        y - placement.source_y as f32 * scale_y,
        image_w * scale_x,
        image_h * scale_y,
    );
    Some((graphene::Rect::new(x, y, width, height), whole))
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
/// the selection and the cursor still draw on top.
fn painted_row<'a>(frame: &'a Frame, row: usize, marks: &[Option<bool>]) -> Vec<Painted<'a>> {
    let info = &frame.info;
    let cols = usize::from(info.cols);
    (0..cols)
        .filter_map(|col| frame.cell(row, col).map(|cell| (col, cell)))
        .map(|(col, cell)| {
            let cursor = is_cursor(info, row, col);
            let cluster = match frame.grapheme(row, col) {
                [KITTY_PLACEHOLDER, ..] => &[],
                cluster => cluster,
            };
            let painted = Painted::new(info, cell, cluster, cursor);
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
        let (text, columns) = run_text(&painted_row(&frame, 0, &[])[..5]);
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
    fn placeholder_cells_draw_no_text() {
        let engine = super::super::engine::Engine::new(4, 1).expect("engine");
        engine.write("a\u{10eeee}\u{305}\u{305}b".as_bytes());
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let (text, _) = run_text(&painted_row(&frame, 0, &[])[..3]);
        assert_eq!(text, "a b");
    }

    fn placement(col: i32, row: i32) -> Placement {
        Placement {
            col,
            row,
            width: 40,
            height: 60,
            source_width: 40,
            source_height: 60,
            ..Placement::default()
        }
    }

    fn sides(rect: &graphene::Rect) -> [f32; 4] {
        [rect.x(), rect.y(), rect.width(), rect.height()]
    }

    #[test]
    fn an_image_sits_at_its_cell_plus_its_offset() {
        let offset = Placement {
            offset_x: 3,
            offset_y: 5,
            ..placement(2, 1)
        };
        let (target, whole) = image_rects(&offset, (10, 20), (40.0, 60.0)).unwrap();
        assert_eq!(sides(&target), [23.0, 25.0, 40.0, 60.0]);
        assert_eq!(sides(&whole), sides(&target));
        // Above the viewport it starts off the widget, to be clipped.
        let (target, _) = image_rects(&placement(0, -2), (10, 20), (40.0, 60.0)).unwrap();
        assert_eq!(sides(&target), [0.0, -40.0, 40.0, 60.0]);
    }

    #[test]
    fn the_source_rectangle_lands_on_the_target() {
        // The right half of a 40 × 60 image, drawn twice its size.
        let half = Placement {
            width: 40,
            height: 120,
            source_x: 20,
            source_width: 20,
            ..placement(1, 0)
        };
        let (target, whole) = image_rects(&half, (10, 20), (40.0, 60.0)).unwrap();
        assert_eq!(sides(&target), [10.0, 0.0, 40.0, 120.0]);
        assert_eq!(sides(&whole), [-30.0, 0.0, 80.0, 120.0]);
    }

    #[test]
    fn an_empty_source_rectangle_draws_nothing() {
        let empty = Placement {
            source_width: 0,
            ..placement(0, 0)
        };
        assert!(image_rects(&empty, (10, 20), (40.0, 60.0)).is_none());
    }

    #[test]
    fn every_stored_format_makes_a_texture() {
        let pixels = |format, data: &[u8]| Pixels {
            width: 2,
            height: 1,
            format,
            data: data.to_vec(),
        };
        for (format, bytes) in [
            (ffi::FORMAT_RGB, 3),
            (ffi::FORMAT_RGBA, 4),
            (ffi::FORMAT_GRAY_ALPHA, 2),
            (ffi::FORMAT_GRAY, 1),
        ] {
            let data = vec![9; 2 * bytes];
            let made = texture(pixels(format, &data)).expect("texture");
            assert_eq!((made.width(), made.height()), (2, 1));
            assert!(texture(pixels(format, &data[1..])).is_none(), "short");
        }
        assert!(texture(pixels(2, &[0; 8])).is_none(), "PNG is never stored");
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
        clusters_shape_whole(&backend, &view);
        shell_watchers(&backend, &view);
        search_highlight(&backend, &view);
        primary_selection(&backend, &view, &mut rx);
        kitty_images(&backend, &view);
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
            let (text, columns) = run_text(&painted_row(&frame, row, &[])[..4]);
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

    /// Images land at their cells in their layers, scroll with the text,
    /// go when deleted, and animate; called from the test above, which
    /// owns GTK.
    fn kitty_images(backend: &GhosttyBackend, view: &GhosttyView) {
        const RED: [u8; 4] = [0, 0, 255, 255];
        const GREEN: [u8; 4] = [0, 255, 0, 255];
        assert!(backend.features().kitty_graphics);
        let (cell_w, cell_h) = view.cell_size();
        let (w, h) = (cell_w as usize, cell_h as usize);
        // An image filling `cols` × `rows` cells, in one color.
        let show = |id: u32, [b, g, r, a]: [u8; 4], (cols, rows): (usize, usize), rest: &str| {
            let (width, height) = (cols * w, rows * h);
            let data = [r, g, b, a].repeat(width * height);
            let control = format!("a=T,f=32,i={id},s={width},v={height},q=2{rest}");
            let data = glib::base64_encode(&data);
            backend.write(format!("\x1b_G{control};{data}\x1b\\").as_bytes());
        };
        // The middle of a cell.
        let mid = |at: &dyn Fn(usize, usize) -> [u8; 4], col: usize, row: usize| {
            at(col * w + w / 2, row * h + h / 2)
        };
        // With the cursor hidden: it would sit on the cell after an image.
        let fresh = || {
            backend.reset();
            backend.write(b"\x1b[?25l");
        };
        fresh();
        let background = pixels(view)(view.width() as usize - 1, view.height() as usize - 1);

        // At its cell, and not beside it.
        backend.write(b"\x1b[2;3H");
        show(1, RED, (2, 1), "");
        let at = pixels(view);
        assert_eq!(mid(&at, 2, 1), RED);
        assert_eq!(mid(&at, 3, 1), RED);
        assert_eq!(mid(&at, 1, 1), background);
        assert_eq!(mid(&at, 4, 1), background);
        assert_eq!(mid(&at, 2, 0), background);
        assert_eq!(mid(&at, 2, 2), background);

        // Scrolls with the text, is clipped at the top, then goes.
        backend.write(b"\x1b[999;1H\r\n");
        assert_eq!(mid(&pixels(view), 2, 0), RED, "moved up a row");
        backend.write(b"\r\n");
        assert_eq!(mid(&pixels(view), 2, 0), background, "scrolled off");
        assert!(view.imp().textures.borrow().is_empty(), "texture kept");
        view.shared().unwrap().engine.scroll(Scroll::By(-1));
        assert_eq!(mid(&pixels(view), 2, 0), RED, "back in the scrollback");
        view.shared().unwrap().engine.scroll(Scroll::Bottom);

        // Partly above the viewport, what is left shows.
        fresh();
        backend.write(b"\x1b[999;1H");
        show(10, RED, (1, 2), ",C=1");
        let rows = usize::from(view.shared().unwrap().engine.size().1);
        backend.write(b"\x1b[999;1H");
        backend.write("\r\n".repeat(rows).as_bytes());
        let at = pixels(view);
        assert_eq!(view.imp().frame.borrow().placements[0].row, -1);
        assert_eq!(mid(&at, 0, 0), RED);
        assert_eq!(mid(&at, 0, 1), background);

        // Past the grid's edge it is clipped, even with room to draw.
        fresh();
        let (cols, rows) = view.shared().unwrap().engine.size();
        view.request_grid(rows, cols - 1);
        let last = usize::from(cols) - 2;
        backend.write(format!("\x1b[1;{}H", last + 1).as_bytes());
        show(11, RED, (3, 1), ",C=1");
        let at = pixels(view);
        assert_eq!(mid(&at, last, 0), RED);
        assert_eq!(mid(&at, last + 1, 0), background, "past the last column");
        view.request_grid(rows, cols);

        // Deleted, it goes, along with its texture.
        fresh();
        show(2, RED, (1, 1), "");
        assert_eq!(mid(&pixels(view), 0, 0), RED);
        backend.write(b"\x1b_Ga=d,d=I,i=2,q=2\x1b\\");
        assert_eq!(mid(&pixels(view), 0, 0), background);
        assert!(view.imp().textures.borrow().is_empty(), "texture kept");

        // Below text: over a colored cell, under its text. A block fills
        // the middle of its cell, so text shows through at that pixel.
        fresh();
        backend.write("\x1b[1;1H\x1b[44m \u{2588}\x1b[0m\x1b[1;1H".as_bytes());
        show(3, RED, (2, 1), ",z=-1,C=1");
        let at = pixels(view);
        assert_eq!(mid(&at, 0, 0), RED, "over the cell's background");
        assert_ne!(mid(&at, 1, 0), RED, "under the text");

        // Below backgrounds: over the default one, under a colored cell.
        fresh();
        backend.write(b"\x1b[1;1H\x1b[44m \x1b[0m\x1b[1;1H");
        show(4, RED, (2, 1), ",z=-1073741825,C=1");
        let at = pixels(view);
        assert_ne!(mid(&at, 0, 0), RED, "under the colored cell");
        assert_eq!(mid(&at, 1, 0), RED, "over the default background");

        // Above text, and higher z over lower.
        fresh();
        backend.write("\x1b[1;1H\u{2588}\x1b[1;1H".as_bytes());
        show(5, GREEN, (1, 1), ",z=2,C=1");
        show(6, RED, (1, 1), ",z=1,C=1");
        assert_eq!(mid(&pixels(view), 0, 0), GREEN);

        // The source rectangle crops: the right half of red | green.
        fresh();
        let row: Vec<u8> = [[255, 0, 0, 255].repeat(w), [0, 255, 0, 255].repeat(w)].concat();
        let data = glib::base64_encode(&row.repeat(h));
        let (width, half) = (2 * w, w);
        let control = format!("a=T,f=32,i=7,s={width},v={h},x={half},w={half},q=2");
        backend.write(format!("\x1b_G{control};{data}\x1b\\").as_bytes());
        let at = pixels(view);
        assert_eq!(mid(&at, 0, 0), GREEN);
        assert_eq!(mid(&at, 1, 0), background);

        // A placeholder cell shows the image, with no glyph over it.
        fresh();
        let (width, height) = (w, h);
        let data = glib::base64_encode(&[255, 0, 0, 255].repeat(width * height));
        let control = format!("a=T,U=1,f=32,i=8,s={width},v={height},c=1,r=1,q=2");
        backend.write(format!("\x1b_G{control};{data}\x1b\\").as_bytes());
        backend.write("\x1b[38;5;8m\u{10eeee}\u{305}\u{305}\x1b[39m".as_bytes());
        let at = pixels(view);
        for (x, y) in [(1, 1), (w / 2, h / 2), (w - 2, h - 2)] {
            assert_eq!(at(x, y), RED, "({x}, {y})");
        }

        // An animation runs by itself: the view asks for its own frames.
        fresh();
        let frame = |control: &str, pixel: [u8; 3]| {
            let [r, g, b] = pixel;
            let data = glib::base64_encode(&[r, g, b, 255].repeat(w * h));
            let size = format!("f=32,s={w},v={h},q=2");
            backend.write(format!("\x1b_G{control},{size};{data}\x1b\\").as_bytes());
        };
        frame("a=T,i=9", [255, 0, 0]);
        frame("a=f,i=9,z=40", [0, 255, 0]);
        backend.write(b"\x1b_Ga=a,i=9,r=1,z=40,q=2\x1b\\");
        backend.write(b"\x1b_Ga=a,i=9,s=3,q=2\x1b\\");
        // Only the main loop runs here, so each frame is one GTK drew.
        let context = glib::MainContext::default();
        let mut frames = std::collections::HashSet::new();
        for _ in 0..500 {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
            let drawn = view.imp().frame.borrow();
            frames.extend(drawn.placements.iter().map(|p| p.generation));
            if frames.len() >= 3 {
                break;
            }
        }
        assert!(frames.len() >= 3, "{frames:?}");
        backend.reset();
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
