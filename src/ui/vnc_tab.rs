//! The VNC tab page (doc 08 §VNC, §Tabs): a `DrawingArea` that blits the
//! engine's framebuffer and forwards input to it.
//!
//! The connect flow mirrors the SSH dial in `tab.rs`: the password prompt
//! blocks, so the whole dial (prompt, handshake, engine spawn) runs on the
//! blocking pool. The engine's event receiver is a tokio channel that works
//! from any executor, so a GTK-side future drains it directly — no bridge
//! task. Input goes the other way through the sync, non-blocking
//! [`VncInput`](crate::vnc::VncInput).
//!
//! ## GTK-side findings
//!
//! - **Pixels:** the framebuffer is `a << 24 | r << 16 | g << 8 | b` per
//!   `u32`, which is cairo's native-endian `RGB24` word on any host, so the
//!   blit is a per-row `to_ne_bytes` copy ([`blit_rows`]). `RGB24` (not
//!   `ARGB32`) ignores the pad byte outright.
//! - **Scaled blits need `Extend::Pad`** and a fill of just the picture
//!   rectangle, or the border pixels fade to black (`View::paint`).
//! - **Cairo strides** may exceed `width * 4`; always copy row by row.
//! - **`ImageSurface::data` needs the only reference** to the surface, so the
//!   cached surface is touched before it is handed to the context, never
//!   while a source pattern still holds it.
//! - **GDK keyvals are X11 keysyms** (doc 08): forwarded unchanged, and
//!   modifiers are plain key events of their own, so there is no modifier
//!   state machine here.
//! - **No Tier-2 controller** (doc 08 §Tabs): the page installs only the four
//!   remote-input controllers; window accelerators still fire first.
//! - **Weak page refs in tasks:** a task holding the page strongly would keep
//!   it alive after the tab is closed, and the engine is cancelled only when
//!   the page's data drops (`EngineSlot`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::{cairo, gdk, glib};
use tokio::sync::mpsc;

use crate::store::model::{SessionConfig, VncOptions};
use crate::transport::credentials::CredentialManager;
use crate::transport::types::TransportState;
use crate::ui::tab::{
    credential_manager, current_gen, filled, page_store, publish_status, set_terminal_area,
    show_connecting_view, show_disconnected, show_error_view, show_failure, NO_HOST,
};
use crate::ui::widgets::set_data;
use crate::vnc::{
    ClientKeyEvent, ClientMouseEvent, VncEngine, VncPixels, VncSession, VncUiEvent, X11Event,
};

/// The RFB default (display :0).
pub const DEFAULT_PORT: u16 = 5900;
const ENGINE_KEY: &str = "wayterm-vnc-engine";

/// RFB pointer button bits (RFC 6143 §7.5.5).
const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 4;
const WHEEL_UP: u8 = 8;
const WHEEL_DOWN: u8 = 16;

// ---------------------------------------------------------------------------
// Pure mapping (display-free; unit-tested in tests/vnc_tab_flow.rs)
// ---------------------------------------------------------------------------

/// Where the framebuffer sits inside the widget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub scale: f64,
    pub off_x: f64,
    pub off_y: f64,
}

/// Fit (`scaling`: aspect-preserving, centered) or center 1:1.
pub fn placement(widget: (i32, i32), fb: (u16, u16), scaling: bool) -> Placement {
    let (w, h) = (f64::from(widget.0.max(0)), f64::from(widget.1.max(0)));
    let (fw, fh) = (f64::from(fb.0), f64::from(fb.1));
    let scale = if scaling && fw > 0.0 && fh > 0.0 {
        (w / fw).min(h / fh)
    } else {
        1.0
    };
    Placement {
        scale,
        off_x: (w - fw * scale) / 2.0,
        off_y: (h - fh * scale) / 2.0,
    }
}

