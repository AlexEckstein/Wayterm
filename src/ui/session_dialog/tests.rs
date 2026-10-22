//! The dialog's display-free mapping: [`config_from_fields`] and the
//! per-type helpers.

use crate::store::model::{
    AuthMethod, Folder, SessionConfig, SessionType, RDP_COLOR_DEPTHS, SERIAL_DATA_BITS,
    SERIAL_FLOW_CONTROLS, SERIAL_PARITIES, SERIAL_STOP_BITS,
};
use crate::store::SessionStore;
use crate::ui::tunnels_dialog::model::ForwardRuleDraft;

use super::{
    collect_folders, collect_peer_sessions, config_from_fields, default_tmux_session_name,
    folder_labels, format_resolution, jump_candidates, jump_error, jump_missing_label,
    jump_picker_model, parse_resolution, resolve_jump, session_type_from_index, type_index_for,
    DialogFields, DEFAULT_SERIAL_BAUD, JUMP_NONE_LABEL, TYPE_INDEX_LOCAL, TYPE_INDEX_RDP,
    TYPE_INDEX_SERIAL, TYPE_INDEX_SSH, TYPE_INDEX_VNC,
};

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

/// Every per-type widget populated, so a test can prove that a hidden group
/// never leaks into the stored row.
fn all_per_type_fields_filled() -> DialogFields {
    DialogFields {
        serial_device: "/dev/ttyUSB0".to_string(),
        serial_baud: 9600,
        serial_data_bits_index: 2,
        serial_parity_index: 2,
        serial_stop_bits_index: 1,
        serial_flow_control_index: 2,
        rdp_domain: "CORP".to_string(),
        rdp_resolution: "1920x1080".to_string(),
        rdp_color_depth_index: 1,
        local_shell: "/bin/zsh".to_string(),
        local_working_dir: "/srv".to_string(),
        ..Default::default()
    }
}

/// Assert that no per-type group left a payload behind.
fn assert_no_per_type_payload(config: &crate::store::model::SessionConfig) {
    assert_eq!(config.serial_device, None);
    assert_eq!(config.serial_baud_rate, None);
    assert_eq!(config.serial_data_bits, None);
    assert_eq!(config.serial_parity, None);
    assert_eq!(config.serial_stop_bits, None);
    assert_eq!(config.serial_flow_control, None);
    assert_eq!(config.rdp_domain, None);
    assert_eq!(config.rdp_width, None);
    assert_eq!(config.rdp_height, None);
    assert_eq!(config.rdp_color_depth, None);
    assert!(!config.rdp_compression);
    assert_eq!(config.rdp_options, None);
    assert_eq!(config.vnc_color_depth, None);
    assert_eq!(config.vnc_options, None);
    assert_eq!(config.local_shell, None);
    assert_eq!(config.local_working_dir, None);
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
        let config = config_from_fields(&fields, &folders, &[]);
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
        let config = config_from_fields(&fields, &folders, &[]);
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
        base: SessionConfig {
            id: 42,
            last_connected: Some("2026-10-02T12:34:56Z".to_string()),
            connect_count: 7,
            ..Default::default()
        },
        name: "updated".to_string(),
        host: "host.example".to_string(),
        port: 2222,
        username: "alice".to_string(),
        type_index: 0,
        folder_index: 1,
        auth_index: 1,
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.id, 42);
    assert_eq!(
        config.last_connected,
        Some("2026-10-02T12:34:56Z".to_string())
    );
    assert_eq!(config.connect_count, 7);
    assert_eq!(config.folder_id, Some(1));
}

/// A row as an import leaves it: columns the dialog has no widget for.
fn row_with_hidden_columns(session_type: SessionType) -> SessionConfig {
    use crate::store::model::{HostKeyPolicy, RdpOptions, SshOptions, VncOptions};
    SessionConfig {
        id: 9,
        session_type,
        key_path: Some("/home/u/.ssh/id_ed25519".to_string()),
        host_key_policy: Some(HostKeyPolicy::AcceptNew),
        ssh_options: Some(SshOptions {
            compression: true,
            keepalive_interval: Some(30),
            keepalive_count: None,
            ssh_w_fallback: true,
        }),
        rdp_compression: false,
        rdp_options: Some(RdpOptions {
            audio: true,
            clipboard: false,
            fonts: true,
            printer: false,
        }),
        vnc_color_depth: Some(16),
        vnc_options: Some(VncOptions {
            quality: 3,
            scaling: false,
            view_only: true,
        }),
        preferred_backend: Some("vte".to_string()),
        color_scheme: Some("Solarized".to_string()),
        font_family: Some("Fira Code".to_string()),
        font_size: Some(13.0),
        opacity: Some(0.9),
        ..Default::default()
    }
}

