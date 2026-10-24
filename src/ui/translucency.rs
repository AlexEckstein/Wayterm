//! See-through terminals (doc 02 §Opacity). A backend marks its widget
//! with [`TRANSLUCENT_CLASS`] while its background is translucent; while
//! such a widget is mapped — its tab is the current one — the window
//! drops its own background and the notebook's, so the desktop shows
//! through. Widgets marked with [`CHROME_CLASS`] keep the theme's
//! background. Main thread only.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::terminal::TRANSLUCENT_CLASS;
use crate::ui::widgets::{data_or_default, window_of};

/// On a window while a translucent terminal in it is mapped.
pub(in crate::ui) const SEE_THROUGH_CLASS: &str = "wayterm-see-through";
/// The window's parts outside the tabs, which stay opaque.
pub(in crate::ui) const CHROME_CLASS: &str = "wayterm-chrome";

const CSS: &str = "
window.wayterm-see-through,
window.wayterm-see-through notebook > stack {
    background: none;
}
window.wayterm-see-through .wayterm-chrome {
    background-color: @theme_bg_color;
}
";

/// Window data key: the translucent terminals mapped in it.
const SHOWN_KEY: &str = "wayterm-translucent-shown";

type Shown = Rc<RefCell<Vec<glib::WeakRef<gtk4::Widget>>>>;

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Load the stylesheet on the default display, once.
pub fn install() {
    if INSTALLED.get() {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(CSS);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    INSTALLED.set(true);
}

/// Keep `terminal`'s window see-through while `terminal` is mapped and
/// carries [`TRANSLUCENT_CLASS`].
pub fn follow(terminal: &gtk4::Widget) {
    terminal.connect_map(sync);
    terminal.connect_unmap(sync);
    terminal.connect_notify_local(Some("css-classes"), |terminal, _| sync(terminal));
    sync(terminal);
}

/// Unmap runs before the widget leaves its window, so the window is
/// always found.
fn sync(terminal: &gtk4::Widget) {
    let Some(window) = window_of(terminal) else {
        return;
    };
    let shown: Shown = data_or_default(&window, SHOWN_KEY);
    let mut shown = shown.borrow_mut();
    shown.retain(|weak| weak.upgrade().is_some_and(|other| &other != terminal));
    if terminal.is_mapped() && terminal.has_css_class(TRANSLUCENT_CLASS) {
        shown.push(terminal.downgrade());
    }
    if shown.is_empty() {
        window.remove_css_class(SEE_THROUGH_CLASS);
    } else {
        window.add_css_class(SEE_THROUGH_CLASS);
    }
}

#[cfg(test)]
pub(crate) mod gtk_tests {
    use std::time::Duration;

    use super::*;
    use crate::terminal::{TerminalBackend, VteBackend};
    use crate::ui::gtk_tests::FatalWarnings;

    /// The alpha `widget` renders at `(x, y)`, in its own coordinates.
    pub(crate) fn alpha_at(widget: &impl IsA<gtk4::Widget>, x: i32, y: i32) -> u8 {
        let (width, height) = (widget.width(), widget.height());
        let snapshot = gtk4::Snapshot::new();
        gtk4::WidgetPaintable::new(Some(widget)).snapshot(
            &snapshot,
            f64::from(width),
            f64::from(height),
        );
        let node = snapshot.to_node().expect("the widget draws");
        let renderer = widget
            .native()
            .and_then(|n| n.renderer())
            .expect("renderer");
        let texture = renderer.render_texture(
            node,
            Some(&gtk4::graphene::Rect::new(
                0.0,
                0.0,
                width as f32,
                height as f32,
            )),
        );
        let stride = width as usize * 4;
        let mut pixels = vec![0; stride * height as usize];
        // GDK_MEMORY_DEFAULT: premultiplied BGRA.
        texture.download(&mut pixels, stride);
        pixels[y as usize * stride + x as usize * 4 + 3]
    }

    fn settle() {
        let context = glib::MainContext::default();
        for _ in 0..50 {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A VTE terminal fades its default background only, and its window
    /// clears its own background while the terminal's tab is current.
    pub(in crate::ui) fn a_translucent_terminal_clears_the_window_under_it() {
        let _fatal = FatalWarnings::new();
        install();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let backend = VteBackend::new(tx);
        let terminal = backend.widget();
        follow(&terminal);

        let chrome = gtk4::Label::new(Some("chrome"));
        chrome.add_css_class(CHROME_CLASS);
        let notebook = gtk4::Notebook::new();
        notebook.append_page(&gtk4::Label::new(Some("home")), None::<&gtk4::Widget>);
        notebook.append_page(&terminal, None::<&gtk4::Widget>);
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&chrome);
        root.append(&notebook);
        let window = gtk4::Window::new();
        window.set_default_size(400, 300);
        window.set_child(Some(&root));
        window.present();

        backend.set_color_scheme("Dark");
        backend.set_opacity(0.5);
        // Two cells a program colored, top left.
        backend.write(b"\x1b[41m  \x1b[0m");
        assert!(terminal.has_css_class(TRANSLUCENT_CLASS));
        settle();
        assert!(!window.has_css_class(SEE_THROUGH_CLASS), "home is current");

        notebook.set_current_page(Some(1));
        settle();
        assert!(window.has_css_class(SEE_THROUGH_CLASS));
        let (w, h) = (terminal.width(), terminal.height());
        assert!(w > 0 && h > 0);
        let faded = alpha_at(&terminal, w - 2, h - 2);
        assert!((120..=135).contains(&faded), "default background {faded}");
        assert_eq!(alpha_at(&terminal, 1, 1), 255, "colored cells stay opaque");

        let inside = terminal
            .compute_point(
                &window,
                &gtk4::graphene::Point::new(w as f32 - 2.0, h as f32 - 2.0),
            )
            .expect("in the window");
        let through = alpha_at(&window, inside.x() as i32, inside.y() as i32);
        assert!(
            (120..=135).contains(&through),
            "window under the terminal {through}"
        );
        let label = chrome
            .compute_point(&window, &gtk4::graphene::Point::new(1.0, 1.0))
            .expect("in the window");
        assert_eq!(alpha_at(&window, label.x() as i32, label.y() as i32), 255);

        notebook.set_current_page(Some(0));
        settle();
        assert!(!window.has_css_class(SEE_THROUGH_CLASS), "home is current");
        notebook.set_current_page(Some(1));
        settle();
        backend.set_opacity(1.0);
        assert!(!terminal.has_css_class(TRANSLUCENT_CLASS));
        assert!(!window.has_css_class(SEE_THROUGH_CLASS));
        settle();
        assert_eq!(alpha_at(&terminal, w - 2, h - 2), 255);

        backend.set_opacity(f64::NAN);
        assert!(!terminal.has_css_class(TRANSLUCENT_CLASS));
        backend.set_opacity(0.3);
        assert!(window.has_css_class(SEE_THROUGH_CLASS));
        notebook.remove_page(Some(1));
        assert!(!window.has_css_class(SEE_THROUGH_CLASS), "closing the tab");
        window.close();
    }
}