/// Widget coordinates to framebuffer pixels, clamped to the framebuffer so a
/// drag that leaves the picture keeps tracking the edge.
pub fn to_framebuffer(p: &Placement, fb: (u16, u16), x: f64, y: f64) -> (u16, u16) {
    let axis = |v: f64, off: f64, len: u16| -> u16 {
        if len == 0 || p.scale <= 0.0 {
            return 0;
        }
        let pos = ((v - off) / p.scale).floor();
        // NaN-safe: `as` saturates and maps NaN to 0.
        (pos as i64).clamp(0, i64::from(len) - 1) as u16
    };
    (axis(x, p.off_x, fb.0), axis(y, p.off_y, fb.1))
}

/// The RFB button bit for a GDK button number (1 left, 2 middle, 3 right).
pub fn button_bit(gdk_button: u32) -> u8 {
    match gdk_button {
        1 => BUTTON_LEFT,
        2 => BUTTON_MIDDLE,
        3 => BUTTON_RIGHT,
        _ => 0,
    }
}

/// The wheel bit for a vertical scroll delta (GDK: negative = up).
pub fn wheel_bit(dy: f64) -> Option<u8> {
    if dy < 0.0 {
        Some(WHEEL_UP)
    } else if dy > 0.0 {
        Some(WHEEL_DOWN)
    } else {
        None
    }
}

/// `mask` with `bit` set or cleared.
pub fn apply_button(mask: u8, bit: u8, down: bool) -> u8 {
    if down {
        mask | bit
    } else {
        mask & !bit
    }
}

/// Scaling and view-only from the row; no options means scaled and
/// interactive.
pub fn effective_options(session: &SessionConfig) -> (bool, bool) {
    session.vnc_options.as_ref().map_or(
        (true, false),
        |VncOptions {
             scaling, view_only, ..
         }| { (*scaling, *view_only) },
    )
}

/// The page state an engine event implies, if any. A requested cancel is a
/// clean disconnect; any other end is an error.
pub fn state_for(event: &VncUiEvent) -> Option<TransportState> {
    match event {
        VncUiEvent::Ended(reason) if reason == crate::vnc::client::CANCELLED => {
            Some(TransportState::Disconnected)
        }
        VncUiEvent::Ended(reason) => Some(TransportState::Error(reason.clone())),
        _ => None,
    }
}

/// Copy `src` (row-major, `width` pixels per row) into a cairo `RGB24` buffer
/// with row pitch `stride` bytes. Short buffers copy what fits.
pub fn blit_rows(src: &[u32], width: usize, dst: &mut [u8], stride: usize) {
    if width == 0 || stride == 0 {
        return;
    }
    for (row, out) in src.chunks(width).zip(dst.chunks_mut(stride)) {
        let (pixels, _) = out.as_chunks_mut::<4>();
        for (pixel, bytes) in row.iter().zip(pixels) {
            *bytes = pixel.to_ne_bytes();
        }
    }
}

// ---------------------------------------------------------------------------
// The view: framebuffer -> cairo, widget input -> engine
// ---------------------------------------------------------------------------

type Sink = Box<dyn Fn(X11Event) -> bool>;

/// Everything the draw and input handlers share. Input goes through `sink`
/// (the engine's `VncInput` in production, a spy in tests).
pub(crate) struct View {
    pixels: Arc<Mutex<VncPixels>>,
    sink: Sink,
    scaling: bool,
    view_only: bool,
    fb: Cell<(u16, u16)>,
    widget_size: Cell<(i32, i32)>,
    pointer: Cell<(f64, f64)>,
    buttons: Cell<u8>,
    surface: RefCell<Option<cairo::ImageSurface>>,
    scratch: RefCell<Vec<u32>>,
}

impl View {
    pub(crate) fn new(
        pixels: Arc<Mutex<VncPixels>>,
        sink: Sink,
        scaling: bool,
        view_only: bool,
    ) -> Rc<Self> {
        let fb = lock(&pixels).dims();
        Rc::new(Self {
            pixels,
            sink,
            scaling,
            view_only,
            fb: Cell::new(fb),
            widget_size: Cell::new((0, 0)),
            pointer: Cell::new((0.0, 0.0)),
            buttons: Cell::new(0),
            surface: RefCell::default(),
            scratch: RefCell::default(),
        })
    }

