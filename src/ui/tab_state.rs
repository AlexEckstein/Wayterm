//! The open-tab list's format and every decision about it that needs no
//! display (doc 10 Phase 4). It is one JSON array in the `open_tabs`
//! settings row — always read and written whole, so no table. Only tab
//! identity is stored, never a credential or connection detail.

use serde::{Deserialize, Serialize};

/// The settings row holding the list.
pub const OPEN_TABS_SETTING_KEY: &str = "open_tabs";

/// Quiet time before a save, so a rename (one signal per keystroke) is a
/// single write.
pub const SAVE_DEBOUNCE_MS: i64 = 2_000;

/// One saved tab. Missing fields default and unknown ones are ignored, so
/// lists from older and newer builds both restore; an incompatible format
/// change should use a new settings key.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    /// `None` for a tab without a saved session (it restores unconnected).
    pub session_id: Option<i64>,
    /// As shown, so the tab reads the same even if the row was renamed.
    pub label: String,
    /// Unix millis; restored tabs keep their age.
    pub created_at: i64,
    /// Notebook position among saved tabs, dense from 0.
    pub order: i64,
}

impl TabState {
    pub fn session(session_id: i64, label: impl Into<String>, created_at: i64, order: i64) -> Self {
        Self {
            session_id: Some(session_id),
            label: label.into(),
            created_at,
            order,
        }
    }

    pub fn detached(label: impl Into<String>, created_at: i64, order: i64) -> Self {
        Self {
            session_id: None,
            label: label.into(),
            created_at,
            order,
        }
    }
}

pub fn tabs_to_json(tabs: &[TabState]) -> Result<String, serde_json::Error> {
    serde_json::to_string(tabs)
}

/// Malformed JSON is an error, for the caller to handle.
pub fn tabs_from_json(json: &str) -> Result<Vec<TabState>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Reopen order: `(order, created_at, session_id)`, a total order so ties
/// never reorder tabs between runs.
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

/// The notebook page of restored tab `index` (after the home tab).
pub fn restore_page_index(index: usize) -> u32 {
    index as u32 + 1
}

/// Rewrite `order` to `0..len`, closing any gaps.
pub fn renumber(tabs: &mut [TabState]) {
    for (index, tab) in tabs.iter_mut().enumerate() {
        tab.order = index as i64;
    }
}

/// One notebook page as plain values.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PageInfo {
    pub session_id: Option<i64>,
    pub label: String,
    pub created_at: i64,
}

/// The pages to save, skipping the home tab (rebuilt every launch; found
/// by identity, so reordering cannot confuse it).
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
    renumber(&mut tabs);
    tabs
}

/// Unix millis; never panics on an odd clock.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Trailing-edge debounce: each change pushes the deadline out. The clock
/// is a parameter so the policy is testable without waiting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DebouncedSave {
    interval_ms: i64,
    /// The latest change; `None` when clean.
    dirty_since_ms: Option<i64>,
}

impl DebouncedSave {
    pub fn new() -> Self {
        Self::with_interval(SAVE_DEBOUNCE_MS)
    }

    pub fn with_interval(interval_ms: i64) -> Self {
        Self {
            interval_ms,
            dirty_since_ms: None,
        }
    }

    pub fn interval_ms(&self) -> i64 {
        self.interval_ms
    }

    pub fn mark_dirty(&mut self, now_ms: i64) {
        self.dirty_since_ms = Some(now_ms);
    }

    /// When the pending change is due; `None` when clean.
    pub fn due_at_ms(&self) -> Option<i64> {
        self.dirty_since_ms
            .map(|since| since.saturating_add(self.interval_ms))
    }

    pub fn is_due(&self, now_ms: i64) -> bool {
        match self.due_at_ms() {
            Some(due) => now_ms >= due,
            None => false,
        }
    }

    /// Consume the change if due; returns whether a save is owed (so a
    /// timer and a window close never both save it).
    pub fn take_due(&mut self, now_ms: i64) -> bool {
        if !self.is_due(now_ms) {
            return false;
        }
        self.dirty_since_ms = None;
        true
    }

    /// Drop the pending change (the caller saves right away instead).
    pub fn cancel(&mut self) {
        self.dirty_since_ms = None;
    }
}

#[cfg(test)]
mod tests;
