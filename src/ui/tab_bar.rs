//! The notebook's tab bar (doc 01 §Tab bar): a "+" drawn as a tab right
//! after the last tab opens a local shell (doc 03 §New Tab); once the tabs
//! overflow the bar they scroll, with arrows at both ends, and "+" stays
//! pinned at the bar's end.
//!
//! GtkNotebook stretches its tab strip over the whole header, so an end
//! action widget sits at the far right, and a scrollable strip asks for
//! only one tab's width. So the bar has two layouts, picked after every
//! frame:
//!
//! - fitting: not scrollable, the strip at its natural width (the sum of
//!   its tabs), "+" after it and the spare width after "+";
//! - overflowing: scrollable, the strip filling the bar, "+" at its end.
//!
//! Overflow shows as no spare width: the notebook is then allocated its
//! minimum (all the tabs) and clipped by its parent, so its own width
//! cannot tell.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::host_picker::local_shell_session;
use crate::ui::tab::SessionTab;

/// The "+" tab's tooltip and accessible name.
const NEW_TAB_LABEL: &str = "New local shell tab";

/// Give `notebook` its "+" tab and fit its tab bar from now on.
pub fn install(notebook: &gtk4::Notebook, store: &Rc<SessionStore>) {
    let plus = new_tab_button(notebook, store);
    // Named as the strip and a tab, so the theme draws "+" as a tab.
    let end = glib::Object::builder::<gtk4::Box>()
        .property("css-name", "tabs")
        .build();
    end.append(&plus);
    notebook.set_action_widget(&end, gtk4::PackType::End);

    let Some(strip) = child_named(notebook.upcast_ref(), "header")
        .and_then(|header| child_named(&header, "tabs"))
    else {
        eprintln!("Wayterm: no notebook tab strip; \"+\" stays at the tab bar's end");
        notebook.set_scrollable(true);
        return;
    };
    let bar = Rc::new(TabBar {
        notebook: notebook.clone(),
        strip,
        end,
        plus: plus.upcast(),
        strip_extra: Cell::new(0),
    });
    bar.set_overflowing(false);

    let watch: Rc<RefCell<Option<(gtk4::gdk::FrameClock, glib::SignalHandlerId)>>> = Rc::default();
    let for_realize = watch.clone();
    notebook.connect_realize(move |notebook| {
        let Some(clock) = notebook.frame_clock() else {
            return;
        };
        let bar = bar.clone();
        let id = clock.connect_after_paint(move |_| bar.fit());
        for_realize.replace(Some((clock, id)));
    });
    notebook.connect_unrealize(move |_| {
        if let Some((clock, id)) = watch.take() {
            clock.disconnect(id);
        }
    });
}

/// The "+": a local shell in a new tab, without the host picker that
/// `new_tab` opens.
fn new_tab_button(notebook: &gtk4::Notebook, store: &Rc<SessionStore>) -> gtk4::Button {
    let button = glib::Object::builder::<gtk4::Button>()
        .property("css-name", "tab")
        .property("icon-name", "list-add-symbolic")
        .build();
    // Like the tabs, it is clicked, not focused; Ctrl+Shift+T is the
    // keyboard's way to a new tab.
    button.set_focusable(false);
    button.set_tooltip_text(Some(NEW_TAB_LABEL));
    button.update_property(&[gtk4::accessible::Property::Label(NEW_TAB_LABEL)]);
    let (notebook, store) = (notebook.clone(), store.clone());
    button.connect_clicked(move |_| {
        SessionTab::open(&notebook, &local_shell_session(), &store);
    });
    button
}

struct TabBar {
    notebook: gtk4::Notebook,
    /// GtkNotebook's own tab strip.
    strip: gtk4::Widget,
    /// The end action widget holding "+".
    end: gtk4::Box,
    plus: gtk4::Widget,
    /// The strip's width beyond its tabs (its CSS box), measured while it
    /// fits: a scrolling strip reports less than its tabs.
    strip_extra: Cell<i32>,
}

impl TabBar {
    fn set_overflowing(&self, overflowing: bool) {
        self.notebook.set_scrollable(overflowing);
        self.strip.set_hexpand(overflowing);
        self.end.set_hexpand(!overflowing);
    }

