//! Unit tests for the display-free mapping logic of [`super::SessionDialog`].
//!
//! GTK dialog construction needs a display, so these tests exercise only
//! [`config_from_fields`] and the [`DialogFields`] mapping.

use crate::store::model::{AuthMethod, Folder, SessionType};

use super::{config_from_fields, DialogFields};

fn sample_folders() -> Vec<Folder> {
    vec![
        Folder {
            id: 1,
            name: "Work".to_string(),
            parent_id: None,
            sort_order: 0,
            icon: None,
        },
        Folder {
            id: 2,
            name: "Home".to_string(),
            parent_id: None,
            sort_order: 1,
            icon: None,
        },
    ]
}

#[test]
fn type_index_maps_to_session_type() {
    let folders = sample_folders();
    for (index, expected) in [
        (0, SessionType::SSH),
        (1, SessionType::RDP),
        (2, SessionType::VNC),
        (3, SessionType::Serial),
        (4, SessionType::LocalShell),
    ] {
        let fields = DialogFields {
            type_index: index,
            ..Default::default()
        };
        let config = config_from_fields(&fields, &folders);
        assert_eq!(
            config.session_type, expected,
            "type_index {index} should map to {expected:?}"
        );
    }
}

#[test]
fn auth_index_maps_to_auth_method() {
    let folders = sample_folders();
    for (index, expected) in [
        (0, AuthMethod::Password),
        (1, AuthMethod::Key),
        (2, AuthMethod::Agent),
        (3, AuthMethod::KeyboardInteractive),
        (4, AuthMethod::None),
    ] {
        let fields = DialogFields {
            auth_index: index,
            ..Default::default()
        };
        let config = config_from_fields(&fields, &folders);
        assert_eq!(
            config.auth_method,
            Some(expected.clone()),
            "auth_index {index} should map to {expected:?}"
        );
    }
}

#[test]
fn edit_mode_preserves_identity_fields() {
    let folders = sample_folders();
    let fields = DialogFields {
        id: 42,
        folder_id: Some(99),
        last_connected: Some("2026-10-02T12:34:56Z".to_string()),
        connect_count: 7,
        name: "updated".to_string(),
        host: "host.example".to_string(),
        port: 2222,
        username: "alice".to_string(),
        type_index: 0,
        folder_index: 1,
        auth_index: 1,
        notes: "note".to_string(),
    };

    let config = config_from_fields(&fields, &folders);

    assert_eq!(config.id, 42);
    assert_eq!(
        config.last_connected,
        Some("2026-10-02T12:34:56Z".to_string())
    );
    assert_eq!(config.connect_count, 7);
    // folder_id from DialogFields is ignored; folder_index resolves it.
    assert_eq!(config.folder_id, Some(1));
}

#[test]
fn port_zero_falls_back_to_twenty_two() {
    let folders = sample_folders();
    let fields = DialogFields {
        port: 0,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders);
    assert_eq!(config.port, Some(22));
}

#[test]
fn port_too_large_falls_back_to_twenty_two() {
    let folders = sample_folders();
    let fields = DialogFields {
        port: 70_000,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders);
    assert_eq!(config.port, Some(22));
}

#[test]
fn notes_round_trip() {
    let folders = sample_folders();
    let notes = "Line one\nLine two\n  padded  ".to_string();
    let fields = DialogFields {
        notes: notes.clone(),
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders);
    assert_eq!(config.notes, Some(notes.trim().to_string()));
}

#[test]
fn empty_optional_strings_become_none() {
    let folders = sample_folders();
    let fields = DialogFields {
        name: "   ".to_string(),
        host: "   ".to_string(),
        username: "   ".to_string(),
        notes: "   ".to_string(),
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders);
    assert_eq!(config.name, "");
    assert_eq!(config.host, None);
    assert_eq!(config.username, None);
    assert_eq!(config.notes, None);
}

#[test]
fn folder_index_zero_is_root() {
    let folders = sample_folders();
    let fields = DialogFields {
        folder_index: 0,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders);
    assert_eq!(config.folder_id, None);
}

#[test]
fn folder_index_resolves_to_folder_id() {
    let folders = sample_folders();
    let fields = DialogFields {
        folder_index: 2,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders);
    assert_eq!(config.folder_id, Some(2));
}