    /// The desktop changed size; the surface is rebuilt on the next draw.
    pub(crate) fn resolved(&self, width: u16, height: u16) {
        self.fb.set((width, height));
    }

    /// Queue `event` unless the tab is view-only.
    fn forward(&self, event: X11Event) -> bool {
        !self.view_only && (self.sink)(event)
    }

    fn key(&self, keysym: u32, down: bool) -> bool {
        self.forward(X11Event::KeyEvent(ClientKeyEvent {
            keycode: keysym,
            down,
        }))
    }

    fn send_pointer(&self, x: f64, y: f64, buttons: u8) -> bool {
        self.pointer.set((x, y));
        let p = placement(self.widget_size.get(), self.fb.get(), self.scaling);
        let (px, py) = to_framebuffer(&p, self.fb.get(), x, y);
        self.forward(X11Event::PointerEvent(ClientMouseEvent {
            position_x: px,
            position_y: py,
            bottons: buttons,
        }))
    }

    fn moved(&self, x: f64, y: f64) -> bool {
        self.send_pointer(x, y, self.buttons.get())
    }

    fn button(&self, gdk_button: u32, down: bool, x: f64, y: f64) -> bool {
        let mask = apply_button(self.buttons.get(), button_bit(gdk_button), down);
        self.buttons.set(mask);
        self.send_pointer(x, y, mask)
    }

    /// A wheel notch is a press and release of its button bit.
    fn scrolled(&self, dy: f64) -> bool {
        let Some(bit) = wheel_bit(dy) else {
            return false;
        };
        let (x, y) = self.pointer.get();
        let held = self.buttons.get();
        let pressed = self.send_pointer(x, y, held | bit);
        self.send_pointer(x, y, held) && pressed
    }

    /// Paint the framebuffer onto `cr`, a `width` x `height` target.
    pub(crate) fn paint(&self, cr: &cairo::Context, width: i32, height: i32) {
        self.widget_size.set((width, height));
        cr.set_source_rgb(0.0, 0.0, 0.0);
        let _ = cr.paint();
        let Some(surface) = self.refresh_surface() else {
            return;
        };
        let p = placement((width, height), self.fb.get(), self.scaling);
        cr.translate(p.off_x, p.off_y);
        cr.scale(p.scale, p.scale);
        if cr.set_source_surface(&surface, 0.0, 0.0).is_ok() {
            // Scaling samples past the border; without Pad the outer pixels
            // blend with transparent and the picture gets dark edges.
            cr.source().set_extend(cairo::Extend::Pad);
            cr.rectangle(
                0.0,
                0.0,
                f64::from(surface.width()),
                f64::from(surface.height()),
            );
            let _ = cr.fill();
        }
    }

    /// Snapshot the engine's pixels into the cached surface (recreated when
    /// the desktop size changes). The pixel lock is held only for the copy
    /// into the reusable scratch buffer.
    fn refresh_surface(&self) -> Option<cairo::ImageSurface> {
        let mut scratch = self.scratch.borrow_mut();
        let dims = {
            let pixels = lock(&self.pixels);
            pixels.write_snapshot(&mut scratch);
            pixels.dims()
        };
        self.fb.set(dims);
        let (w, h) = (usize::from(dims.0), usize::from(dims.1));
        if w == 0 || h == 0 {
            return None;
        }
        let mut cached = self.surface.borrow_mut();
        if cached
            .as_ref()
            .is_none_or(|s| (s.width(), s.height()) != (i32::from(dims.0), i32::from(dims.1)))
        {
            *cached = cairo::ImageSurface::create(
                cairo::Format::Rgb24,
                i32::from(dims.0),
                i32::from(dims.1),
            )
            .ok();
        }
        let surface = cached.as_mut()?;
        let stride = usize::try_from(surface.stride()).ok()?;
        surface.flush();
        {
            let mut dst = surface.data().ok()?;
            blit_rows(&scratch, w, &mut dst, stride);
        }
        surface.mark_dirty();
        Some(surface.clone())
    }
}

