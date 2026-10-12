//! The display-free half of the side dock: its panels and `toggle_sftp`.

use std::collections::HashSet;

use crate::ui::icons::{BUILTIN, CHROME};

use super::*;

#[test]
fn the_strip_runs_sessions_macros_sftp() {
    assert_eq!(
        DockPage::ALL,
        [DockPage::Sessions, DockPage::Macros, DockPage::Sftp]
    );
}

#[test]
fn every_panel_has_its_own_id_label_and_icon() {
    for field in [DockPage::id, DockPage::label, DockPage::icon] {
        let distinct: HashSet<&str> = DockPage::ALL.into_iter().map(field).collect();
        assert_eq!(distinct.len(), DockPage::ALL.len());
    }
    for page in DockPage::ALL {
        assert!(!page.label().is_empty(), "{page:?}: the tooltip names it");
    }
}

/// The tabs carry no text, so a missing icon would leave a blank button:
/// every icon ships in the binary.
#[test]
fn every_tab_icon_is_bundled() {
    for page in DockPage::ALL {
        let icon = page.icon();
        assert!(
            BUILTIN.contains(&icon) || CHROME.contains(&icon),
            "{page:?}: {icon} is bundled"
        );
    }
}

#[test]
fn toggle_sftp_goes_to_sftp_and_back() {
    use DockPage::*;
    assert_eq!(toggle_target(Sessions, Sessions), Sftp);
    assert_eq!(toggle_target(Macros, Sessions), Sftp);
    assert_eq!(toggle_target(Sftp, Sessions), Sessions);
    assert_eq!(toggle_target(Sftp, Macros), Macros);
}
