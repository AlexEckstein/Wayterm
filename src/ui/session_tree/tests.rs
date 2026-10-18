//! The display-free half of the session tree: row collection and icons.

use crate::store::model::{SessionConfig, SessionType};
use crate::store::SessionStore;

use super::{cloned_session, collect_rows, SessionRow};

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
    store
        .update_folder(folder_b, "B", Some("network-server-symbolic"))
        .unwrap();

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
        icon: Some("wayterm-homelab".to_string()),
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
                icon: None,
                default_icon: "wayterm-folder-symbolic",
                parent_id: None,
            },
            SessionRow {
                id: folder_b,
                is_folder: true,
                name: "B".to_string(),
                icon: Some("network-server-symbolic".to_string()),
                default_icon: "wayterm-folder-symbolic",
                parent_id: Some(folder_a),
            },
            SessionRow {
                id: s1_id,
                is_folder: false,
                name: "s1".to_string(),
                icon: None,
                default_icon: "wayterm-server-symbolic",
                parent_id: Some(folder_a),
            },
            SessionRow {
                id: s2_id,
                is_folder: false,
                name: "s2".to_string(),
                icon: Some("wayterm-homelab".to_string()),
                default_icon: "wayterm-remote-desktop-symbolic",
                parent_id: None,
            },
        ]
    );
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

/// A session saved through one connection is there for the next launch.
#[test]
fn session_persists_across_fresh_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("wayterm.db");

    let store_a = SessionStore::open(&db_path).unwrap();
    let folder_id = store_a.insert_folder("Work", None).unwrap();
    let session = SessionConfig {
        name: "prod-db".to_string(),
        session_type: SessionType::SSH,
        folder_id: Some(folder_id),
        host: Some("db.prod.lan".to_string()),
        port: Some(22),
        username: Some("admin".to_string()),
        auth_method: Some(crate::store::model::AuthMethod::Key),
        ..Default::default()
    };
    let id = store_a.insert_session(&session).unwrap();

    let store_b = SessionStore::open(&db_path).unwrap();
    let loaded = store_b.get_session(id).unwrap();
    assert_eq!(loaded.name, "prod-db");
    assert_eq!(loaded.session_type, SessionType::SSH);
    assert_eq!(loaded.folder_id, Some(folder_id));
    assert_eq!(loaded.host, Some("db.prod.lan".to_string()));
    assert_eq!(
        loaded.auth_method,
        Some(crate::store::model::AuthMethod::Key)
    );
}

#[test]
fn cloned_session_is_a_new_copy_in_the_same_folder() {
    let store = SessionStore::in_memory().unwrap();
    let folder = store.insert_folder("F", None).unwrap();
    let original = SessionConfig {
        name: "web".to_string(),
        session_type: SessionType::SSH,
        folder_id: Some(folder),
        host: Some("example.test".to_string()),
        port: Some(2222),
        username: Some("admin".to_string()),
        ..Default::default()
    };
    let original_id = store.insert_session(&original).unwrap();
    store.touch_connected(original_id).unwrap();

    let source = store.get_session(original_id).unwrap();
    let clone_id = store.insert_session(&cloned_session(&source)).unwrap();
    assert_ne!(clone_id, original_id);

    let clone = store.get_session(clone_id).unwrap();
    assert_eq!(clone.name, "web (copy)");
    assert_eq!(clone.folder_id, Some(folder));
    assert_eq!(clone.host.as_deref(), Some("example.test"));
    assert_eq!(clone.port, Some(2222));
    assert_eq!(clone.username.as_deref(), Some("admin"));
    assert_eq!(clone.connect_count, 0);
    assert_eq!(clone.last_connected, None);
    // The original is untouched.
    assert_eq!(store.get_session(original_id).unwrap().name, "web");
}
