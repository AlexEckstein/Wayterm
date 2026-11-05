//! The options every view shares (doc 02 §View options): installed once,
//! they reach open views and each new one.

use std::cell::RefCell;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use super::GhosttyView;
use crate::terminal::defaults::GhosttyOptions;

#[derive(Default)]
struct Installed {
    options: GhosttyOptions,
    views: Vec<glib::WeakRef<GhosttyView>>,
}

thread_local! {
    static INSTALLED: RefCell<Installed> = RefCell::default();
}

/// Make `options` every view's. Main thread, as views are.
pub fn install_options(options: &GhosttyOptions) {
    let views: Vec<GhosttyView> = INSTALLED.with(|installed| {
        let mut installed = installed.borrow_mut();
        installed.options = options.clone();
        let views = installed.views.iter();
        views.filter_map(glib::WeakRef::upgrade).collect()
    });
    for view in views {
        view.set_options(options.clone());
    }
}

impl GhosttyView {
    /// Take the installed options, now and whenever they change.
    pub(super) fn follow_options(&self) {
        let options = INSTALLED.with(|installed| {
            let mut installed = installed.borrow_mut();
            installed.views.retain(|view| view.upgrade().is_some());
            installed.views.push(self.downgrade());
            installed.options.clone()
        });
        self.imp().options.replace(options);
    }

    /// Apply `options` at once. A stopped blink timer restarts on the next
    /// paint, at the new period.
    pub(super) fn set_options(&self, options: GhosttyOptions) {
        let previous = self.imp().options.replace(options.clone());
        if previous.cursor_blink != options.cursor_blink {
            self.stop_blink();
        }
        if previous.text_blink != options.text_blink {
            self.stop_text_blink();
        }
        self.queue_draw();
    }
}