fn lock(pixels: &Mutex<VncPixels>) -> std::sync::MutexGuard<'_, VncPixels> {
    // A poisoned buffer is still a valid picture.
    pixels.lock().unwrap_or_else(|e| e.into_inner())
}

/// The drawing area with its draw handler and the input controllers. No
/// shortcut controller: the remote desktop keeps its keys (doc 08 §Tabs).
pub(crate) fn build_area(view: &Rc<View>) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_focusable(true);
    area.set_focus_on_click(true);

    let v = view.clone();
    area.set_draw_func(move |_, cr, w, h| v.paint(cr, w, h));
    let v = view.clone();
    area.connect_resize(move |_, w, h| v.widget_size.set((w, h)));

    let keys = gtk4::EventControllerKey::new();
    let v = view.clone();
    keys.connect_key_pressed(move |_, key, _, _| propagation(v.key(keysym(key), true)));
    let v = view.clone();
    keys.connect_key_released(move |_, key, _, _| {
        v.key(keysym(key), false);
    });
    area.add_controller(keys);

    let motion = gtk4::EventControllerMotion::new();
    let v = view.clone();
    motion.connect_motion(move |_, x, y| {
        v.moved(x, y);
    });
    area.add_controller(motion);

    // Button 0 = every button.
    let click = gtk4::GestureClick::new();
    click.set_button(0);
    let v = view.clone();
    let focus = area.downgrade();
    click.connect_pressed(move |gesture, _, x, y| {
        if let Some(area) = focus.upgrade() {
            area.grab_focus();
        }
        v.button(gesture.current_button(), true, x, y);
    });
    let v = view.clone();
    click.connect_released(move |gesture, _, x, y| {
        v.button(gesture.current_button(), false, x, y);
    });
    area.add_controller(click);

    let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    let v = view.clone();
    scroll.connect_scroll(move |_, _, dy| propagation(v.scrolled(dy)));
    area.add_controller(scroll);
    area
}

/// GDK keyvals are X11 keysyms; the glib value is the keysym itself.
fn keysym(key: gdk::Key) -> u32 {
    key.into_glib()
}

fn propagation(handled: bool) -> glib::Propagation {
    if handled {
        glib::Propagation::Stop
    } else {
        glib::Propagation::Proceed
    }
}

// ---------------------------------------------------------------------------
// Connect flow
// ---------------------------------------------------------------------------

/// Owns the page's engine; dropping it (page finalized, or replaced by a
/// retry) cancels the engine, which closes the socket and ends its task.
struct EngineSlot(RefCell<Option<VncEngine>>);

impl Drop for EngineSlot {
    fn drop(&mut self) {
        if let Some(engine) = self.0.get_mut().take() {
            engine.cancel();
        }
    }
}

/// Cancel the page's current engine, if any.
fn retire_engine(page: &gtk4::Widget) {
    set_data(page, ENGINE_KEY, Rc::new(EngineSlot(RefCell::default())));
}

enum Dial {
    /// The password prompt was cancelled.
    Cancelled,
    Failed(String),
    Ready(VncEngine),
}

/// Resolve the password, then handshake and start the engine. Blocks on the
/// prompt, so it runs on the blocking pool inside the runtime.
fn dial(creds: &CredentialManager, session_id: &str, host: &str, port: u16) -> Dial {
    let Some(password) = creds.lookup(session_id, "vnc_password") else {
        return Dial::Cancelled;
    };
    let connected = tokio::runtime::Handle::current().block_on(async {
        VncSession::connect(host, port, Some(password))
            .await
            .map(VncSession::spawn)
    });
    match connected {
        Ok(engine) => Dial::Ready(engine),
        Err(err) => Dial::Failed(err.to_string()),
    }
}

