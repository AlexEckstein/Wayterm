//! The experimental libghostty-vt backend (doc 02 §GhosttyBackend), built
//! only with the `backend-ghostty` feature.
//!
//! - [`ffi`] declares `shim.c`, a narrow C layer over the pinned
//!   libghostty-vt C API (doc 02 §libghostty-vt pin).
//! - [`engine::Engine`] owns one terminal, and its search, safely and needs
//!   no display.
//! - [`view::GhosttyView`] is the GTK4 widget that paints the engine's
//!   grid snapshot, lights search matches, and turns keys, mouse, wheel
//!   and focus events into input or local selection and scrolling.
//! - [`GhosttyBackend`] puts them behind [`TerminalBackend`].

pub mod engine;
pub mod ffi;
pub mod view;

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{pango, Widget};
use tokio::sync::mpsc;

use super::backend::{
    clamp_opacity, mark_translucent, TerminalBackend, TerminalEvent, TerminalFeatures,
};
use super::color_scheme;
use super::vte::EventSender;
use engine::{ClipboardDestination, Effect, Engine, Scroll};
use view::GhosttyView;

/// The grid a backend starts with, before its widget is allocated.
const INITIAL_GRID: (u16, u16) = (80, 24);

/// The engine and the event queue, shared by the backend and its widget.
struct Shared {
    engine: Engine,
    events: EventSender,
    watchers: Watchers,
}

/// What the tab follows the shell by (doc 07 §Following the shell), as
/// VTE's `commit`, `contents-changed` and `current-directory-uri-changed`
/// signals; connected through [`GhosttyView`].
#[derive(Default)]
struct Watchers {
    input: WatchList<InputWatch>,
    output: WatchList<dyn Fn()>,
    directory: WatchList<dyn Fn(&str)>,
    /// A program's clipboard write; [`GhosttyView`] applies it.
    clipboard: WatchList<dyn Fn(ClipboardDestination, &str)>,
}

type InputWatch = dyn Fn(&[u8]);
type WatchList<F> = RefCell<Vec<Rc<F>>>;

/// A copy of the list, so a watcher may connect another while it runs.
fn each<F: ?Sized>(list: &WatchList<F>) -> Vec<Rc<F>> {
    list.borrow().clone()
}

impl Shared {
    /// Forward an engine effect as a terminal event; a reported directory
    /// and a clipboard write go to the watchers instead.
    fn emit(&self, effects: Vec<Effect>) {
        for effect in effects {
            let event = match effect {
                Effect::Pty(reply) => TerminalEvent::DataOut(reply),
                Effect::Bell => TerminalEvent::Bell,
                Effect::Title(title) => TerminalEvent::TitleChanged(title),
                Effect::Pwd(dir) => {
                    for watch in each(&self.watchers.directory) {
                        watch(&dir);
                    }
                    continue;
                }
                Effect::ClipboardWrite { destination, text } => {
                    for watch in each(&self.watchers.clipboard) {
                        watch(destination, &text);
                    }
                    continue;
                }
            };
            self.events.send(event);
        }
    }

    /// Output from the remote side.
    fn output(&self, data: &[u8]) {
        let effects = self.engine.write(data);
        self.emit(effects);
        for watch in each(&self.watchers.output) {
            watch();
        }
    }

    /// Input for the remote side; the view follows it to the bottom.
    fn input(&self, data: Vec<u8>) {
        if !data.is_empty() {
            self.engine.scroll(Scroll::Bottom);
            for watch in each(&self.watchers.input) {
                watch(&data);
            }
            self.events.send(TerminalEvent::DataOut(data));
        }
    }
}

/// A libghostty-vt terminal drawn by [`GhosttyView`]. Main thread only.
pub struct GhosttyBackend {
    shared: Rc<Shared>,
    view: GhosttyView,
}