/// Saving an edit keeps what the dialog cannot show: the appearance
/// overrides always, the per-type columns while the type stays.
#[test]
fn editing_keeps_the_columns_the_dialog_does_not_show() {
    for (session_type, type_index) in [
        (SessionType::SSH, TYPE_INDEX_SSH),
        (SessionType::RDP, TYPE_INDEX_RDP),
        (SessionType::VNC, TYPE_INDEX_VNC),
    ] {
        let base = row_with_hidden_columns(session_type.clone());
        let fields = DialogFields {
            type_index,
            base: base.clone(),
            ..Default::default()
        };
        let config = config_from_fields(&fields, &[], &[]);
        assert_eq!(config.id, 9);
        assert_eq!(config.font_family, base.font_family, "{session_type:?}");
        assert_eq!(config.font_size, base.font_size);
        assert_eq!(config.opacity, base.opacity);
        assert_eq!(config.color_scheme, base.color_scheme);
        assert_eq!(config.preferred_backend, base.preferred_backend);
        let ssh = session_type == SessionType::SSH;
        assert_eq!(config.key_path.is_some(), ssh, "{session_type:?}");
        assert_eq!(config.host_key_policy.is_some(), ssh);
        assert_eq!(config.ssh_options.is_some(), ssh);
        let rdp = session_type == SessionType::RDP;
        assert_eq!(config.rdp_options.is_some(), rdp, "{session_type:?}");
        let vnc = session_type == SessionType::VNC;
        assert_eq!(config.vnc_options.is_some(), vnc, "{session_type:?}");
        assert_eq!(config.vnc_color_depth.is_some(), vnc);
    }
    let rdp = DialogFields {
        type_index: TYPE_INDEX_RDP,
        base: row_with_hidden_columns(SessionType::RDP),
        ..Default::default()
    };
    assert!(
        !config_from_fields(&rdp, &[], &[]).rdp_compression,
        "a stored compression choice survives"
    );
}

/// A type change starts the new type's hidden columns fresh: the old
/// type's never leak into it.
#[test]
fn a_type_change_drops_the_old_types_hidden_columns() {
    let fields = DialogFields {
        type_index: TYPE_INDEX_RDP,
        base: row_with_hidden_columns(SessionType::SSH),
        ..Default::default()
    };
    let config = config_from_fields(&fields, &[], &[]);
    assert_eq!(config.session_type, SessionType::RDP);
    assert_eq!(config.key_path, None);
    assert_eq!(config.host_key_policy, None);
    assert_eq!(config.ssh_options, None);
    assert_eq!(config.rdp_options, None);
    assert!(config.rdp_compression, "a new RDP row compresses (doc 09)");
    assert_eq!(config.font_family.as_deref(), Some("Fira Code"));
}

#[test]
fn port_zero_falls_back_to_twenty_two() {
    let folders = sample_folders();
    let fields = DialogFields {
        port: 0,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders, &[]);
    assert_eq!(config.port, Some(22));
}

#[test]
fn port_too_large_falls_back_to_twenty_two() {
    let folders = sample_folders();
    let fields = DialogFields {
        port: 70_000,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders, &[]);
    assert_eq!(config.port, Some(22));
}

#[test]
fn icon_is_trimmed_and_blank_means_default() {
    let chosen = DialogFields {
        icon: " wayterm-homelab ".to_string(),
        ..DialogFields::default()
    };
    assert_eq!(
        config_from_fields(&chosen, &[], &[]).icon.as_deref(),
        Some("wayterm-homelab")
    );
    let blank = DialogFields {
        icon: "  ".to_string(),
        ..DialogFields::default()
    };
    assert_eq!(config_from_fields(&blank, &[], &[]).icon, None);
}

#[test]
fn empty_optional_strings_become_none() {
    let folders = sample_folders();
    let fields = DialogFields {
        name: "   ".to_string(),
        host: "   ".to_string(),
        username: "   ".to_string(),
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders, &[]);
    assert_eq!(config.name, "");
    assert_eq!(config.host, None);
    assert_eq!(config.username, None);
}

#[test]
fn folder_index_zero_is_root() {
    let folders = sample_folders();
    let fields = DialogFields {
        folder_index: 0,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders, &[]);
    assert_eq!(config.folder_id, None);
}

#[test]
fn folder_index_resolves_to_folder_id() {
    let folders = sample_folders();
    let fields = DialogFields {
        folder_index: 2,
        ..Default::default()
    };
    let config = config_from_fields(&fields, &folders, &[]);
    assert_eq!(config.folder_id, Some(2));
}

// ---------------------------------------------------------------------------
// Type DropDown ⇄ SessionType
// ---------------------------------------------------------------------------

#[test]
fn type_index_round_trips_for_every_type() {
    for session_type in [
        SessionType::SSH,
        SessionType::RDP,
        SessionType::VNC,
        SessionType::Serial,
        SessionType::LocalShell,
    ] {
        assert_eq!(
            session_type_from_index(type_index_for(&session_type)),
            session_type
        );
    }

    assert_eq!(type_index_for(&SessionType::SSH), TYPE_INDEX_SSH);
    assert_eq!(type_index_for(&SessionType::RDP), TYPE_INDEX_RDP);
    assert_eq!(type_index_for(&SessionType::VNC), TYPE_INDEX_VNC);
    assert_eq!(type_index_for(&SessionType::Serial), TYPE_INDEX_SERIAL);
    assert_eq!(type_index_for(&SessionType::LocalShell), TYPE_INDEX_LOCAL);
}

