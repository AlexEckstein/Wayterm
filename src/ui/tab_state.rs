//! Display-free model for tab session persistence — doc 10 Phase 4
//! ("Remember open tabs on exit, restore on launch").
//!
//! # Where the list lives
//!
//! Doc 06 has no `open_tabs` table, and adding one would be the wrong
//! trade: the list is a single value that is always read whole and
//! written whole, while doc 06 already ships a key-value table for
//! exactly that shape — `settings(key TEXT PRIMARY KEY, value TEXT)`.
//! One JSON array in the `open_tabs` settings row therefore needs no
//! schema change, no migration, and no new index, and it inherits the
//! seed-once behaviour doc 06's own §"Known gap" note relies on (the
//! row only exists once the app writes it, so `SELECT EXISTS(SELECT 1
//! FROM settings)` keeps meaning "seeded"). The store side is
//! [`SessionStore::save_open_tabs`](crate::store::SessionStore::save_open_tabs)
//! / [`load_open_tabs`](crate::store::SessionStore::load_open_tabs).
//!
//! # Restore restores tabs, not connections
//!
//! A [`TabState`] records *which session a tab belongs to*, never any
//! credential, host, or connection handle (doc 06 §"Credential
//! Storage": secrets are keyring-only). On launch the tab frame comes
//! back — paned layout, sidebar skeleton, placeholder — and the
//! connection lands when the user clicks it; auto-connecting on launch
//! would mean re-prompting for credentials before the user has asked
//! for anything.
//!
//! # Testability
//!
//! Nothing in this module touches GTK: the scan/restore callers in
//! [`crate::ui::tab_restore`] hand plain values in and take plain
//! values out, so the whole wire format is exercised without a display.

use serde::{Deserialize, Serialize};

/// `settings` key holding the persisted open-tab list (doc 06's
/// key-value table — see the module docs for why not a table).
pub const OPEN_TABS_SETTING_KEY: &str = "open_tabs";

/// Trailing debounce window for the live save (doc 10 Phase 4 asks for
/// remembered tabs, not a write per keystroke).
///
/// A rename produces one `notify::text` per character, so saving on
/// every signal would turn a single rename into dozens of SQLite
/// writes. Two seconds of quiet is long enough to coalesce a whole
/// rename and short enough that a crash mid-typing loses nothing the
/// user would notice.
pub const SAVE_DEBOUNCE_MS: i64 = 2_000;

/// The saved list is a plain JSON array today.
///
/// `TabState` deserializes with `#[serde(default)]`, so a row written
/// before a field existed still restores (that field reads as its
/// `Default`), and serde ignores *unknown* keys by default, so a list
/// written by a newer Wayterm still restores in an older one as far as
/// the fields it understands. Both properties are asserted in
/// [`tests`]; a future format change should be a new key rather than a
/// reinterpretation of `open_tabs`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    /// `sessions.id` this tab was opened from, or `None` for a tab with
    /// no saved session behind it (ad-hoc shell, or a session that was
    /// deleted between runs — it still restores, unconnected).
    pub session_id: Option<i64>,
    /// Tab label as shown, so a restored tab reads the same even if the
    /// session row is gone or was renamed since.
    pub label: String,
    /// Creation timestamp, unix milliseconds ([`now_ms`]). Carried
    /// across restarts so restored tabs keep their age, and used as
    /// the deterministic tie-break for equal `order` values.
    pub created_at: i64,
    /// Left-to-right position among the persistable tabs, dense from 0.
    ///
    /// `order` is the notebook position, not a sortable preference: the
    /// tab bar is what the user sees, so the saved list reopens in
    /// exactly the arrangement they left behind.
    pub order: i64,
}

impl TabState {
    /// A tab bound to a saved session.
    pub fn session(session_id: i64, label: impl Into<String>, created_at: i64, order: i64) -> Self {
        Self {
            session_id: Some(session_id),
            label: label.into(),
            created_at,
            order,
        }
    }

    /// A tab with no saved session behind it.
    pub fn detached(label: impl Into<String>, created_at: i64, order: i64) -> Self {
        Self {
            session_id: None,
            label: label.into(),
            created_at,
            order,
        }
    }
}

/// Serialize the persistable tab list for the `settings` row.
pub fn tabs_to_json(tabs: &[TabState]) -> Result<String, serde_json::Error> {
    serde_json::to_string(tabs)
}

/// Parse the `settings` row back into a tab list.
///
/// Returns the serde error for malformed JSON; the caller decides
/// whether that means "start with no tabs" (the UI does) or something
/// louder.
pub fn tabs_from_json(json: &str) -> Result<Vec<TabState>, serde_json::Error> {
    serde_json::from_str(json)
}

/// The tabs in the order they must be reopened.
///
/// Sort key is `(order, created_at, session_id)` — a *total* order, so
/// the result is identical on every machine and every run. Sorting by
/// `order` alone would leave ties to the sort implementation's
/// behaviour; ties do happen (two tabs saved in the same millisecond),
/// and a restore that reorders tabs nondeterministically looks like
/// corruption to the user.
pub fn sorted_for_restore(tabs: &[TabState]) -> Vec<TabState> {
    let mut sorted = tabs.to_vec();
    sorted.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    sorted
}

