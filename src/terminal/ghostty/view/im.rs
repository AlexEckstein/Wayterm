//! Input methods: IBus, Fcitx, dead keys and Compose.
//!
//! Every key press goes to a `GtkIMMulticontext` first and on per
//! [`key_route`]. GTK's context commits plain typing too, so the view
//! filters keys itself instead of `EventControllerKey::set_im_context`: a
//! commit during an uncomposed press stays that key's text and reaches the
//! key encoder, which Kitty keyboard modes need, and only composed text
//! leaves as plain bytes. The unfinished text is drawn at the cursor, and
//! the input method is told where that is for its candidate window.

use std::cell::{Cell, RefCell};

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, pango};

use super::{pixels, rgba, GhosttyView, Metrics};
use crate::terminal::ghostty::engine::{FrameInfo, Scroll};

#[derive(Default)]
pub(super) struct ImState {
    context: RefCell<Option<gtk4::IMContext>>,
    preedit: RefCell<Option<Preedit>>,
    /// A key press is inside the input method; commits wait in `committed`.
    in_key: Cell<bool>,
    committed: RefCell<String>,
    /// The cursor area last sent to the input method.
    location: Cell<Option<gdk::Rectangle>>,
}

impl ImState {
    pub(super) fn composing(&self) -> bool {
        self.preedit.borrow().is_some()
    }
}

/// The unfinished text. `attrs` are the input method's own, which is how
/// it marks the clause being converted; `cursor` is a byte index.
struct Preedit {
    text: String,
    attrs: pango::AttrList,
    cursor: usize,
}

/// What a key press does once the input method has seen it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyRoute {
    /// The input method kept it.
    Kept,
    Key,
    /// Plain typing: the key, carrying the committed text.
    KeyWithText,
    /// Composed text; the key only finished it.
    Text,
    /// Composed text, then the key: Enter after Hangul commits the syllable
    /// and is still an Enter.
    TextThenKey,
}

/// `filtered` is the input method's answer, `composing` whether it held
/// unfinished text before or after the press.
fn key_route(filtered: bool, composing: bool, committed: bool) -> KeyRoute {
    match (filtered, committed) {
        (true, false) => KeyRoute::Kept,
        (false, false) => KeyRoute::Key,
        (true, true) if composing => KeyRoute::Text,
        (true, true) => KeyRoute::KeyWithText,
        (false, true) => KeyRoute::TextThenKey,
    }
}

/// The byte index of the `chars`-th character; past the end, or negative
/// as an input method may report, the text's end.
fn byte_index(text: &str, chars: i32) -> usize {
    usize::try_from(chars)
        .ok()
        .and_then(|chars| text.char_indices().nth(chars).map(|(index, _)| index))
        .unwrap_or(text.len())
}

/// Where text of `size` starting at `at` goes to stay inside `bounds`: it
/// moves left and up as far as it must, and no further than the corner.
fn preedit_origin(at: (i32, i32), size: (i32, i32), bounds: (i32, i32)) -> (i32, i32) {
    let fit = |at: i32, size: i32, bound: i32| at.min(bound - size).max(0);
    (fit(at.0, size.0, bounds.0), fit(at.1, size.1, bounds.1))
}

impl GhosttyView {
    fn im(&self) -> Option<gtk4::IMContext> {
        self.imp().im.context.borrow().clone()
    }

    pub(super) fn install_im(&self, im: gtk4::IMContext) {
        im.set_input_purpose(gtk4::InputPurpose::Terminal);
        let view = self.downgrade();
        im.connect_commit(move |_, text| {
            if let Some(view) = view.upgrade() {
                view.im_commit(text);
            }
        });
        let view = self.downgrade();
        im.connect_preedit_changed(move |im| {
            if let Some(view) = view.upgrade() {
                view.preedit_changed(im);
            }
        });
        let view = self.downgrade();
        im.connect_preedit_end(move |im| {
            if let Some(view) = view.upgrade() {
                view.preedit_changed(im);
            }
        });
        self.imp().im.context.replace(Some(im));
    }