#[test]
fn out_of_range_type_index_falls_back_to_ssh() {
    assert_eq!(session_type_from_index(99), SessionType::SSH);
    assert_eq!(session_type_from_index(5), SessionType::SSH);
}

// ---------------------------------------------------------------------------
// Per-type port defaults
// ---------------------------------------------------------------------------

#[test]
fn invalid_port_falls_back_to_the_types_default() {
    let folders = sample_folders();
    for (type_index, expected) in [
        (TYPE_INDEX_SSH, Some(22)),
        (TYPE_INDEX_RDP, Some(3389)),
        (TYPE_INDEX_VNC, Some(5900)),
        (TYPE_INDEX_SERIAL, None),
        (TYPE_INDEX_LOCAL, None),
    ] {
        for port in [0, 70_000] {
            let fields = DialogFields {
                type_index,
                port,
                ..Default::default()
            };
            let config = config_from_fields(&fields, &folders, &[]);
            assert_eq!(
                config.port, expected,
                "type_index {type_index} with port {port}"
            );
        }
    }
}

#[test]
fn valid_port_is_kept_for_network_types() {
    let folders = sample_folders();
    for (type_index, port, expected) in [
        (TYPE_INDEX_SSH, 2222, Some(2222)),
        (TYPE_INDEX_RDP, 13389, Some(13389)),
        (TYPE_INDEX_VNC, 5901, Some(5901)),
    ] {
        let fields = DialogFields {
            type_index,
            port,
            ..all_per_type_fields_filled()
        };
        let config = config_from_fields(&fields, &folders, &[]);
        assert_eq!(config.port, expected, "type_index {type_index}");
    }
}

#[test]
fn typed_port_is_dropped_for_serial_and_local() {
    let folders = sample_folders();
    // A stale port from a previously selected type must not be stored.
    for type_index in [TYPE_INDEX_SERIAL, TYPE_INDEX_LOCAL] {
        let fields = DialogFields {
            type_index,
            port: 2222,
            ..Default::default()
        };
        assert_eq!(config_from_fields(&fields, &folders, &[]).port, None);
    }
}

// ---------------------------------------------------------------------------
// Serial group (doc 09 `serial_*` columns)
// ---------------------------------------------------------------------------

#[test]
fn serial_fields_map_to_serial_columns() {
    let folders = sample_folders();
    let fields = DialogFields {
        type_index: TYPE_INDEX_SERIAL,
        name: "console".to_string(),
        serial_device: " /dev/ttyUSB0 ".to_string(),
        serial_baud: 9600,
        serial_data_bits_index: 0,    // 5
        serial_parity_index: 3,       // mark
        serial_stop_bits_index: 1,    // 2
        serial_flow_control_index: 1, // hardware
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.session_type, SessionType::Serial);
    assert_eq!(config.serial_device.as_deref(), Some("/dev/ttyUSB0"));
    assert_eq!(config.serial_baud_rate, Some(9600));
    assert_eq!(config.serial_data_bits, Some(5));
    assert_eq!(config.serial_parity.as_deref(), Some("mark"));
    assert_eq!(config.serial_stop_bits, Some(2));
    assert_eq!(config.serial_flow_control.as_deref(), Some("hardware"));
}

#[test]
fn serial_defaults_match_doc_06_column_defaults() {
    let folders = sample_folders();
    // An untouched serial group still writes the doc-06 defaults
    // (8 / 'none' / 1 / 'none') rather than NULLs.
    let fields = DialogFields {
        type_index: TYPE_INDEX_SERIAL,
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.serial_device, None);
    assert_eq!(config.serial_baud_rate, Some(DEFAULT_SERIAL_BAUD));
    assert_eq!(config.serial_data_bits, Some(8));
    assert_eq!(config.serial_parity.as_deref(), Some("none"));
    assert_eq!(config.serial_stop_bits, Some(1));
    assert_eq!(config.serial_flow_control.as_deref(), Some("none"));
}

#[test]
fn zero_baud_falls_back_to_the_dialog_default() {
    let folders = sample_folders();
    let fields = DialogFields {
        type_index: TYPE_INDEX_SERIAL,
        serial_baud: 0,
        ..Default::default()
    };
    assert_eq!(
        config_from_fields(&fields, &folders, &[]).serial_baud_rate,
        Some(DEFAULT_SERIAL_BAUD)
    );
}