/// Notebook page the restored tab at `index` occupies.
///
/// The welcome ("home") tab is created fresh at startup before any
/// session tab, so restored tabs start after it and keep their
/// relative order.
pub fn restore_page_index(index: usize) -> u32 {
    index as u32 + 1
}

/// Rewrite `order` to `0..len` in the slice's current order.
///
/// Called on a freshly scanned list so the persisted `order` is always
/// dense and equals notebook position; a list that arrives with gaps
/// (hand-edited, or from an interrupted write) would otherwise drift
/// further from the truth on every save.
pub fn renumber(tabs: &mut [TabState]) {
    for (index, tab) in tabs.iter_mut().enumerate() {
        tab.order = index as i64;
    }
}

/// One notebook page, read out of GTK — plain values so the scan is
/// testable without a display.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PageInfo {
    /// `sessions.id` from the page's widget data, if it is a session tab.
    pub session_id: Option<i64>,
    /// The tab label's text.
    pub label: String,
    /// The page's creation timestamp (unix millis).
    pub created_at: i64,
}

/// Turn live notebook pages into the persistable tab list.
///
/// `home_index` is the page number of the welcome tab, which the caller
/// identifies by widget identity and which is skipped: it is
/// reconstructed at every launch, so persisting it would duplicate it
/// on the next one. Every other page gets a dense `order` from 0,
/// left to right.
///
/// Identifying home by *position* instead would be simpler, but it
/// breaks the moment a tab is reordered — the welcome tab would be
/// saved and the first real tab dropped.
pub fn states_from_pages(pages: &[PageInfo], home_index: Option<usize>) -> Vec<TabState> {
    let mut tabs: Vec<TabState> = pages
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != home_index)
        .map(|(_, page)| match page.session_id {
            Some(session_id) => {
                TabState::session(session_id, page.label.clone(), page.created_at, 0)
            }
            None => TabState::detached(page.label.clone(), page.created_at, 0),
        })
        .collect();
    // Dense `order` from 0 over exactly the pages that survived the
    // filter, so the persisted order can never disagree with the tab
    // bar.
    renumber(&mut tabs);
    tabs
}

/// Unix time in milliseconds, saturating rather than panicking on a
/// pre-epoch or absurd system clock.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Trailing debounce state for the live save: "something changed at
/// `t`", "is the quiet period over yet?".
///
/// The clock is an explicit argument rather than a call to
/// [`now_ms`] so the policy is testable without waiting two seconds and
/// without a display — the timer itself is the only part that needs
/// GTK, and it lives in [`crate::ui::tab_restore`].
///
/// Trailing, not leading: a burst of events keeps pushing the deadline
/// out, so the save lands once the user stops acting. That is the
/// behaviour a rename needs (one write per rename, not one per
/// keystroke) and it is also why `cancel` exists — on exit we want the
/// write now, not after the quiet period.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DebouncedSave {
    interval_ms: i64,
    /// When the current burst started; `None` when clean.
    dirty_since_ms: Option<i64>,
}

impl DebouncedSave {
    /// A debouncer using [`SAVE_DEBOUNCE_MS`].
    pub fn new() -> Self {
        Self::with_interval(SAVE_DEBOUNCE_MS)
    }

    /// A debouncer with an explicit quiet period.
    pub fn with_interval(interval_ms: i64) -> Self {
        Self {
            interval_ms,
            dirty_since_ms: None,
        }
    }

    /// The quiet period in milliseconds.
    pub fn interval_ms(&self) -> i64 {
        self.interval_ms
    }

    /// Record a change at `now_ms`, (re)starting the quiet period.
    pub fn mark_dirty(&mut self, now_ms: i64) {
        self.dirty_since_ms = Some(now_ms);
    }

    /// When the pending change becomes due, if one is pending.
    ///
    /// `None` means the debouncer is clean — no change is waiting. That
    /// is the one predicate callers need, so there is deliberately no
    /// separate `is_dirty`.
    pub fn due_at_ms(&self) -> Option<i64> {
        self.dirty_since_ms
            .map(|since| since.saturating_add(self.interval_ms))
    }

    /// Whether the pending change is due at `now_ms`.
    pub fn is_due(&self, now_ms: i64) -> bool {
        match self.due_at_ms() {
            Some(due) => now_ms >= due,
            None => false,
        }
    }

    /// Consume the pending change if it is due at `now_ms`.
    ///
    /// Returns whether a save is owed. Consuming it here — rather than
    /// in the caller — is what stops a second save from firing for the
    /// same event when the timer and a window-close happen to coincide.
    pub fn take_due(&mut self, now_ms: i64) -> bool {
        if !self.is_due(now_ms) {
            return false;
        }
        self.dirty_since_ms = None;
        true
    }

    /// Drop the pending change without saving (the caller is saving
    /// synchronously instead).
    pub fn cancel(&mut self) {
        self.dirty_since_ms = None;
    }
}

#[cfg(test)]
mod tests;