    pub(super) fn im_realized(&self, realized: bool) {
        if let Some(im) = self.im() {
            im.set_client_widget(realized.then_some(self));
        }
    }

    /// Losing focus drops what was being composed, so a tab in the
    /// background shows none. Gaining it sends the cursor area again: the
    /// input method may have forgotten it.
    pub(super) fn im_focus(&self, gained: bool) {
        let Some(im) = self.im() else { return };
        if gained {
            self.imp().im.location.set(None);
            im.focus_in();
        } else {
            im.reset();
            im.focus_out();
        }
    }

    /// Show the input method the key event `keys` is handling.
    pub(super) fn im_filter(&self, keys: &gtk4::EventControllerKey) -> bool {
        match (self.im(), keys.current_event()) {
            (Some(im), Some(event)) => im.filter_keypress(&event),
            _ => false,
        }
    }

    /// A key press: `filter` offers it to the input method, then it goes
    /// where [`key_route`] says. `false` lets it propagate.
    pub(super) fn key_event(
        &self,
        keyval: gdk::Key,
        state: gdk::ModifierType,
        filter: impl FnOnce() -> bool,
    ) -> bool {
        let im = &self.imp().im;
        let was_composing = im.composing();
        im.in_key.set(true);
        let filtered = filter();
        im.in_key.set(false);
        let composing = was_composing || im.composing();
        let committed = im.committed.take();
        match key_route(filtered, composing, !committed.is_empty()) {
            KeyRoute::Kept => true,
            KeyRoute::Key => self.key(keyval, state),
            KeyRoute::KeyWithText => {
                if !self.key_with_text(keyval, state, &committed) {
                    self.send_text(&committed);
                }
                true
            }
            KeyRoute::Text => {
                self.send_text(&committed);
                true
            }
            KeyRoute::TextThenKey => {
                self.send_text(&committed);
                self.key(keyval, state);
                true
            }
        }
    }

    /// Text from the input method. Outside a key press it is finished
    /// text; an input method answering asynchronously always lands here.
    fn im_commit(&self, text: &str) {
        let im = &self.imp().im;
        if im.in_key.get() {
            im.committed.borrow_mut().push_str(text);
        } else {
            self.send_text(text);
        }
    }

    fn send_text(&self, text: &str) {
        let Some(shared) = self.shared() else { return };
        shared.engine.clear_selection();
        shared.engine.clear_search();
        shared.input(text.as_bytes().to_vec());
        self.queue_draw();
    }

    /// Composing starts at the cursor, so it brings the viewport back to
    /// it as typing does.
    fn preedit_changed(&self, im: &gtk4::IMContext) {
        let (text, attrs, cursor) = im.preedit_string();
        let preedit = (!text.is_empty()).then(|| Preedit {
            cursor: byte_index(&text, cursor),
            text: text.into(),
            attrs,
        });
        if let (true, Some(shared)) = (preedit.is_some(), self.shared()) {
            shared.engine.scroll(Scroll::Bottom);
        }
        self.imp().im.preedit.replace(preedit);
        self.queue_draw();
    }