#[test]
fn every_serial_dropdown_index_maps_to_a_documented_value() {
    let folders = sample_folders();
    for (index, expected) in SERIAL_DATA_BITS.iter().enumerate() {
        let fields = DialogFields {
            type_index: TYPE_INDEX_SERIAL,
            serial_data_bits_index: index as u32,
            ..Default::default()
        };
        assert_eq!(
            config_from_fields(&fields, &folders, &[]).serial_data_bits,
            Some(*expected)
        );
    }
    for (index, expected) in SERIAL_PARITIES.iter().enumerate() {
        let fields = DialogFields {
            type_index: TYPE_INDEX_SERIAL,
            serial_parity_index: index as u32,
            ..Default::default()
        };
        assert_eq!(
            config_from_fields(&fields, &folders, &[])
                .serial_parity
                .as_deref(),
            Some(*expected)
        );
    }
    for (index, expected) in SERIAL_STOP_BITS.iter().enumerate() {
        let fields = DialogFields {
            type_index: TYPE_INDEX_SERIAL,
            serial_stop_bits_index: index as u32,
            ..Default::default()
        };
        assert_eq!(
            config_from_fields(&fields, &folders, &[]).serial_stop_bits,
            Some(*expected)
        );
    }
    for (index, expected) in SERIAL_FLOW_CONTROLS.iter().enumerate() {
        let fields = DialogFields {
            type_index: TYPE_INDEX_SERIAL,
            serial_flow_control_index: index as u32,
            ..Default::default()
        };
        assert_eq!(
            config_from_fields(&fields, &folders, &[])
                .serial_flow_control
                .as_deref(),
            Some(*expected)
        );
    }
}

// ---------------------------------------------------------------------------
// RDP group (doc 09 `rdp_*` columns)
// ---------------------------------------------------------------------------

#[test]
fn rdp_fields_map_to_rdp_columns() {
    let folders = sample_folders();
    let fields = DialogFields {
        type_index: TYPE_INDEX_RDP,
        name: "win".to_string(),
        host: "10.0.0.5".to_string(),
        rdp_domain: " CORP ".to_string(),
        rdp_resolution: "1920x1080".to_string(),
        rdp_color_depth_index: 1, // 24
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.session_type, SessionType::RDP);
    assert_eq!(config.rdp_domain.as_deref(), Some("CORP"));
    assert_eq!(config.rdp_width, Some(1920));
    assert_eq!(config.rdp_height, Some(1080));
    assert_eq!(config.rdp_color_depth, Some(24));
    // Doc 09's schema default for rdp_compression is 1 (on) and the dialog
    // exposes no compression widget.
    assert!(config.rdp_compression);
}

#[test]
fn empty_rdp_resolution_leaves_doc_08_to_supply_the_fallback() {
    let folders = sample_folders();
    let fields = DialogFields {
        type_index: TYPE_INDEX_RDP,
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.rdp_width, None);
    assert_eq!(config.rdp_height, None);
    assert_eq!(config.rdp_domain, None);
    // Doc 08 §RDP: `/bpp:{rdp_color_depth.unwrap_or(32)}`.
    assert_eq!(config.rdp_color_depth, Some(32));
}

#[test]
fn every_rdp_color_depth_index_maps() {
    let folders = sample_folders();
    for (index, expected) in RDP_COLOR_DEPTHS.iter().enumerate() {
        let fields = DialogFields {
            type_index: TYPE_INDEX_RDP,
            rdp_color_depth_index: index as u32,
            ..Default::default()
        };
        assert_eq!(
            config_from_fields(&fields, &folders, &[]).rdp_color_depth,
            Some(*expected)
        );
    }
}

#[test]
fn resolution_parsing() {
    assert_eq!(parse_resolution("1920x1080"), Some((1920, 1080)));
    assert_eq!(parse_resolution("1024X768"), Some((1024, 768)));
    assert_eq!(parse_resolution("  800 x 600  "), Some((800, 600)));
    // Empty / malformed leaves the columns NULL for doc 08's 1024x768.
    assert_eq!(parse_resolution(""), None);
    assert_eq!(parse_resolution("   "), None);
    assert_eq!(parse_resolution("wide"), None);
    assert_eq!(parse_resolution("1920"), None);
    assert_eq!(parse_resolution("1920x"), None);
    assert_eq!(parse_resolution("0x1080"), None);
}

#[test]
fn resolution_formatting_round_trips() {
    assert_eq!(format_resolution(Some(1920), Some(1080)), "1920x1080");
    assert_eq!(format_resolution(None, None), "");
    assert_eq!(format_resolution(Some(1920), None), "");
    assert_eq!(
        parse_resolution(&format_resolution(Some(1280), Some(1024))),
        Some((1280, 1024))
    );
}

// ---------------------------------------------------------------------------
// Local shell group (doc 09 `local_*` columns)
// ---------------------------------------------------------------------------

#[test]
fn local_fields_map_to_local_columns() {
    let folders = sample_folders();
    let fields = DialogFields {
        type_index: TYPE_INDEX_LOCAL,
        name: "shell".to_string(),
        local_shell: " /bin/zsh ".to_string(),
        local_working_dir: " /srv/www ".to_string(),
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.session_type, SessionType::LocalShell);
    assert_eq!(config.local_shell.as_deref(), Some("/bin/zsh"));
    assert_eq!(config.local_working_dir.as_deref(), Some("/srv/www"));
    assert_eq!(config.port, None);
}

// ---------------------------------------------------------------------------
// VNC group — no secret field, ever
// ---------------------------------------------------------------------------

