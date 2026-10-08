//! GTK side of tab session persistence — doc 10 Phase 4 ("Remember
//! open tabs on exit, restore on launch").
//!
//! [`super::tab_state`] owns the wire format and every decision that
//! can be made without a display; this module owns the display side:
//! reading the notebook back into that format, keeping it saved while
//! the user works, and reopening it at launch.
//!
//! The three seams into the notebook are:
//!
//! * **save** — [`TabPersistence::schedule_save`] on tab open, close,
//!   and rename, coalesced by a 2-second trailing debounce so a rename
//!   is one SQLite write instead of one per keystroke;
//! * **restore** — [`TabPersistence::restore`] at launch, which reopens
//!   the *tabs*, not the connections (doc 10 Phase 4 is "remember open
//!   tabs"; credentials are keyring-only per doc 06, so there is
//!   nothing here to reconnect with, and nothing to prompt for);
//! * **exit** — [`TabPersistence::flush`] from the window's
//!   close-request, which abandons the debounce and writes once,
//!   synchronously, because the process is about to go away.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::tab::{SessionTab, SESSION_ID_KEY};
use crate::ui::tab_state::{self, DebouncedSave, PageInfo, TabState};

/// Widget-data key for a page's creation timestamp (unix millis).
///
/// Stamped on the page's content widget — the same place `tab.rs` keeps
/// the session id — so the scan can read a tab's age back without
/// asking the UI to remember it.
const TAB_CREATED_AT_KEY: &str = "wayterm-tab-created-at";

/// Mirrors a [`gtk4::Notebook`]'s tabs into the session store.
///
/// Held by the window's close-request handler; the notebook's own
/// signal handlers only hold a [`Weak`] reference back, so the window
/// dropping its handler is what finally drops this.
pub struct TabPersistence {
    notebook: gtk4::Notebook,
    /// The welcome tab. Identified by widget, not by position: a
    /// reordered notebook must still never persist it (see
    /// [`tab_state::states_from_pages`]).
    home: gtk4::Widget,
    store: SessionStore,
    debounce: RefCell<DebouncedSave>,
    /// The one pending save timer, or `None` when none is armed.
    ///
    /// At most one exists at a time: re-arming removes the previous
    /// source first, and the timer callback clears this before breaking
    /// out, so no source can be left pointing at a destroyed window.
    timer: RefCell<Option<glib::SourceId>>,
}

impl TabPersistence {
    /// Watch `notebook`, skipping the welcome tab `home`, and mirror it
    /// into `store`.
    pub fn new(notebook: gtk4::Notebook, home: gtk4::Widget, store: SessionStore) -> Rc<Self> {
        Rc::new(Self {
            notebook,
            home,
            store,
            debounce: RefCell::new(DebouncedSave::new()),
            timer: RefCell::new(None),
        })
    }

    /// Reopen the saved tabs, in order, and focus the last one.
    ///
    /// Restoring *creates the tab frame only* — the same placeholder
    /// path that opening a session from the tree takes when no
    /// connection is involved. No transport is created, no credential
    /// is read: restore remembers tabs, and the connection lands when
    /// the user asks for it by clicking the tab.
    ///
    /// Returns the number of saved tabs that were reopened (tabs whose
    /// session row has since been deleted reopen as unconnected
    /// placeholders, so the count is the number of *tabs*, not of live
    /// sessions).
    pub fn restore(self: &Rc<Self>) -> usize {
        let saved = match self.store.load_open_tabs() {
            Ok(tabs) => tabs,
            Err(err) => {
                // A corrupt row must not stop the app from starting; it
                // costs the user their tab layout for one run, and the
                // next save overwrites the bad row.
                eprintln!("Wayterm: could not read the saved tab list ({err}); starting with none");
                return 0;
            }
        };
        let ordered = tab_state::sorted_for_restore(&saved);
        let restored = ordered.len();
        for tab in &ordered {
            self.open_restored(tab);
        }
        if restored > 0 {
            // Focus the last restored tab, which is where opening them
            // one at a time would have left the notebook. Clamped
            // because deduplication (two saved rows for one session) can
            // collapse pages.
            let page = tab_state::restore_page_index(restored - 1)
                .min(self.notebook.n_pages().saturating_sub(1));
            self.notebook.set_current_page(Some(page));
        }
        restored
    }