impl GhosttyBackend {
    /// Build the engine and its widget; events go to `event_tx`, the
    /// bounded channel the transport pump consumes. `None` if
    /// libghostty-vt cannot allocate a terminal.
    pub fn new(event_tx: mpsc::Sender<TerminalEvent>) -> Option<Self> {
        let engine = Engine::new(INITIAL_GRID.0, INITIAL_GRID.1)?;
        let shared = Rc::new(Shared {
            engine,
            events: EventSender::new(event_tx),
            watchers: Watchers::default(),
        });
        let view = GhosttyView::new(shared.clone());
        Some(Self { shared, view })
    }

    fn engine(&self) -> &Engine {
        &self.shared.engine
    }
}

impl TerminalBackend for GhosttyBackend {
    fn widget(&self) -> Widget {
        self.view.clone().upcast()
    }

    fn write(&self, data: &[u8]) {
        self.shared.output(data);
        self.view.queue_draw();
    }

    /// Typed-input path: straight onto the `DataOut` queue, like keys.
    fn send_input(&self, data: &[u8]) {
        self.shared.input(data.to_vec());
        self.view.queue_draw();
    }

    /// Resize the grid now; the widget's next allocation still wins.
    fn set_size(&self, rows: u32, cols: u32) {
        let clamp = |n: u32| u16::try_from(n).unwrap_or(u16::MAX).max(1);
        self.view.request_grid(clamp(rows), clamp(cols));
    }

    fn features(&self) -> TerminalFeatures {
        TerminalFeatures {
            search: true,
            color_schemes: true,
            scrollback_search: true,
            url_detection: false,
            sixel_graphics: false,
            // libghostty-vt parses Kitty graphics; the widget draws no
            // images.
            kitty_graphics: false,
            custom_fonts: true,
            reflow: true,
            alternate_screen: true,
            scrollback_config: true,
        }
    }

    /// A built-in palette by name ([`color_scheme::by_name`]), as in VTE;
    /// an unknown name leaves the colors as they are. Until the first call
    /// the engine has libghostty-vt's built-in colors.
    fn set_color_scheme(&self, name: &str) {
        let Some(scheme) = color_scheme::by_name(name) else {
            return;
        };
        self.engine().set_color_scheme(scheme);
        self.view.queue_draw();
    }

    fn set_font(&self, font_desc: &pango::FontDescription) {
        self.view.set_font(font_desc);
    }

    fn set_opacity(&self, opacity: f64) {
        let opacity = clamp_opacity(opacity);
        self.view.set_opacity(opacity as f32);
        mark_translucent(&self.view, opacity);
    }

    fn set_scrollback_limit(&self, lines: u32) {
        self.engine()
            .set_scrollback_lines(usize::try_from(lines).unwrap_or(usize::MAX));
    }

    fn selected_text(&self) -> Option<String> {
        self.engine().selection_text()
    }

    fn copy_selection(&self) {
        if let Some(text) = self.selected_text() {
            self.view.clipboard().set_text(&text);
        }
    }

    /// Bracketed when the program enabled mode 2004.
    fn paste(&self, text: &str) {
        self.shared.input(self.engine().encode_paste(text));
        self.view.queue_draw();
    }

    fn select_all(&self) {
        self.engine().select_all();
        self.view.copy_primary();
        self.view.queue_draw();
    }

    /// A full reset, as VTE's `clear` does.
    fn clear(&self) {
        self.reset();
    }

    /// Case-sensitive, through the scrollback, wrapping around; the match
    /// is highlighted and scrolled into view, the selection left alone. An
    /// empty query clears the highlight.
    fn search(&self, text: &str, reverse: bool) {
        self.engine().search(text, reverse);
        self.view.queue_draw();
    }

    fn scrollback_text(&self) -> Option<String> {
        self.engine().screen_text()
    }

    fn scroll_to_top(&self) {
        self.engine().scroll(Scroll::Top);
        self.view.queue_draw();
    }

    fn scroll_to_bottom(&self) {
        self.engine().scroll(Scroll::Bottom);
        self.view.queue_draw();
    }

    fn reset(&self) {
        self.engine().reset();
        self.view.queue_draw();
    }

    fn name(&self) -> &str {
        "ghostty"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &str {
        "libghostty-vt engine with a GTK4 render widget (experimental)"
    }
}
