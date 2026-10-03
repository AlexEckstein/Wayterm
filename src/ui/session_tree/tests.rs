//! Unit tests for the display-free data layer of [`super::SessionTree`].
//!
//! These tests avoid GTK initialization and exercise only
//! [`collect_rows`] and [`icon_for_session_type`].

use crate::store::model::{SessionConfig, SessionType};
use crate::store::SessionStore;

use super::{collect_rows, icon_for_session_type, SessionRow};

#[test]
fn collect_rows_empty_store() {
    let store = SessionStore::in_memory().unwrap();
    assert!(collect_rows(&store).unwrap().is_empty());
}

#[test]
fn collect_rows_hierarchy_and_order() {
    let store = SessionStore::in_memory().unwrap();

    let folder_a = store.insert_folder("A", None).unwrap();
    let folder_b = store.insert_folder("B", Some(folder_a)).unwrap();

    let s1 = SessionConfig {
        name: "s1".to_string(),
        session_type: SessionType::SSH,
        folder_id: Some(folder_a),
        ..Default::default()
    };
    let s1_id = store.insert_session(&s1).unwrap();

    let s2 = SessionConfig {
        name: "s2".to_string(),
        session_type: SessionType::VNC,
        ..Default::default()
    };
    let s2_id = store.insert_session(&s2).unwrap();

    let rows = collect_rows(&store).unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, [folder_a, folder_b, s1_id, s2_id]);

    assert_eq!(
        rows,
        vec![
            SessionRow {
                id: folder_a,
                is_folder: true,
                name: "A".to_string(),
                icon: "📁",
                parent_id: None,
            },
            SessionRow {
                id: folder_b,
                is_folder: true,
                name: "B".to_string(),
                icon: "📁",
                parent_id: Some(folder_a),
            },
            SessionRow {
                id: s1_id,
                is_folder: false,
                name: "s1".to_string(),
                icon: "🖥",
                parent_id: Some(folder_a),
            },
            SessionRow {
                id: s2_id,
                is_folder: false,
                name: "s2".to_string(),
                icon: "📺",
                parent_id: None,
            },
        ]
    );
}

#[test]
fn icon_mapping_for_all_session_types() {
    assert_eq!(icon_for_session_type(&SessionType::SSH), "🖥");
    assert_eq!(icon_for_session_type(&SessionType::RDP), "🖼");
    assert_eq!(icon_for_session_type(&SessionType::VNC), "📺");
    assert_eq!(icon_for_session_type(&SessionType::Serial), "🔌");
    assert_eq!(icon_for_session_type(&SessionType::LocalShell), "🏠");
}

#[test]
fn collect_rows_sorts_folders_then_sessions_at_each_level() {
    let store = SessionStore::in_memory().unwrap();

    let root_session = store
        .insert_session(&SessionConfig {
            name: "root-session".to_string(),
            session_type: SessionType::SSH,
            ..Default::default()
        })
        .unwrap();
    let folder_z = store.insert_folder("Z", None).unwrap();
    let folder_a = store.insert_folder("A", None).unwrap();
    let child_session = store
        .insert_session(&SessionConfig {
            name: "child-session".to_string(),
            session_type: SessionType::LocalShell,
            folder_id: Some(folder_a),
            ..Default::default()
        })
        .unwrap();
    let child_folder = store.insert_folder("Child", Some(folder_a)).unwrap();

    let rows = collect_rows(&store).unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

    assert_eq!(
        ids,
        [
            folder_a,
            child_folder,
            child_session,
            folder_z,
            root_session
        ]
    );

    let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names, ["A", "Child", "child-session", "Z", "root-session",]);
}