    /// Switch layouts when the tabs start or stop fitting.
    fn fit(&self) {
        let overflowing = self.notebook.is_scrollable();
        let fits = if overflowing {
            let Some(bar) = self.strip.parent() else {
                return;
            };
            let need = self.tabs_width() + self.strip_extra.get() + min_width(&self.end);
            // Strictly: fitting needs spare width, so both tests agree and
            // the bar never flips back and forth.
            need < bar.width()
        } else {
            self.strip_extra
                .set(min_width(&self.strip) - self.tabs_width());
            self.end.width() > min_width(&self.plus)
        };
        if fits == overflowing {
            self.set_overflowing(!fits);
        }
    }

    /// The shown tabs' total width, as GtkNotebook sums them.
    fn tabs_width(&self) -> i32 {
        (0..self.notebook.n_pages())
            .filter_map(|page_num| self.notebook.nth_page(Some(page_num)))
            .filter(|page| page.is_visible())
            .filter_map(|page| self.notebook.tab_label(&page)?.parent())
            .map(|tab| min_width(&tab))
            .sum()
    }
}

fn min_width(widget: &impl IsA<gtk4::Widget>) -> i32 {
    widget.measure(gtk4::Orientation::Horizontal, -1).0
}

/// `widget`'s first child with CSS name `name`.
fn child_named(widget: &gtk4::Widget, name: &str) -> Option<gtk4::Widget> {
    let mut child = widget.first_child();
    while let Some(node) = child {
        if node.css_name() == name {
            return Some(node);
        }
        child = node.next_sibling();
    }
    None
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Run frames until `done`, so the bar refits after each change.
    fn spin_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn left_and_right(widget: &impl IsA<gtk4::Widget>, notebook: &gtk4::Notebook) -> (f32, f32) {
        let bounds = widget.compute_bounds(notebook).expect("bounds");
        (bounds.x(), bounds.x() + bounds.width())
    }

    /// "+" follows the last tab while the tabs fit; past that the tabs
    /// scroll and "+" sits at the bar's end; closing tabs brings it back.
    pub(in crate::ui) fn plus_follows_the_tabs_then_pins_when_they_scroll() {
        let store = Rc::new(SessionStore::in_memory().expect("in-memory store"));
        let notebook = gtk4::Notebook::new();
        install(&notebook, &store);
        let window = gtk4::Window::builder()
            .default_width(600)
            .default_height(200)
            .child(&notebook)
            .build();
        window.present();
        let add = |n: usize| {
            for i in 0..n {
                let label = gtk4::Label::new(Some(&format!("Tab {i}")));
                notebook.append_page(&gtk4::Label::new(None), Some(&label));
            }
        };
        let plus = notebook
            .action_widget(gtk4::PackType::End)
            .and_then(|end| end.first_child())
            .and_downcast::<gtk4::Button>()
            .expect("the + tab");
        assert_eq!(plus.css_name(), "tab");
        let last_tab = || {
            let page = notebook.nth_page(Some(notebook.n_pages() - 1)).unwrap();
            notebook
                .tab_label(&page)
                .and_then(|label| label.parent())
                .unwrap()
        };

        add(2);
        spin_until("two tabs laid out", || {
            notebook.width() > 0 && left_and_right(&last_tab(), &notebook).1 > 0.0
        });
        assert!(!notebook.is_scrollable());
        let (_, tab_end) = left_and_right(&last_tab(), &notebook);
        let (plus_start, plus_end) = left_and_right(&plus, &notebook);
        assert!(
            (tab_end..tab_end + 40.0).contains(&plus_start),
            "+ at {plus_start} follows the last tab ending at {tab_end}"
        );
        assert!(
            plus_end < notebook.width() as f32 / 2.0,
            "+ is not pinned right"
        );

        add(40);
        spin_until("the tabs scroll", || notebook.is_scrollable());
        spin_until("+ pinned at the end", || {
            left_and_right(&plus, &notebook).1 > notebook.width() as f32 - 20.0
        });

        while notebook.n_pages() > 2 {
            notebook.remove_page(Some(0));
        }
        spin_until("the tabs fit again", || !notebook.is_scrollable());
        spin_until("+ follows the last tab again", || {
            left_and_right(&plus, &notebook).1 < notebook.width() as f32 / 2.0
        });
        window.destroy();
    }
}
