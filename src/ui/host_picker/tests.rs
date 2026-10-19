//! The display-free half of the host picker: which hosts it offers, in
//! which order, under which folder headings.

use crate::store::model::{SessionConfig, SessionType};
use crate::store::SessionStore;
use crate::ui::session_tree::collect_rows;

use super::{local_shell_session, picker_entries, PickerEntry, LOCAL_SHELL_ID};

fn session(store: &SessionStore, name: &str, folder: Option<i64>) -> i64 {
    store
        .insert_session(&SessionConfig {
            name: name.into(),
            folder_id: folder,
            ..Default::default()
        })
        .unwrap()
}

fn entries(store: &SessionStore) -> Vec<PickerEntry> {
    picker_entries(&collect_rows(store).unwrap())
}

/// `(name, depth, headings as "name@depth")` per entry.
fn shape(entries: &[PickerEntry]) -> Vec<(String, usize, Vec<String>)> {
    entries
        .iter()
        .map(|entry| {
            let headings = entry
                .headings
                .iter()
                .map(|heading| format!("{}@{}", heading.name, heading.depth))
                .collect();
            (entry.name.clone(), entry.depth, headings)
        })
        .collect()
}

#[test]
fn the_local_shell_comes_first_even_with_no_sessions() {
    let store = SessionStore::in_memory().unwrap();
    let entries = entries(&store);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, None);
    assert_eq!(entries[0].name, "Local shell");
    assert_eq!(entries[0].depth, 0);
    assert!(entries[0].headings.is_empty());
}

#[test]
fn sessions_follow_in_tree_order_under_their_folders() {
    let store = SessionStore::in_memory().unwrap();
    let prod = store.insert_folder("Prod", None).unwrap();
    let web = store.insert_folder("Web", Some(prod)).unwrap();
    let w1 = session(&store, "w1", Some(web));
    session(&store, "p1", Some(prod));
    session(&store, "root", None);

    let entries = entries(&store);
    assert_eq!(
        shape(&entries),
        vec![
            ("Local shell".into(), 0, vec![]),
            ("w1".into(), 2, vec!["Prod@0".into(), "Web@1".into()]),
            // Prod's heading already opened above w1.
            ("p1".into(), 1, vec![]),
            ("root".into(), 0, vec![]),
        ]
    );
    assert_eq!(entries[1].session_id, Some(w1));
}

#[test]
fn folders_with_no_session_below_are_left_out() {
    let store = SessionStore::in_memory().unwrap();
    let a = store.insert_folder("A", None).unwrap();
    store.insert_folder("Empty", Some(a)).unwrap();
    store.insert_folder("Also empty", None).unwrap();
    session(&store, "a1", Some(a));

    assert_eq!(
        shape(&entries(&store)),
        vec![
            ("Local shell".into(), 0, vec![]),
            ("a1".into(), 1, vec!["A@0".into()]),
        ]
    );
}

#[test]
fn entries_carry_their_icons() {
    let store = SessionStore::in_memory().unwrap();
    store
        .insert_session(&SessionConfig {
            name: "serial".into(),
            session_type: SessionType::Serial,
            icon: Some("wayterm-router-symbolic".into()),
            ..Default::default()
        })
        .unwrap();
    let entries = entries(&store);
    assert_eq!(entries[0].default_icon, "wayterm-terminal-symbolic");
    assert_eq!(entries[1].default_icon, "wayterm-serial-symbolic");
    assert_eq!(entries[1].icon.as_deref(), Some("wayterm-router-symbolic"));
}

#[test]
fn the_local_shell_session_is_an_unsaved_login_shell() {
    let config = local_shell_session();
    assert_eq!(config.id, LOCAL_SHELL_ID);
    assert_eq!(config.session_type, SessionType::LocalShell);
    assert_eq!(config.local_shell, None, "None runs $SHELL");
    assert!(config.local_working_dir.is_some_and(|dir| !dir.is_empty()));
}