#[test]
fn vnc_session_stores_no_secret_and_no_dialog_payload() {
    let folders = sample_folders();
    // Every other group is filled in to prove none of it leaks into a VNC
    // row; the VNC password is a keyring secret (doc 05) and the dialog
    // has no field for it.
    let fields = DialogFields {
        type_index: TYPE_INDEX_VNC,
        name: "screen".to_string(),
        host: "10.0.0.9".to_string(),
        port: 0, // untouched port → falls back to VNC's 5900
        ..all_per_type_fields_filled()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.session_type, SessionType::VNC);
    assert_eq!(config.host.as_deref(), Some("10.0.0.9"));
    assert_eq!(config.port, Some(5900));
    assert_no_per_type_payload(&config);
}

// ---------------------------------------------------------------------------
// Hidden groups must not leak into the stored row
// ---------------------------------------------------------------------------

#[test]
fn hidden_groups_do_not_leak_into_other_types() {
    let folders = sample_folders();
    for (type_index, session_type) in [
        (TYPE_INDEX_SSH, SessionType::SSH),
        (TYPE_INDEX_RDP, SessionType::RDP),
        (TYPE_INDEX_VNC, SessionType::VNC),
        (TYPE_INDEX_LOCAL, SessionType::LocalShell),
    ] {
        let mut fields = all_per_type_fields_filled();
        fields.type_index = type_index;
        let config = config_from_fields(&fields, &folders, &[]);
        assert_eq!(config.session_type, session_type);

        // Each payload is written only for the type that owns it.
        assert_eq!(
            config.serial_device.is_some(),
            type_index == TYPE_INDEX_SERIAL,
            "serial payload for type_index {type_index}"
        );
        assert_eq!(
            config.rdp_domain.is_some(),
            type_index == TYPE_INDEX_RDP,
            "rdp payload for type_index {type_index}"
        );
        assert_eq!(
            config.local_shell.is_some(),
            type_index == TYPE_INDEX_LOCAL,
            "local payload for type_index {type_index}"
        );
    }
}

#[test]
fn ssh_session_with_filled_groups_stores_only_common_fields() {
    let folders = sample_folders();
    let fields = DialogFields {
        type_index: TYPE_INDEX_SSH,
        name: "box".to_string(),
        host: "host.example".to_string(),
        username: "alice".to_string(),
        ..all_per_type_fields_filled()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.session_type, SessionType::SSH);
    assert_eq!(config.port, Some(22));
    assert_no_per_type_payload(&config);
}

// ---------------------------------------------------------------------------
// tmux persistence (doc 04 §Tmux session persistence)
//
// The checkbox and the name Entry live in the SSH group, but doc 04 scopes
// the feature to SSH rather than making the columns SSH-only, so the flag is
// stored for every type and only the SSH transport reads it.
// ---------------------------------------------------------------------------

/// An SSH session with tmux enabled, named `name`.
fn tmux_fields(name: &str, session_name: &str) -> DialogFields {
    DialogFields {
        type_index: TYPE_INDEX_SSH,
        name: name.to_string(),
        host: "host.example".to_string(),
        tmux_enabled: true,
        tmux_session_name: session_name.to_string(),
        ..Default::default()
    }
}

#[test]
fn tmux_fields_round_trip_through_config_from_fields() {
    let folders = sample_folders();
    let config = config_from_fields(&tmux_fields("prod", "wayterm-prod"), &folders, &[]);

    assert!(config.tmux_enabled);
    // The stored column is the bare suffix: `SSHTransport` prepends
    // `wayterm-` itself, so the remote name is `wayterm-prod`.
    assert_eq!(config.tmux_session_name.as_deref(), Some("prod"));
    assert_eq!(config.tmux_session_suffix().as_deref(), Some("prod"));
    assert_eq!(config.tmux_remote_name().as_deref(), Some("wayterm-prod"));
}

#[test]
fn tmux_name_entered_without_the_prefix_is_normalised() {
    let folders = sample_folders();
    // A user who ignores the `wayterm-` convention and types the bare name
    // must not end up with `wayterm-wayterm-prod` on the remote host.
    let config = config_from_fields(&tmux_fields("prod", " prod "), &folders, &[]);

    assert_eq!(config.tmux_session_name.as_deref(), Some("prod"));
    assert_eq!(config.tmux_remote_name().as_deref(), Some("wayterm-prod"));
}

#[test]
fn tmux_names_with_shell_metacharacters_survive_verbatim() {
    let folders = sample_folders();
    // Doc 04's quoting caveat: `;` is legal in a tmux session name and
    // `SSHTransport::shell_quote` handles the command side. The dialog must
    // not "sanitise" the name away.
    let config = config_from_fields(&tmux_fields("odd", "wayterm-a;b"), &folders, &[]);

    assert_eq!(config.tmux_session_name.as_deref(), Some("a;b"));
    assert_eq!(config.tmux_remote_name().as_deref(), Some("wayterm-a;b"));
}

