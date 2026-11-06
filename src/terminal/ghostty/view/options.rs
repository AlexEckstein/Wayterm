//! The options every view shares (doc 02 §View options): installed once,
//! they reach open views and each new one.

use std::cell::RefCell;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use super::GhosttyView;
use crate::terminal::ghostty::ffi;
use crate::terminal::options::{GhosttyCursor, GhosttyOptions};

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
        self.apply_cursor(&options);
        self.apply_image_limit(&options);
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
        // Only a change: this takes the cursor from a program that set one.
        if (previous.cursor_style, previous.cursor_blinks)
            != (options.cursor_style, options.cursor_blinks)
        {
            self.apply_cursor(&options);
        }
        if previous.image_limit_mb != options.image_limit_mb {
            self.apply_image_limit(&options);
        }
        self.queue_draw();
    }

    fn apply_cursor(&self, options: &GhosttyOptions) {
        if let Some(shared) = self.shared() {
            let style = engine_cursor(options.cursor_style);
            shared
                .engine
                .set_cursor_default(style, options.cursor_blinks);
        }
    }

    fn apply_image_limit(&self, options: &GhosttyOptions) {
        if let Some(shared) = self.shared() {
            let bytes = u64::from(options.image_limit_mb) * 1_000_000;
            shared.engine.set_image_limit(bytes);
        }
    }
}

fn engine_cursor(cursor: GhosttyCursor) -> u8 {
    match cursor {
        GhosttyCursor::Block => ffi::CURSOR_BLOCK,
        GhosttyCursor::Bar => ffi::CURSOR_BAR,
        GhosttyCursor::Underline => ffi::CURSOR_UNDERLINE,
        GhosttyCursor::Hollow => ffi::CURSOR_BLOCK_HOLLOW,
    }
}

#[cfg(test)]
mod tests {
    use crate::terminal::ghostty::ffi;
    use crate::terminal::options::{GhosttyOptions, IMAGE_LIMIT};

    /// The default option is what a terminal already holds, so a tab with
    /// no row set behaves as one built without it.
    #[test]
    fn the_image_limit_default_is_the_terminals_own() {
        let default = u64::from(GhosttyOptions::default().image_limit_mb);
        assert_eq!(default * 1_000_000, ffi::KITTY_STORAGE_LIMIT);
        assert_eq!(
            u64::from(*IMAGE_LIMIT.end()) * 1_000_000,
            ffi::KITTY_STORAGE_LIMIT
        );
    }
}
