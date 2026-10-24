//! [`GhosttyView`]: the GTK4 widget that draws a libghostty-vt grid.
//!
//! Each frame snapshots the engine's viewport and paints it with
//! `GtkSnapshot`: one background rectangle per run of equal colors, one
//! Pango layout per run of equal style. ASCII runs share a layout; any
//! other character gets its own, anchored to its column, so wide glyphs
//! never shift the rest of the row. Only a grapheme's base codepoint is
//! drawn.
//!
//! Input: keys go through libghostty-vt's key encoder (so cursor-key and
//! Kitty keyboard modes apply) and leave as `DataOut`; Shift+PageUp/
//! PageDown/Home/End scroll the viewport as in VTE. Mouse events go where
//! [`route`] says: to the program through the mouse encoder while it
//! tracks the mouse, as arrow keys for the wheel under alternate scroll,
//! else local, where a primary-button drag selects and the wheel scrolls
//! the viewport three rows per notch (smooth steps accumulate). Shift
//! keeps them local. Focus changes are reported when the program asks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, pango};

use super::engine::{Cell as GridCell, Frame, FrameInfo, MouseEvent, MouseModes, Scroll};
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

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GhosttyView {
        pub(super) shared: RefCell<Option<Rc<Shared>>>,
        pub(super) frame: RefCell<Frame>,
        pub(super) font: RefCell<Option<pango::FontDescription>>,
        /// The default background's alpha; colored cells stay opaque.
        pub(super) opacity: Cell<f32>,
        /// Cell size in pixels; recomputed after a font change.
        pub(super) cell: Cell<Option<(i32, i32)>>,
        /// The grid asked for by `set_size`: the natural size.
        pub(super) requested: Cell<(u16, u16)>,
        /// The last grid reported as `SizeChange`, as `(cols, rows)`.
        pub(super) reported: Cell<Option<(u16, u16)>>,
        /// Where the current selection drag started, `(col, row)`.
        pub(super) anchor: Cell<Option<(u16, u16)>>,
        /// Wheel rows not yet scrolled; see [`super::wheel_rows`].
        pub(super) wheel_carry: Cell<f64>,
        /// Where the primary-button drag in progress goes.
        pub(super) drag: Cell<Option<super::Route>>,
        /// The last pointer position, for wheel reports.
        pub(super) pointer: Cell<(f64, f64)>,
        /// The cell of the last reported motion, so each cell is sent once.
        pub(super) motion_cell: Cell<Option<(u16, u16)>>,
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

    pub(super) fn set_font(&self, font: &pango::FontDescription) {
        self.imp().font.replace(Some(font.clone()));
        self.imp().cell.set(None);
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

    /// One cell in whole pixels, measured from the font's "M".
    fn cell_size(&self) -> (i32, i32) {
        if let Some(cell) = self.imp().cell.get() {
            return cell;
        }
        let layout = self.create_pango_layout(Some("M"));
        layout.set_font_description(Some(&self.font()));
        let (w, h) = layout.pixel_size();
        let cell = (w.max(1), h.max(1));
        self.imp().cell.set(Some(cell));
        cell
    }

    /// Match the grid to the allocation and report a changed grid once.
    fn fit_grid(&self, width: i32, height: i32) {
        let Some(shared) = self.shared() else { return };
        let (cell_w, cell_h) = self.cell_size();
        let fit =
            |pixels: i32, cell: i32| u16::try_from((pixels / cell).max(1)).unwrap_or(u16::MAX);
        let grid = (fit(width, cell_w), fit(height, cell_h));
        if shared.engine.size() != grid {
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
                view.press(drag.current_event_state(), (x, y));
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

    /// Primary button down at widget pixels `at`: report it, or start a
    /// selection. The whole drag goes the same way.
    fn press(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        let imp = self.imp();
        let route = route(shared.engine.mouse_modes(), state, false);
        imp.drag.set(Some(route));
        imp.pointer.set(at);
        shared.engine.clear_selection();
        if route == Route::Report {
            imp.anchor.set(None);
            self.report(&shared, ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, state, at);
        } else {
            imp.anchor.set(Some(self.cell_at(at.0, at.1)));
        }
        self.queue_draw();
    }

    /// The primary button dragged to `at`.
    fn drag_to(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        let imp = self.imp();
        imp.pointer.set(at);
        if imp.drag.get() == Some(Route::Report) {
            self.report_motion(&shared, ffi::BUTTON_LEFT, state, at);
        } else if let Some(anchor) = imp.anchor.get() {
            shared.engine.select(anchor, self.cell_at(at.0, at.1));
            self.queue_draw();
        }
    }

    /// The primary button released at `at`.
    fn release(&self, state: gdk::ModifierType, at: (f64, f64)) {
        let Some(shared) = self.shared() else { return };
        if self.imp().drag.take() == Some(Route::Report) {
            self.report(&shared, ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, state, at);
        }
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
        let (cell_w, cell_h) = self.cell_size();
        let (cw, ch) = (cell_w as f32, cell_h as f32);
        snapshot.append_color(
            &rgba(info.bg, self.imp().opacity.get()),
            &graphene::Rect::new(0.0, 0.0, self.width() as f32, self.height() as f32),
        );

        let font = self.font();
        for row in 0..usize::from(info.rows) {
            let y = row as f32 * ch;
            let cells: Vec<Painted> = (0..usize::from(info.cols))
                .filter_map(|col| frame.cell(row, col).map(|cell| (col, cell)))
                .map(|(col, cell)| Painted::new(&info, cell, is_cursor(&info, row, col)))
                .collect();

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

            // Text: ASCII runs of one style share a layout.
            let mut col = 0;
            while col < cells.len() {
                let start = col;
                let mut text = String::from(cells[col].ch);
                col += 1;
                if cells[start].ch.is_ascii() {
                    while col < cells.len()
                        && cells[col].ch.is_ascii()
                        && cells[col].same_style(&cells[start])
                    {
                        text.push(cells[col].ch);
                        col += 1;
                    }
                }
                let style = &cells[start];
                if style.flags & ffi::CELL_INVISIBLE != 0 || text.trim().is_empty() {
                    continue;
                }
                let layout = self.create_pango_layout(Some(&text));
                layout.set_font_description(Some(&style.font(&font)));
                layout.set_attributes(style.attributes().as_ref());
                snapshot.save();
                snapshot.translate(&graphene::Point::new(start as f32 * cw, y));
                let alpha = if style.flags & ffi::CELL_FAINT != 0 {
                    0.6
                } else {
                    1.0
                };
                snapshot.append_layout(&layout, &rgba(style.fg, alpha));
                snapshot.restore();
            }
        }
    }
}

/// One cell as painted: colors after inverse/selection/cursor swaps.
struct Painted {
    ch: char,
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

impl Painted {
    fn new(info: &FrameInfo, cell: &GridCell, cursor: bool) -> Self {
        let ch = char::from_u32(cell.codepoint)
            .filter(|c| *c != '\0')
            .unwrap_or(' ');
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
            ch,
            fg,
            bg,
            flags: cell.flags & TEXT_FLAGS,
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

    fn attributes(&self) -> Option<pango::AttrList> {
        if self.flags & (ffi::CELL_UNDERLINE | ffi::CELL_STRIKE) == 0 {
            return None;
        }
        let attrs = pango::AttrList::new();
        if self.flags & ffi::CELL_UNDERLINE != 0 {
            attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single));
        }
        if self.flags & ffi::CELL_STRIKE != 0 {
            attrs.insert(pango::AttrInt::new_strikethrough(true));
        }
        Some(attrs)
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
    };
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
        view.press(none, at(0, 0));
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
        view.press(none, at(1, 1));
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
        view.press(shift, at(0, 0));
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
}