#[test]
fn empty_tmux_name_is_stored_as_null_and_derived_at_connect() {
    let folders = sample_folders();
    // An Entry cleared by hand means "use the session name", not a session
    // literally called `wayterm-`.
    let config = config_from_fields(&tmux_fields("prod", ""), &folders, &[]);

    assert!(config.tmux_enabled);
    assert_eq!(config.tmux_session_name, None);
    assert_eq!(config.tmux_session_suffix().as_deref(), Some("prod"));
    assert_eq!(config.tmux_remote_name().as_deref(), Some("wayterm-prod"));
    // …and the dialog shows what the remote will see, even before the box is
    // ticked.
    assert_eq!(config.tmux_name_for_dialog(), "wayterm-prod");
}

#[test]
fn whitespace_only_tmux_name_counts_as_unset() {
    let folders = sample_folders();
    let config = config_from_fields(&tmux_fields("prod", "   "), &folders, &[]);

    assert_eq!(config.tmux_session_name, None);
    assert_eq!(config.tmux_remote_name().as_deref(), Some("wayterm-prod"));
}

#[test]
fn derived_tmux_name_follows_name_field_edits() {
    let folders = sample_folders();
    // The GTK Entry re-derives on every Name keystroke until the user edits
    // the Entry; this is the display-free version of that rule.
    assert_eq!(default_tmux_session_name("prod"), "wayterm-prod");
    assert_eq!(default_tmux_session_name("  prod  "), "wayterm-prod");
    assert_eq!(default_tmux_session_name(" staging "), "wayterm-staging");
    assert_eq!(default_tmux_session_name(""), "wayterm-");

    // Each rename of the session re-derives the remote name, and the stored
    // row follows only because the Entry text is what gets stored.
    for name in ["prod", "prod2", "edge-1"] {
        let entry = default_tmux_session_name(name);
        let config = config_from_fields(&tmux_fields(name, &entry), &folders, &[]);
        assert_eq!(config.tmux_remote_name().as_deref(), Some(entry.as_str()));
    }
}

#[test]
fn explicit_tmux_name_overrides_the_name_derived_default() {
    let folders = sample_folders();
    // Once the user types their own name the Name field stops driving it.
    let config = config_from_fields(&tmux_fields("prod", "wayterm-shared-1"), &folders, &[]);

    assert_eq!(config.tmux_session_name.as_deref(), Some("shared-1"));
    assert_eq!(
        config.tmux_remote_name().as_deref(),
        Some("wayterm-shared-1")
    );
    assert_ne!(config.tmux_remote_name().as_deref(), Some("wayterm-prod"));
}

#[test]
fn tmux_unticked_stores_no_name_but_keeps_the_flag_off() {
    let folders = sample_folders();
    let fields = DialogFields {
        tmux_enabled: false,
        tmux_session_name: "wayterm-prod".to_string(),
        ..tmux_fields("prod", "wayterm-prod")
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert!(!config.tmux_enabled);
    // The name the dialog displays is still stored, so re-opening the dialog
    // shows what the user last saw; `tmux_session_suffix` gates on the flag.
    assert_eq!(config.tmux_session_name.as_deref(), Some("prod"));
    assert_eq!(config.tmux_session_suffix(), None);
    assert_eq!(config.tmux_remote_name(), None);
}

#[test]
fn tmux_flag_on_a_serial_session_is_stored_but_ignored() {
    let folders = sample_folders();
    // Doc 04 scopes tmux to SSH: a serial row may carry the flag (the
    // columns are common in doc 09 and the dialog keeps one checkbox), but
    // `tmux_session_suffix` — the only thing the SSH factory consults — gates
    // on the type, and no serial transport exists to read it anyway.
    let fields = DialogFields {
        type_index: TYPE_INDEX_SERIAL,
        name: "console".to_string(),
        serial_device: "/dev/ttyUSB0".to_string(),
        tmux_enabled: true,
        tmux_session_name: "wayterm-console".to_string(),
        ..Default::default()
    };

    let config = config_from_fields(&fields, &folders, &[]);

    assert_eq!(config.session_type, SessionType::Serial);
    assert!(config.tmux_enabled, "the flag is stored as typed");
    assert_eq!(config.tmux_session_name.as_deref(), Some("console"));
    assert_eq!(
        config.tmux_session_suffix(),
        Some("console".to_string()),
        "the suffix resolves, but nothing reads it: create_transport matches \
         on the type and only SSH exists"
    );
    // The type-owned payload is unaffected by the tmux group.
    assert_eq!(config.serial_device.as_deref(), Some("/dev/ttyUSB0"));
}

#[test]
fn tmux_name_for_dialog_prefers_the_stored_name_over_the_session_name() {
    let config = crate::store::model::SessionConfig {
        name: "prod".to_string(),
        tmux_enabled: true,
        tmux_session_name: Some("shared-1".to_string()),
        ..Default::default()
    };
    assert_eq!(config.tmux_name_for_dialog(), "wayterm-shared-1");

    // A legacy row that stored the full name (before the prefix was split
    // out) must not render as `wayterm-wayterm-x`.
    let legacy = crate::store::model::SessionConfig {
        name: "prod".to_string(),
        tmux_enabled: true,
        tmux_session_name: Some("wayterm-legacy".to_string()),
        ..Default::default()
    };
    assert_eq!(legacy.tmux_name_for_dialog(), "wayterm-legacy");
    assert_eq!(legacy.tmux_remote_name().as_deref(), Some("wayterm-legacy"));
}

// ---------------------------------------------------------------------------
// Jump hosts (doc 04, depth ≤ 1) + forward rules (doc 09 `forward_rules`)
// ---------------------------------------------------------------------------

fn ssh_peer(id: i64, name: &str, jump: Option<i64>) -> SessionConfig {
    SessionConfig {
        id,
        name: name.to_string(),
        session_type: SessionType::SSH,
        jump_session_id: jump,
        ..Default::default()
    }
}

fn sample_peers() -> Vec<SessionConfig> {
    vec![
        ssh_peer(1, "bastion", None),
        ssh_peer(2, "via-bastion", Some(1)),
        SessionConfig {
            id: 3,
            name: "win".to_string(),
            session_type: SessionType::RDP,
            ..Default::default()
        },
    ]
}

fn forward_draft(bind_port: &str, remote_port: &str) -> ForwardRuleDraft {
    ForwardRuleDraft {
        bind_addr: "127.0.0.1".to_string(),
        bind_port: bind_port.to_string(),
        remote_host: "web1.lan".to_string(),
        remote_port: remote_port.to_string(),
        ..Default::default()
    }
}

#[test]
fn jump_candidates_are_ssh_only_and_exclude_self() {
    let peers = sample_peers();
    let ids: Vec<i64> = jump_candidates(&peers, 9).iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![1, 2], "RDP sessions are never jump hosts");
    let ids: Vec<i64> = jump_candidates(&peers, 1).iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![2], "a session cannot jump through itself");
}