    /// Tell the input method where the cursor is. It may answer with a
    /// commit, which must not land in the middle of a snapshot.
    pub(super) fn follow_im_cursor(&self, info: &FrameInfo, metrics: &Metrics) {
        let imp = self.imp();
        if !imp.focused.get() {
            return;
        }
        let area = gdk::Rectangle::new(
            i32::from(info.cursor_x) * metrics.width,
            i32::from(info.cursor_y) * metrics.height,
            metrics.width,
            metrics.height,
        );
        if imp.im.location.replace(Some(area)) == Some(area) {
            return;
        }
        let view = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(im) = view.upgrade().and_then(|view| view.im()) {
                im.set_cursor_location(&area);
            }
        });
    }

    /// The unfinished text over the grid, from the cursor's cell, with its
    /// own cursor. Pango lays it out: it is not in the terminal yet, so it
    /// keeps the input method's styling and need not sit on cells.
    pub(super) fn paint_preedit(
        &self,
        snapshot: &gtk4::Snapshot,
        info: &FrameInfo,
        metrics: &Metrics,
    ) {
        let preedit = self.imp().im.preedit.borrow();
        let Some(preedit) = preedit.as_ref() else {
            return;
        };
        let layout = self.create_pango_layout(Some(&preedit.text));
        layout.set_font_description(Some(&self.font()));
        layout.set_attributes(Some(&preedit.attrs));
        layout.set_width(self.width() * pango::SCALE);
        layout.set_wrap(pango::WrapMode::Char);

        // On the cell's baseline: a fallback font's line can be taller.
        let rise = (metrics.baseline - pixels(layout.baseline())).round() as i32;
        let cell = (
            i32::from(info.cursor_x) * metrics.width,
            i32::from(info.cursor_y) * metrics.height + rise,
        );
        let (width, height) = layout.pixel_size();
        let (x, y) = preedit_origin(cell, (width, height), (self.width(), self.height()));
        let (x, y) = (x as f32, y as f32);
        snapshot.append_color(
            &rgba(info.bg, 1.0),
            &graphene::Rect::new(x, y, width as f32, height as f32),
        );
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x, y));
        snapshot.append_layout(&layout, &rgba(info.fg, 1.0));
        let index = i32::try_from(preedit.cursor).unwrap_or(i32::MAX);
        let (cursor, _) = layout.cursor_pos(index);
        let color = if info.cursor_has_rgb != 0 {
            info.cursor_rgb
        } else {
            info.fg
        };
        snapshot.append_color(
            &rgba(color, 1.0),
            &graphene::Rect::new(
                pixels(cursor.x()),
                pixels(cursor.y()),
                metrics.underline.1,
                pixels(cursor.height()),
            ),
        );
        snapshot.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_goes_where_the_input_method_leaves_it() {
        use KeyRoute::{Kept, Key, KeyWithText, Text, TextThenKey};
        // Declined and silent: an ordinary key, composing or not, so Ctrl+C
        // still interrupts with text half composed.
        assert_eq!(key_route(false, false, false), Key);
        assert_eq!(key_route(false, true, false), Key);
        // A dead key, a syllable in progress, an answer still to come.
        assert_eq!(key_route(true, true, false), Kept);
        assert_eq!(key_route(true, false, false), Kept);
        assert_eq!(key_route(true, false, true), KeyWithText);
        assert_eq!(key_route(true, true, true), Text);
        assert_eq!(key_route(false, true, true), TextThenKey);
        assert_eq!(key_route(false, false, true), TextThenKey);
    }

    #[test]
    fn a_preedit_cursor_counts_characters() {
        assert_eq!(byte_index("你好", 0), 0);
        assert_eq!(byte_index("你好", 1), 3);
        assert_eq!(byte_index("你好", 2), 6);
        assert_eq!(byte_index("你好", 9), 6);
        assert_eq!(byte_index("你好", -1), 6);
        assert_eq!(byte_index("", 0), 0);
    }

    #[test]
    fn preedit_text_moves_only_as_far_as_it_must() {
        let bounds = (100, 50);
        assert_eq!(preedit_origin((10, 20), (30, 10), bounds), (10, 20));
        assert_eq!(preedit_origin((90, 20), (30, 10), bounds), (70, 20));
        assert_eq!(preedit_origin((90, 45), (30, 10), bounds), (70, 40));
        // Taller or wider than the view: the corner, never off the top.
        assert_eq!(preedit_origin((90, 45), (200, 80), bounds), (0, 0));
        assert_eq!(preedit_origin((10, -2), (30, 10), bounds), (10, 0));
    }
}
