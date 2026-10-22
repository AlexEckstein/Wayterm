//! The experimental libghostty-vt backend (doc 02 §GhosttyBackend), built
//! only with the `backend-ghostty` feature.
//!
//! - [`ffi`] declares `shim.c`, a narrow C layer over the pinned
//!   libghostty-vt C API (doc 02 §libghostty-vt pin).
//! - [`engine::Engine`] owns one terminal safely and needs no display.
//! - [`view::GhosttyView`] is the GTK4 widget that paints the engine's
//!   grid snapshot and turns keys, mouse drags, and wheel turns into input.
//! - [`GhosttyBackend`] puts them behind [`TerminalBackend`].

pub mod engine;
pub mod ffi;
pub mod view;

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{pango, Widget};
use tokio::sync::mpsc;

use super::backend::{TerminalBackend, TerminalEvent, TerminalFeatures};
use super::vte::EventSender;
use engine::{Effect, Engine, Scroll};
use view::GhosttyView;

/// The grid a backend starts with, before its widget is allocated.
const INITIAL_GRID: (u16, u16) = (80, 24);

/// The engine and the event queue, shared by the backend and its widget.
struct Shared {
    /// The terminal state.
    engine: Engine,
    /// Never-blocking sender into the pump's channel.
    events: EventSender,
}

impl Shared {
    /// Forward an engine effect as a terminal event.
    fn emit(&self, effects: Vec<Effect>) {
        for effect in effects {
            self.events.send(match effect {
                Effect::Pty(reply) => TerminalEvent::DataOut(reply),
                Effect::Bell => TerminalEvent::Bell,
                Effect::Title(title) => TerminalEvent::TitleChanged(title),
            });
        }
    }

    /// Input for the remote side; the view follows it to the bottom.
    fn input(&self, data: Vec<u8>) {
        if !data.is_empty() {
            self.engine.scroll(Scroll::Bottom);
            self.events.send(TerminalEvent::DataOut(data));
        }
    }
}

/// A libghostty-vt terminal drawn by [`GhosttyView`]. Main thread only.
pub struct GhosttyBackend {
    /// Engine and event queue, shared with the view.
    shared: Rc<Shared>,
    /// The widget the tab embeds.
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
        });
        let view = GhosttyView::new(shared.clone());
        Some(Self { shared, view })
    }

    /// The shared engine.
    fn engine(&self) -> &Engine {
        &self.shared.engine
    }
}

impl TerminalBackend for GhosttyBackend {
    fn widget(&self) -> Widget {
        self.view.clone().upcast()
    }

    fn write(&self, data: &[u8]) {
        let effects = self.engine().write(data);
        self.shared.emit(effects);
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
            search: false,
            color_schemes: false,
            scrollback_search: false,
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

    /// A no-op: `features().color_schemes` is false.
    fn set_color_scheme(&self, _name: &str) {}

    fn set_font(&self, font_desc: &pango::FontDescription) {
        self.view.set_font(font_desc);
    }

    /// A no-op, as for VTE.
    fn set_opacity(&self, _opacity: f64) {}

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
        self.view.queue_draw();
    }

    /// A full reset, as VTE's `clear` does.
    fn clear(&self) {
        self.reset();
    }

    /// A no-op: `features().search` is false, so the UI hides it.
    fn search(&self, _text: &str, _reverse: bool) {}

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