#[test]
fn jump_depth_beyond_one_is_rejected() {
    let peers = sample_peers();
    let err = jump_error(&peers, 9, Some(2)).expect("depth>1 must be refused");
    assert!(
        err.contains("depth > 1"),
        "the message names the rule: {err}"
    );
    assert_eq!(jump_error(&peers, 9, Some(1)), None);
    assert_eq!(jump_error(&peers, 9, None), None, "direct is always fine");
}

#[test]
fn jump_self_and_non_ssh_are_rejected() {
    let peers = sample_peers();
    assert!(jump_error(&peers, 1, Some(1)).is_some(), "no self-jump");
    assert!(
        jump_error(&peers, 9, Some(3)).is_some(),
        "RDP sessions cannot be jump hosts"
    );
}

#[test]
fn jump_stale_ids_report_missing_and_resolve_to_none() {
    let peers = sample_peers();
    let err = jump_error(&peers, 9, Some(77)).expect("stale ids explain themselves");
    assert!(err.contains(&jump_missing_label(77)), "message: {err}");
    assert_eq!(
        resolve_jump(&peers, 9, Some(77)),
        None,
        "stale self-heals on save"
    );
    assert_eq!(
        resolve_jump(&peers, 9, Some(2)),
        None,
        "depth>1 never stores"
    );
    assert_eq!(resolve_jump(&peers, 9, Some(1)), Some(1));
}

#[test]
fn jump_picker_model_lists_candidates_and_flags_stale() {
    let peers = sample_peers();
    let (labels, ids, selected) = jump_picker_model(&peers, 9, None);
    assert_eq!(labels[0], JUMP_NONE_LABEL);
    assert_eq!(ids, vec![None, Some(1), Some(2)]);
    assert_eq!(selected, 0);

    let (_, _, selected) = jump_picker_model(&peers, 9, Some(1));
    assert_eq!(selected, 1);

    // Stale: the missing entry is appended and selected, never a failure.
    let (labels, ids, selected) = jump_picker_model(&peers, 9, Some(77));
    assert_eq!(
        labels.last().expect("missing entry"),
        &jump_missing_label(77)
    );
    assert_eq!(*ids.last().expect("missing id"), Some(77));
    assert_eq!(selected, (labels.len() - 1) as u32);
}

#[test]
fn jump_and_forwards_store_for_ssh_only() {
    let folders = sample_folders();
    let peers = sample_peers();
    let ssh_fields = DialogFields {
        type_index: TYPE_INDEX_SSH,
        name: "web".to_string(),
        jump_session_id: Some(1),
        forward_drafts: vec![forward_draft("8080", "80")],
        ..Default::default()
    };
    let config = config_from_fields(&ssh_fields, &folders, &peers);
    assert_eq!(config.jump_session_id, Some(1));
    let rules = config.forward_rules.expect("SSH stores its rules");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].bind_port, 8080);
    assert_eq!(rules[0].remote_port, 80);

    // A hidden SSH group never leaks into other types.
    for type_index in [
        TYPE_INDEX_RDP,
        TYPE_INDEX_VNC,
        TYPE_INDEX_SERIAL,
        TYPE_INDEX_LOCAL,
    ] {
        let mut fields = ssh_fields.clone();
        fields.type_index = type_index;
        let config = config_from_fields(&fields, &folders, &peers);
        assert_eq!(config.jump_session_id, None, "type_index {type_index}");
        assert_eq!(config.forward_rules, None, "type_index {type_index}");
    }
}

