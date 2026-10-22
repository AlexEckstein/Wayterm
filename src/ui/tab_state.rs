//! The open-tab list's format and every decision about it that needs no
//! display (doc 03 §Restoring tabs Phase 4). It is one JSON array in the `open_tabs`
//! settings row — always read and written whole, so no table. Only tab
//! identity is stored, never a credential or connection detail.

use serde::{Deserialize, Serialize};

/// The settings row holding the list.
pub const OPEN_TABS_SETTING_KEY: &str = "open_tabs";

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

#[cfg(test)]
mod tests;
