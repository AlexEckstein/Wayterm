//! [`GhosttyView`]: the GTK4 widget that draws a libghostty-vt grid.
//!
//! This file holds the widget, its state and the grid's geometry; [`paint`]
//! and [`text`] draw a frame, [`images`] its Kitty images and [`cursor`] its
//! cursor, [`blink`] runs the timers, [`input`] takes events, [`im`] talks
//! to the input method, [`links`] finds and opens links, and [`options`]
//! carries the settings.

mod blink;
mod cursor;
mod im;
mod images;
mod input;
mod links;
mod options;
mod paint;
mod text;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, pango};

use super::Shared;
use super::engine::{ClipboardDestination, Frame};
use crate::terminal::TerminalEvent;
use crate::terminal::options::GhosttyOptions;
use input::Route;
use links::{Launcher, Link, launch_uri};
pub use options::install_options;

/// The font a view starts with, until `set_font`.
const DEFAULT_FONT: &str = "Monospace 12";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GhosttyView {
        pub(super) shared: RefCell<Option<Rc<Shared>>>,
        pub(super) frame: RefCell<Frame>,
        pub(super) font: RefCell<Option<pango::FontDescription>>,
        pub(super) options: RefCell<GhosttyOptions>,
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
        /// Wheel rows not yet scrolled; see [`super::input::wheel_rows`].
        pub(super) wheel_carry: Cell<f64>,
        /// The same for the sideways wheel, in columns.
        pub(super) wheel_carry_x: Cell<f64>,
        /// Where the primary-button drag in progress goes.
        pub(super) drag: Cell<Option<super::Route>>,
        /// Where the middle and right buttons' drags in progress go.
        pub(super) middle_drag: Cell<Option<super::Route>>,
        pub(super) right_drag: Cell<Option<super::Route>>,
        /// The last pointer position, for wheel reports.
        pub(super) pointer: Cell<(f64, f64)>,
        /// The cell of the last reported motion, so each cell is sent once.
        pub(super) motion_cell: Cell<Option<(u16, u16)>>,
        /// The images the last frame drew, by ID and pixel generation.
        pub(super) textures: RefCell<HashMap<(u32, u64), gdk::Texture>>,
        /// A redraw for an animation's next frame is on its way.
        pub(super) animating: Cell<bool>,
        /// The modifiers of the last pointer motion; `None` while the
        /// pointer is outside the widget.
        pub(super) hover: Cell<Option<gdk::ModifierType>>,
        /// The link under the pointer, underlined.
        pub(super) link: RefCell<Option<super::Link>>,
        /// The link a Ctrl+press landed on, until its release.
        pub(super) pressed_link: RefCell<Option<super::Link>>,
        pub(super) launcher: RefCell<Option<super::Launcher>>,
        /// Whether the view has keyboard focus, from the focus controller.
        pub(super) focused: Cell<bool>,
        /// The blink's off phase; a stopped timer leaves it false.
        pub(super) blink_off: Cell<bool>,
        /// The blink timer; see [`super::cursor::needs_blink_timer`].
        pub(super) blink: RefCell<Option<glib::SourceId>>,
        /// Blinking text is in its off phase.
        pub(super) text_blink_off: Cell<bool>,
        /// Flips `text_blink_off`; runs only while a mapped view shows
        /// blinking text.
        pub(super) text_blink_timer: RefCell<Option<glib::SourceId>>,
        pub(super) im: super::im::ImState,
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

        fn realize(&self) {
            self.parent_realize();
            self.obj().im_realized(true);
        }

        fn unrealize(&self) {
            self.obj().im_realized(false);
            self.parent_unrealize();
        }

        fn unmap(&self) {
            self.obj().stop_blink();
            self.obj().stop_text_blink();
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
        imp.launcher.replace(Some(Rc::new(launch_uri)));
        imp.shared.replace(Some(shared));
        view.set_focusable(true);
        view.set_hexpand(true);
        view.set_vexpand(true);
        view.set_overflow(gtk4::Overflow::Hidden);
        view.follow_options();
        view.install_input();
        view.install_im(gtk4::IMMulticontext::new().upcast());
        view.install_clipboard();
        view
    }

    /// Apply what programs write to the clipboards. The watcher holds the
    /// view weakly, since the view holds the shared state it lives in.
    fn install_clipboard(&self) {
        let view = self.downgrade();
        let apply = move |destination, text: &str| {
            let Some(view) = view.upgrade() else {
                return;
            };
            match destination {
                ClipboardDestination::System => view.clipboard().set_text(text),
                ClipboardDestination::Selection => view.primary_clipboard().set_text(text),
            }
        };
        if let Some(shared) = self.shared() {
            shared.watchers.clipboard.borrow_mut().push(Rc::new(apply));
        }
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
            font_underline: (
                pixels(font_metrics.underline_position()),
                pixels(font_metrics.underline_thickness()),
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
}

/// A cell's size in whole pixels, and where its text sits: the baseline,
/// and the underline and strikethrough as `(top, thickness)`, in pixels
/// down from the cell's top. `font_underline` is the font's own line, as
/// Pango draws it: `(position, thickness)` in pixels, up from the baseline.
#[derive(Clone, Copy, Debug)]
struct Metrics {
    width: i32,
    height: i32,
    baseline: f32,
    underline: (f32, f32),
    font_underline: (f32, f32),
    strike: (f32, f32),
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

/// The widget check needs a display. It is its own test, not part of
/// `ui::gtk_tests`, and must not share a run with it (GTK binds to the
/// first thread that initializes it):
/// `env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test
/// --features backend-ghostty -- --ignored ghostty_display`.
#[cfg(test)]
mod ghostty_display;