/// Start (or retry) the VNC connect for `page`; `gen` is the attempt
/// generation `start_connect` just bumped.
pub(crate) fn start(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    gen: u64,
) {
    retire_engine(page);
    let Some(host) = filled(&session.host) else {
        return show_error_view(notebook, page, session, NO_HOST);
    };
    let host = host.to_string();
    let port = session.port.unwrap_or(DEFAULT_PORT);
    show_connecting_view(page, &host, port);
    publish_status(page, TransportState::Connecting, &host, port);

    let creds = credential_manager(session.id, &session.name, page_store(page).as_deref());
    let (id, dial_host) = (session.id.to_string(), host.clone());
    let task = tokio::task::spawn_blocking(move || dial(&creds, &id, &dial_host, port));

    let (notebook, page, session) = (notebook.clone(), page.clone(), session.clone());
    glib::MainContext::default().spawn_local(async move {
        let result = task.await;
        if current_gen(&page) != gen {
            // Superseded: a late engine must not leak its socket.
            if let Ok(Dial::Ready(engine)) = result {
                engine.cancel();
            }
            return;
        }
        let error = match result {
            Ok(Dial::Ready(engine)) => return attach(&notebook, &page, &session, engine, gen),
            Ok(Dial::Cancelled) => return show_disconnected(&page, &host, port),
            Ok(Dial::Failed(error)) => error,
            Err(join_err) => format!("connect task failed: {join_err}"),
        };
        show_failure(&notebook, &page, &session, &error, &host, port);
    });
}

