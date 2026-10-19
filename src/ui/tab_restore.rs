//! Keeping the open-tab list saved and reopening it at launch (doc 10
//! Phase 4). [`super::tab_state`] owns the format and every display-free
//! decision; this is the notebook side.
//!
//! Saves happen on tab open, close, and rename, debounced so a rename is
//! one write rather than one per keystroke, and once more on exit. Restore
//! reopens tab frames only: connections start when the user asks, since
//! credentials would need prompting.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::host_picker::{local_shell_session, LOCAL_SHELL_ID};
use crate::ui::tab::{session_id_of, SessionTab};
use crate::ui::tab_label::build_tab_label;
use crate::ui::tab_state::{self, PageInfo, TabState};
use crate::ui::widgets::{centered_label, data, set_data};

/// A page's creation time (unix millis), so restored tabs keep their age.
const TAB_CREATED_AT_KEY: &str = "wayterm-tab-created-at";

/// Quiet time before a save, so a rename (one signal per keystroke) is a
/// single write.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Mirrors a notebook's tabs into the store. Owned by the window's close
/// handler; the notebook's handlers only hold weak references.
pub struct TabPersistence {
    notebook: gtk4::Notebook,
    /// The welcome tab, rebuilt every launch and never saved. Found by
    /// identity, so reordering cannot mistake another tab for it.
    home: gtk4::Widget,
    store: Rc<SessionStore>,
    /// The pending save, re-armed by every change.
    timer: RefCell<Option<glib::SourceId>>,
}

impl TabPersistence {
    pub fn new(notebook: gtk4::Notebook, home: gtk4::Widget, store: Rc<SessionStore>) -> Rc<Self> {
        Rc::new(Self {
            notebook,
            home,
            store,
            timer: RefCell::new(None),
        })
    }

    /// Reopen the saved tabs in order and focus the last one; returns how
    /// many were reopened. A corrupt list costs one launch's layout, never
    /// the launch.
    pub fn restore(self: &Rc<Self>) -> usize {
        let saved = match self.store.load_open_tabs() {
            Ok(tabs) => tabs,
            Err(err) => {
                eprintln!("Wayterm: could not read the saved tab list ({err}); starting with none");
                return 0;
            }
        };
        let ordered = tab_state::sorted_for_restore(&saved);
        for tab in &ordered {
            self.open_restored(tab);
        }
        if !ordered.is_empty() {
            // Clamped: two rows for one session collapse into one tab.
            let page = tab_state::restore_page_index(ordered.len() - 1)
                .min(self.notebook.n_pages().saturating_sub(1));
            self.notebook.set_current_page(Some(page));
        }
        ordered.len()
    }

    /// Start watching the notebook; call after [`restore`](Self::restore).
    pub fn install(self: &Rc<Self>) {
        for page_num in 0..self.notebook.n_pages() {
            self.observe_page(page_num);
        }
        let weak = Rc::downgrade(self);
        self.notebook.connect_page_added(move |_, _, page_num| {
            if let Some(this) = weak.upgrade() {
                this.observe_page(page_num);
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

    /// Note a change: the save runs once [`SAVE_DEBOUNCE`] has passed
    /// without another. The trailing-edge timer is the debounce, on the
    /// main loop's monotonic clock, so a wall-clock step cannot lose it.
    pub fn schedule_save(self: &Rc<Self>) {
        self.cancel_timer();
        let this = self.clone();
        let id = glib::timeout_add_local_once(SAVE_DEBOUNCE, move || {
            // A fired source is gone; removing it again would be a GLib
            // error, so just forget the id.
            this.timer.take();
            this.save_now();
        });
        self.timer.replace(Some(id));
    }

    /// Write now, dropping any pending save (the exit path).
    pub fn flush(self: &Rc<Self>) {
        self.cancel_timer();
        self.save_now();
    }

    fn cancel_timer(&self) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
    }

    /// The tabs to persist, in tab-bar order.
    pub fn current_states(&self) -> Vec<TabState> {
        let pages: Vec<PageInfo> = (0..self.notebook.n_pages())
            .filter_map(|page_num| self.page_info(page_num))
            .collect();
        let home = self.notebook.page_num(&self.home).map(|n| n as usize);
        tab_state::states_from_pages(&pages, home)
    }

    fn save_now(&self) {
        if let Err(err) = self.store.save_open_tabs(&self.current_states()) {
            eprintln!("Wayterm: failed to save the open tab list: {err}");
        }
    }

    /// Stamp a page's age and re-save when its label text changes
    /// (GtkNotebook has no "label changed" signal).
    fn observe_page(self: &Rc<Self>, page_num: u32) {
        let Some(content) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        if data::<i64>(&content, TAB_CREATED_AT_KEY).is_none() {
            set_data(&content, TAB_CREATED_AT_KEY, tab_state::now_ms());
        }
        let Some(tab_label) = self.notebook.tab_label(&content) else {
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

    fn open_restored(&self, tab: &TabState) {
        // The New Tab picker's local shell is unsaved: it has its own id.
        let config = tab.session_id.map(|id| match id {
            LOCAL_SHELL_ID => (id, Ok(local_shell_session())),
            id => (id, self.store.get_session(id)),
        });
        let content = match config {
            Some((_, Ok(config))) => SessionTab::restore(&self.notebook, &config, &self.store),
            Some((id, Err(err))) => {
                eprintln!(
                    "Wayterm: tab '{}' points at session {id}, which no longer exists ({err}); restoring it unconnected",
                    tab.label
                );
                self.open_detached(&tab.label)
            }
            None => self.open_detached(&tab.label),
        };
        set_data(&content, TAB_CREATED_AT_KEY, tab.created_at);
    }

    /// A tab with no session behind it: label and close button only.
    fn open_detached(&self, label: &str) -> gtk4::Widget {
        let content: gtk4::Widget =
            centered_label(&format!("{label}\nrestored without a live session")).upcast();
        let (notebook, page) = (self.notebook.clone(), content.clone());
        let (tab_label, _) = build_tab_label(label, move || {
            if let Some(page_num) = notebook.page_num(&page) {
                notebook.remove_page(Some(page_num));
            }
        });
        self.notebook.append_page(&content, Some(&tab_label));
        content
    }

    fn page_info(&self, page_num: u32) -> Option<PageInfo> {
        let content = self.notebook.nth_page(Some(page_num))?;
        let session_id = session_id_of(&content);
        // An empty label would restore as a blank tab: fall back to the
        // session's stored name.
        let label = self
            .notebook
            .tab_label(&content)
            .and_then(|tab_label| labels_in(&tab_label).into_iter().next())
            .map(|label| label.text().to_string())
            .filter(|text| !text.trim().is_empty())
            .or_else(|| Some(self.store.get_session(session_id?).ok()?.name))
            .unwrap_or_default();
        Some(PageInfo {
            session_id,
            label,
            created_at: data::<i64>(&content, TAB_CREATED_AT_KEY).unwrap_or_else(tab_state::now_ms),
        })
    }
}

/// Every `Label` at or below `widget`, depth first.
fn labels_in(widget: &gtk4::Widget) -> Vec<gtk4::Label> {
    let mut found: Vec<gtk4::Label> = widget.downcast_ref().cloned().into_iter().collect();
    let mut child = widget.first_child();
    while let Some(node) = child {
        found.extend(labels_in(&node));
        child = node.next_sibling();
    }
    found
}