#[test]
fn invalid_forward_drafts_store_as_no_rules() {
    let folders = sample_folders();
    let peers = sample_peers();
    let fields = DialogFields {
        type_index: TYPE_INDEX_SSH,
        forward_drafts: vec![forward_draft("nan", "80")],
        ..Default::default()
    };
    assert_eq!(
        config_from_fields(&fields, &folders, &peers).forward_rules,
        None,
        "the dialog's OK validation never lets these through; the mapping drops them"
    );
}

#[test]
fn empty_forward_editor_stores_no_rules() {
    let folders = sample_folders();
    let peers = sample_peers();
    let fields = DialogFields {
        type_index: TYPE_INDEX_SSH,
        ..Default::default()
    };
    assert_eq!(
        config_from_fields(&fields, &folders, &peers).forward_rules,
        None
    );
}

#[test]
fn collect_peer_sessions_walks_every_folder() {
    let store = SessionStore::in_memory().expect("in-memory store");
    let folder = store.insert_folder("ops", None).expect("folder");
    let root = SessionConfig {
        name: "root-box".to_string(),
        ..Default::default()
    };
    store.insert_session(&root).expect("root session");
    let nested = SessionConfig {
        name: "nested-box".to_string(),
        folder_id: Some(folder),
        ..Default::default()
    };
    store.insert_session(&nested).expect("nested session");
    let peers = collect_peer_sessions(&store);
    let names: Vec<&str> = peers.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"root-box"), "root sessions: {names:?}");
    assert!(names.contains(&"nested-box"), "nested sessions: {names:?}");
}

#[test]
fn nested_folders_are_pickable_and_marked_by_depth() {
    let store = SessionStore::in_memory().expect("in-memory store");
    let one = store.insert_folder("Folder 1", None).expect("folder 1");
    let two = store
        .insert_folder("Folder 2", Some(one))
        .expect("folder 2");
    let three = store
        .insert_folder("Folder 3", Some(two))
        .expect("folder 3");
    store.insert_folder("Other", None).expect("other");
    let folders = collect_folders(&store).expect("folders");
    assert_eq!(
        folder_labels(&folders),
        ["Folder 1", "└── Folder 2", "    └── Folder 3", "Other"]
    );
    // Picking the nested folder (index 0 is "None (root)") files the
    // session under it.
    let fields = DialogFields {
        name: "deep".into(),
        folder_index: 3,
        ..DialogFields::default()
    };
    let config = config_from_fields(&fields, &folders, &[]);
    assert_eq!(config.folder_id, Some(three));
}

// --- Save password to keyring ----------------------------------------------

mod save_password {
    use crate::transport::credentials::CredentialError;
    use crate::ui::session_dialog::{
        password_rows_visible, save_password_state, should_store_password, store_error_message,
        SAVE_PASSWORD_LABEL, SAVE_PASSWORD_LABEL_EMPTY, TYPE_INDEX_RDP, TYPE_INDEX_SSH,
    };

    // AUTH_LABELS order: password, key, agent, keyboard-interactive, none.
    const PASSWORD: u32 = 0;
    const KEY: u32 = 1;

    #[test]
    fn rows_show_for_the_password_method_only() {
        assert!(password_rows_visible(PASSWORD));
        for other in 1..5 {
            assert!(!password_rows_visible(other), "auth index {other}");
        }
    }

    #[test]
    fn checkbox_is_disabled_until_a_password_is_typed() {
        assert_eq!(
            save_password_state(false),
            (false, SAVE_PASSWORD_LABEL_EMPTY)
        );
        assert_eq!(save_password_state(true), (true, SAVE_PASSWORD_LABEL));
        assert_eq!(
            SAVE_PASSWORD_LABEL_EMPTY,
            "Save password to keyring (enter password to enable)"
        );
    }

    #[test]
    fn store_only_for_checked_ssh_password_with_text() {
        assert!(should_store_password(
            TYPE_INDEX_SSH,
            PASSWORD,
            true,
            "test-secret-XYZ"
        ));
        assert!(!should_store_password(
            TYPE_INDEX_SSH,
            PASSWORD,
            false,
            "test-secret-XYZ"
        ));
        assert!(!should_store_password(TYPE_INDEX_SSH, PASSWORD, true, ""));
        assert!(!should_store_password(
            TYPE_INDEX_SSH,
            KEY,
            true,
            "test-secret-XYZ"
        ));
        assert!(!should_store_password(
            TYPE_INDEX_RDP,
            PASSWORD,
            true,
            "test-secret-XYZ"
        ));
    }

    #[test]
    fn store_failure_names_the_reason() {
        let err = CredentialError::Unavailable("no credential source supports storing".into());
        assert_eq!(
            store_error_message(&err),
            "Could not store password: credential source unavailable: \
             no credential source supports storing"
        );
    }
}
