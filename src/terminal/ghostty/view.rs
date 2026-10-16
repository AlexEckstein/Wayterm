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
//! Kitty keyboard modes apply) and leave as `DataOut`; a primary-button
//! drag selects; the wheel scrolls the viewport three rows per notch.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, pango};

use super::engine::{Cell as GridCell, Frame, FrameInfo, Scroll};
use super::ffi;
use super::Shared;
use crate::terminal::TerminalEvent;

/// The font a view starts with, until `set_font`.
const DEFAULT_FONT: &str = "Monospace 12";
/// Rows per wheel notch.
const WHEEL_ROWS: f64 = 3.0;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GhosttyView {
        pub(super) shared: RefCell<Option<Rc<Shared>>>,
        pub(super) frame: RefCell<Frame>,
        pub(super) font: RefCell<Option<pango::FontDescription>>,
        /// Cell size in pixels; recomputed after a font change.
        pub(super) cell: Cell<Option<(i32, i32)>>,
        /// The grid asked for by `set_size`: the natural size.
        pub(super) requested: Cell<(u16, u16)>,
        /// The last grid reported as `SizeChange`, as `(cols, rows)`.
        pub(super) reported: Cell<Option<(u16, u16)>>,
        /// Where the current selection drag started, `(col, row)`.
        pub(super) anchor: Cell<Option<(u16, u16)>>,
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
        drag.connect_drag_begin(move |_, x, y| {
            let Some(view) = view.upgrade() else { return };
            view.grab_focus();
            view.imp().anchor.set(Some(view.cell_at(x, y)));
            if let Some(shared) = view.shared() {
                shared.engine.clear_selection();
            }
            view.queue_draw();
        });
        let view = self.downgrade();
        drag.connect_drag_update(move |drag, dx, dy| {
            let Some(view) = view.upgrade() else { return };
            let (Some(anchor), Some((x, y)), Some(shared)) =
                (view.imp().anchor.get(), drag.start_point(), view.shared())
            else {
                return;
            };
            shared.engine.select(anchor, view.cell_at(x + dx, y + dy));
            view.queue_draw();
        });
        self.add_controller(drag);

        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let view = self.downgrade();
        wheel.connect_scroll(move |_, _, dy| {
            let Some(view) = view.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if let Some(shared) = view.shared() {
                shared
                    .engine
                    .scroll(Scroll::By((dy * WHEEL_ROWS).round() as isize));
                view.queue_draw();
            }
            glib::Propagation::Stop
        });
        self.add_controller(wheel);
    }

    /// Encode a key press; `false` lets it propagate (modifier-only keys).
    fn key(&self, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
        let Some(shared) = self.shared() else {
            return false;
        };
        let mut mods = 0;
        for (mask, bit) in [
            (gdk::ModifierType::SHIFT_MASK, ffi::MOD_SHIFT),
            (gdk::ModifierType::CONTROL_MASK, ffi::MOD_CTRL),
            (gdk::ModifierType::ALT_MASK, ffi::MOD_ALT),
            (gdk::ModifierType::SUPER_MASK, ffi::MOD_SUPER),
        ] {
            if state.contains(mask) {
                mods |= bit;
            }
        }
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
            &rgba(info.bg, 1.0),
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

/// The widget check needs a display. It is its own test, not part of
/// `ui::gtk_tests`, and must not share a run with it (GTK binds to the
/// first thread that initializes it):
/// `env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test
/// --features backend-ghostty -- --ignored ghostty_display`.
#[cfg(test)]
mod ghostty_display {
    use super::*;
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

        backend.send_input(b"ls\r");
        assert_eq!(
            rx.try_recv().unwrap(),
            TerminalEvent::DataOut(b"ls\r".to_vec())
        );
        window.close();

        // The factory follows the installed preference; VTE is the default.
        install_backend_preference(BackendKind::Ghostty);
        assert_eq!(create_terminal().backend.name(), "ghostty");
        install_backend_preference(BackendKind::Vte);
        assert_eq!(create_terminal().backend.name(), "vte");
    }
}
