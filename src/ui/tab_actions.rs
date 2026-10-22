//! The tab window actions (doc 11 §Handlers): `new_tab` (the host
//! picker), `close_tab`, `next_tab` / `prev_tab`, `goto_tab_1` …
//! `goto_tab_9`, and `rename_tab`. The Home page never counts as a tab:
//! it is hidden beside open sessions and is not closed or renamed. The
//! index arithmetic ([`step_tab`], [`nth_tab`]) is display-free.

use std::rc::Rc;

use gtk4::gio;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::tab::{current_page, request_close_page};
use crate::ui::{host_picker, tab_label};

/// How many `goto_tab_N` actions there are.
pub const GOTO_TAB_COUNT: usize = 9;

/// The tab one step from `current` among `tabs` (page numbers in tab-bar
/// order), wrapping round; the first or last tab when `current` is not
/// one of them, `None` with no tabs.
pub fn step_tab(tabs: &[u32], current: Option<u32>, forward: bool) -> Option<u32> {
    let at = current.and_then(|current| tabs.iter().position(|&tab| tab == current));
    let next = match (at, forward) {
        (Some(at), true) => (at + 1) % tabs.len(),
        (Some(at), false) => (at + tabs.len() - 1) % tabs.len(),
        (None, true) => 0,
        (None, false) => tabs.len().checked_sub(1)?,
    };
    tabs.get(next).copied()
}

/// The `n`th tab (1-based), if there are that many.
pub fn nth_tab(tabs: &[u32], n: usize) -> Option<u32> {
    tabs.get(n.checked_sub(1)?).copied()
}

/// Register the actions on `window`; `home` is the notebook's Home page.
pub fn install(
    window: &gtk4::ApplicationWindow,
    notebook: &gtk4::Notebook,
    home: &gtk4::Widget,
    store: &Rc<SessionStore>,
) {
    let tabs = Tabs {
        notebook: notebook.clone(),
        home: home.clone(),
    };
    let add = |id: &str, run: Box<dyn Fn(&Tabs)>| {
        let action = gio::SimpleAction::new(id, None);
        let tabs = tabs.clone();
        action.connect_activate(move |_, _| run(&tabs));
        window.add_action(&action);
    };

    let store = store.clone();
    add(
        "new_tab",
        Box::new(move |tabs| host_picker::show(&tabs.notebook, &store)),
    );
    add(
        "close_tab",
        Box::new(|tabs| {
            if let Some(page) = tabs.current() {
                request_close_page(&tabs.notebook, &page);
            }
        }),
    );
    add("next_tab", Box::new(|tabs| tabs.step(true)));
    add("prev_tab", Box::new(|tabs| tabs.step(false)));
    for n in 1..=GOTO_TAB_COUNT {
        add(
            &format!("goto_tab_{n}"),
            Box::new(move |tabs| tabs.show(nth_tab(&tabs.page_numbers(), n))),
        );
    }
    add(
        "rename_tab",
        Box::new(|tabs| {
            let label = tabs
                .current()
                .and_then(|page| tabs.notebook.tab_label(&page));
            if let Some(label) = label {
                tab_label::prompt_rename(&label);
            }
        }),
    );
}

#[derive(Clone)]
struct Tabs {
    notebook: gtk4::Notebook,
    home: gtk4::Widget,
}

impl Tabs {
    /// Page numbers of the tabs, Home left out.
    fn page_numbers(&self) -> Vec<u32> {
        (0..self.notebook.n_pages())
            .filter(|&page_num| {
                self.notebook
                    .nth_page(Some(page_num))
                    .is_some_and(|page| page != self.home)
            })
            .collect()
    }

    /// The current page, unless it is Home.
    fn current(&self) -> Option<gtk4::Widget> {
        current_page(&self.notebook).filter(|page| *page != self.home)
    }

    fn step(&self, forward: bool) {
        let tabs = self.page_numbers();
        self.show(step_tab(&tabs, self.notebook.current_page(), forward));
    }

    fn show(&self, page_num: Option<u32>) {
        if let Some(page_num) = page_num {
            self.notebook.set_current_page(Some(page_num));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_wraps_round_both_ways() {
        let tabs = [1, 2, 3];
        assert_eq!(step_tab(&tabs, Some(1), true), Some(2));
        assert_eq!(step_tab(&tabs, Some(3), true), Some(1));
        assert_eq!(step_tab(&tabs, Some(1), false), Some(3));
        assert_eq!(step_tab(&tabs, Some(2), false), Some(1));
    }

    #[test]
    fn stepping_from_outside_the_tabs_lands_on_an_end() {
        // Home (page 0) is current while it is hidden only for a moment.
        let tabs = [1, 2];
        assert_eq!(step_tab(&tabs, Some(0), true), Some(1));
        assert_eq!(step_tab(&tabs, Some(0), false), Some(2));
        assert_eq!(step_tab(&tabs, None, true), Some(1));
    }

    #[test]
    fn stepping_with_one_or_no_tab() {
        assert_eq!(step_tab(&[4], Some(4), true), Some(4));
        assert_eq!(step_tab(&[4], Some(4), false), Some(4));
        assert_eq!(step_tab(&[], None, true), None);
        assert_eq!(step_tab(&[], Some(0), false), None);
    }

    #[test]
    fn nth_tab_counts_from_one() {
        let tabs = [1, 2, 3];
        assert_eq!(nth_tab(&tabs, 1), Some(1));
        assert_eq!(nth_tab(&tabs, 3), Some(3));
        assert_eq!(nth_tab(&tabs, 4), None);
        assert_eq!(nth_tab(&tabs, 0), None);
    }
}