/// Show the framebuffer and pump engine events into the page.
fn attach(
    notebook: &gtk4::Notebook,
    page: &gtk4::Widget,
    session: &Rc<SessionConfig>,
    mut engine: VncEngine,
    gen: u64,
) {
    let host = session.host.clone().unwrap_or_default();
    let port = session.port.unwrap_or(DEFAULT_PORT);
    let (scaling, view_only) = effective_options(session);
    let input = engine.input.clone();
    let view = View::new(
        engine.pixels.clone(),
        Box::new(move |event| input.send(event)),
        scaling,
        view_only,
    );
    let area = build_area(&view);
    set_terminal_area(page, &area);
    area.grab_focus();

    // The slot keeps the handle (and with it, cancel-on-drop); the receiver
    // moves to the pump, leaving a closed placeholder behind.
    let mut events = std::mem::replace(&mut engine.events, mpsc::channel(1).1);
    set_data(
        page,
        ENGINE_KEY,
        Rc::new(EngineSlot(RefCell::new(Some(engine)))),
    );
    publish_status(page, TransportState::Connected, &host, port);

    let (notebook, weak_page, session) = (notebook.clone(), page.downgrade(), session.clone());
    glib::MainContext::default().spawn_local(async move {
        while let Some(event) = events.recv().await {
            let Some(page) = weak_page.upgrade() else {
                return;
            };
            if current_gen(&page) != gen {
                return;
            }
            match &event {
                VncUiEvent::Dirtied => area.queue_draw(),
                VncUiEvent::Resolved(w, h) => {
                    view.resolved(*w, *h);
                    area.queue_draw();
                }
                VncUiEvent::Bell => area.error_bell(),
                VncUiEvent::Clipboard(text) => {
                    if let Some(display) = gdk::Display::default() {
                        display.clipboard().set_text(text);
                    }
                }
                VncUiEvent::Ended(reason) => {
                    match state_for(&event) {
                        Some(TransportState::Disconnected) => show_disconnected(&page, &host, port),
                        _ => show_failure(&notebook, &page, &session, reason, &host, port),
                    }
                    return;
                }
            }
        }
    });
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    fn spy_view(
        view_only: bool,
        pixels: Arc<Mutex<VncPixels>>,
    ) -> (Rc<View>, Rc<RefCell<Vec<X11Event>>>) {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        let view = View::new(
            pixels,
            Box::new(move |event| {
                sink.borrow_mut().push(event);
                true
            }),
            true,
            view_only,
        );
        (view, seen)
    }

    fn red_4x2() -> Arc<Mutex<VncPixels>> {
        let mut px = VncPixels::new(4, 2);
        // BGRA wire order: pure red.
        let bytes: Vec<u8> = (0..8).flat_map(|_| [0, 0, 255, 0]).collect();
        px.write_rect((0, 0, 4, 2), &bytes).expect("write");
        Arc::new(Mutex::new(px))
    }

    pub(in crate::ui) fn vnc_view_draws_and_routes_input() {
        // GLib criticals abort instead of scrolling by.
        // SAFETY: plain flag change on the process-wide GLib log mask.
        unsafe {
            glib::ffi::g_log_set_always_fatal(
                glib::ffi::G_LOG_LEVEL_CRITICAL | glib::ffi::G_LOG_LEVEL_WARNING,
            );
        }

        let (view, seen) = spy_view(false, red_4x2());
        let area = build_area(&view);
        assert!(area.is_focusable());
        let controllers = area.observe_controllers();
        let kinds: Vec<String> = (0..controllers.n_items())
            .filter_map(|i| controllers.item(i))
            .map(|c| c.type_().name().to_string())
            .collect();
        for expected in [
            "GtkEventControllerKey",
            "GtkEventControllerMotion",
            "GtkGestureClick",
            "GtkEventControllerScroll",
        ] {
            assert!(
                kinds.iter().any(|k| k == expected),
                "{expected} in {kinds:?}"
            );
        }
        assert!(
            !kinds.iter().any(|k| k.contains("Shortcut")),
            "no Tier-2 controller: {kinds:?}"
        );

        // Draw: an 8x4 target scales the 4x2 desktop by 2; every pixel red.
        let target = cairo::ImageSurface::create(cairo::Format::Rgb24, 8, 4).expect("surface");
        {
            let cr = cairo::Context::new(&target).expect("context");
            view.paint(&cr, 8, 4);
        }
        target.flush();
        let mut target = target;
        let stride = target.stride() as usize;
        let data = target.data().expect("data");
        for y in 0..4 {
            for x in 0..8 {
                let at = y * stride + x * 4;
                let word = u32::from_ne_bytes(data[at..at + 4].try_into().unwrap());
                assert_eq!(word & 0x00FF_FFFF, 0x00FF_0000, "pixel ({x},{y})");
            }
        }
        drop(data);

        // Input reaches the sink through the real key controller.
        let key = (0..controllers.n_items())
            .filter_map(|i| controllers.item(i))
            .find_map(|c| c.downcast::<gtk4::EventControllerKey>().ok())
            .expect("key controller");
        let sym: u32 = keysym(gdk::Key::a);
        let handled: bool =
            key.emit_by_name("key-pressed", &[&sym, &0u32, &gdk::ModifierType::empty()]);
        assert!(handled);
        assert!(matches!(
            seen.borrow().as_slice(),
            [X11Event::KeyEvent(ClientKeyEvent { keycode, down: true })] if *keycode == sym
        ));
        assert!(view.moved(2.0, 2.0));
        assert!(matches!(
            seen.borrow().last(),
            Some(X11Event::PointerEvent(ClientMouseEvent {
                position_x: 1,
                position_y: 1,
                bottons: 0
            }))
        ));

        // View-only drops everything, and says so.
        let (view, seen) = spy_view(true, red_4x2());
        assert!(!view.key(sym, true));
        assert!(!view.moved(1.0, 1.0));
        assert!(!view.button(1, true, 1.0, 1.0));
        assert!(!view.scrolled(-1.0));
        assert!(seen.borrow().is_empty());

        // A mid-session resize rebuilds the surface without a hiccup.
        let pixels = red_4x2();
        let (view, _) = spy_view(false, pixels.clone());
        let scratch = cairo::ImageSurface::create(cairo::Format::Rgb24, 8, 8).expect("surface");
        let cr = cairo::Context::new(&scratch).expect("context");
        view.paint(&cr, 8, 8);
        lock(&pixels).resize(6, 6);
        view.resolved(6, 6);
        view.paint(&cr, 8, 8);
    }
}