    /// Start mirroring notebook changes into the store.
    ///
    /// Call after [`restore`](Self::restore): the pages that already
    /// exist are observed here, so the restore itself is not mistaken
    /// for the user opening a pile of tabs.
    pub fn install(self: &Rc<Self>) {
        for page_num in 0..self.notebook.n_pages() {
            self.observe_page(&self.notebook, page_num);
        }

        let weak = Rc::downgrade(self);
        self.notebook
            .connect_page_added(move |notebook, _, page_num| {
                if let Some(this) = weak.upgrade() {
                    this.observe_page(notebook, page_num);
                    this.schedule_save();
                }
            });

        let weak = Rc::downgrade(self);
        self.notebook.connect_page_removed(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.schedule_save();
            }
        });
    }

    /// Note that the tab list changed and (re)arm the debounce timer.
    pub fn schedule_save(self: &Rc<Self>) {
        self.debounce.borrow_mut().mark_dirty(tab_state::now_ms());
        self.rearm_timer();
    }

    /// Write the tab list now, abandoning any pending debounce.
    ///
    /// This is the exit path: the window is closing, so its timer is
    /// about to be torn down with it and there is no second chance.
    pub fn flush(self: &Rc<Self>) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
        self.debounce.borrow_mut().cancel();
        self.save_now();
    }

    /// The tabs to persist right now, in tab-bar order.
    pub fn current_states(&self) -> Vec<TabState> {
        tab_state::states_from_pages(&self.page_infos(), self.home_page_index())
    }

    // -- internals ---------------------------------------------------------

    /// Re-arm the single save timer.
    ///
    /// The previous source is removed before the next is added, so a
    /// burst of events leaves one timer behind — the trailing edge of
    /// the debounce, not a queue of pending writes.
    fn rearm_timer(self: &Rc<Self>) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
        // The debouncer owns the interval, so the timer and the policy
        // that tests exercise can never disagree about it.
        let interval_ms = self.debounce.borrow().interval_ms();
        let this = self.clone();
        let id = glib::timeout_add_local(
            Duration::from_millis(interval_ms.max(0) as u64),
            move || {
                // Breaking out of the loop consumes the source, so drop
                // the handle *without* removing it: removing a finished
                // source is a GLib programming error.
                *this.timer.borrow_mut() = None;
                this.save_if_due();
                glib::ControlFlow::Break
            },
        );
        *self.timer.borrow_mut() = Some(id);
    }

    fn save_if_due(&self) {
        if self.debounce.borrow_mut().take_due(tab_state::now_ms()) {
            self.save_now();
        }
    }

    fn save_now(&self) {
        if let Err(err) = self.store.save_open_tabs(&self.current_states()) {
            eprintln!("Wayterm: failed to save the open tab list: {err}");
        }
    }

    /// Stamp and watch a page that is already in the notebook.
    fn observe_page(self: &Rc<Self>, notebook: &gtk4::Notebook, page_num: u32) {
        let Some(content) = notebook.nth_page(Some(page_num)) else {
            return;
        };
        if created_at_of(&content).is_none() {
            stamp_created_at(&content, tab_state::now_ms());
        }
        self.watch_tab_label(notebook, &content);
    }

    /// Reopen one saved tab.
    fn open_restored(&self, tab: &TabState) {
        let content = match tab.session_id {
            Some(session_id) => match self.store.get_session(session_id) {
                Ok(config) => {
                    // The current tab-opening path: it rebuilds the tab
                    // frame (paned layout, sidebar skeleton,
                    // placeholder) and returns. P3-3's connect flow
                    // makes that same frame connectable when the tab is
                    // used; restore deliberately stops short of it.
                    SessionTab::open(&self.notebook, &config)
                }
                Err(err) => {
                    // The session was deleted between runs. The tab
                    // still comes back — silently dropping it would
                    // lose the user's layout over something they can
                    // fix in the tree — but it comes back unconnected.
                    eprintln!(
                        "Wayterm: tab '{}' points at session {session_id}, which no longer exists ({err}); restoring it unconnected",
                        tab.label
                    );
                    open_detached_tab(&self.notebook, tab)
                }
            },
            None => open_detached_tab(&self.notebook, tab),
        };
        // Restore the tab's original age, so a tab that has been open
        // for a week does not come back looking new.
        stamp_created_at(&content, tab.created_at);
    }

    /// Hook the tab label's text so a rename re-saves the list.
    ///
    /// `GtkNotebook` has no "label changed" signal, so the labels
    /// inside the tab label widget are hooked directly. A tab label
    /// with no `Label` in it (an icon-only tab, say) simply gets no
    /// hook — the list is still saved on open, on close, and on exit.
    fn watch_tab_label(self: &Rc<Self>, notebook: &gtk4::Notebook, content: &gtk4::Widget) {
        let Some(tab_label) = notebook.tab_label(content) else {
            return;
        };
        for label in labels_in(&tab_label) {
            let weak = Rc::downgrade(self);
            label.connect_notify_local(Some("text"), move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.schedule_save();
                }
            });
        }
    }

    fn home_page_index(&self) -> Option<usize> {
        self.notebook
            .page_num(&self.home)
            .map(|page_num| page_num as usize)
    }

    fn page_infos(&self) -> Vec<PageInfo> {
        (0..self.notebook.n_pages())
            .filter_map(|page_num| self.page_info(page_num))
            .collect()
    }

    fn page_info(&self, page_num: u32) -> Option<PageInfo> {
        let content = self.notebook.nth_page(Some(page_num))?;
        let session_id = session_id_of(&content);
        Some(PageInfo {
            session_id,
            label: self.page_label(page_num, session_id),
            created_at: created_at_of(&content).unwrap_or_else(tab_state::now_ms),
        })
    }

    /// The text shown on a tab, falling back to the session's stored
    /// name for a tab label that carries no text (an icon-only tab, or
    /// one GTK built for us) — an empty label would restore as a blank
    /// tab.
    fn page_label(&self, page_num: u32, session_id: Option<i64>) -> String {
        let content = self.notebook.nth_page(Some(page_num));
        let from_widget = content
            .as_ref()
            .and_then(|content| tab_label_text(&self.notebook, content));
        if let Some(text) = from_widget.filter(|text| !text.trim().is_empty()) {
            return text;
        }
        session_id
            .and_then(|id| self.store.get_session(id).ok())
            .map(|config| config.name)
            .unwrap_or_default()
    }
}

/// Open a tab with no session behind it.
///
/// Two cases land here: a saved tab whose session has since been
/// deleted, and a tab that was never bound to one. It rebuilds the
/// frame — label plus close button — and nothing else, because there is
/// no session to connect to and restore never connects.
fn open_detached_tab(notebook: &gtk4::Notebook, tab: &TabState) -> gtk4::Widget {
    let placeholder = gtk4::Label::new(Some(&format!(
        "{}\nrestored without a live session",
        tab.label
    )));
    placeholder.set_vexpand(true);
    placeholder.set_hexpand(true);
    placeholder.set_valign(gtk4::Align::Center);
    placeholder.set_halign(gtk4::Align::Center);
    placeholder.set_justify(gtk4::Justification::Center);

    let content: gtk4::Widget = placeholder.upcast();
    let tab_label = detached_tab_label(notebook, &tab.label, &content);
    notebook.append_page(&content, Some(&tab_label));

    content
}

/// Tab label for a detached tab: name plus close button.
///
/// Mirrors `tab.rs`'s shape. The close button finds its page by
/// identity, since a detached tab has no session id to look up.
fn detached_tab_label(
    notebook: &gtk4::Notebook,
    name: &str,
    content: &gtk4::Widget,
) -> gtk4::Widget {
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    box_.append(&gtk4::Label::new(Some(name)));

    let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
    close_button.set_has_frame(false);
    close_button.set_tooltip_text(Some("Close tab"));
    close_button.set_focus_on_click(false);

    let notebook = notebook.clone();
    let content = content.clone();
    close_button.connect_clicked(move |_| {
        if let Some(page_num) = notebook.page_num(&content) {
            notebook.remove_page(Some(page_num));
        }
    });

    box_.append(&close_button);
    box_.upcast()
}

/// The session id `tab.rs` tagged this page with, if any.
fn session_id_of(content: &gtk4::Widget) -> Option<i64> {
    // The same key `tab.rs` uses; see `SESSION_ID_KEY`.
    unsafe {
        content
            .data::<i64>(SESSION_ID_KEY)
            .map(|value| *value.as_ptr())
    }
}

/// The creation timestamp stamped on this page, if any.
fn created_at_of(content: &gtk4::Widget) -> Option<i64> {
    unsafe {
        content
            .data::<i64>(TAB_CREATED_AT_KEY)
            .map(|value| *value.as_ptr())
    }
}

fn stamp_created_at(content: &gtk4::Widget, created_at: i64) {
    unsafe {
        content.set_data(TAB_CREATED_AT_KEY, created_at);
    }
}

/// Text of the first `Label` inside a tab label widget.
fn tab_label_text(notebook: &gtk4::Notebook, content: &gtk4::Widget) -> Option<String> {
    let tab_label = notebook.tab_label(content)?;
    labels_in(&tab_label)
        .into_iter()
        .next()
        .map(|label| label.text().to_string())
}

/// Every `Label` at or below `widget`, depth first.
fn labels_in(widget: &gtk4::Widget) -> Vec<gtk4::Label> {
    let mut found = Vec::new();
    if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
        found.push(label.clone());
    }
    let mut child = widget.first_child();
    while let Some(node) = child {
        if let Some(label) = node.downcast_ref::<gtk4::Label>() {
            found.push(label.clone());
        }
        found.extend(labels_in(&node));
        child = node.next_sibling();
    }
    found
}
