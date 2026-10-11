//! Wire-format and round-trip tests for the session model.
//!
//! Every string asserted here is the exact vocabulary docs/06-session-store.md
//! stores in its TEXT and JSON columns. Treat any change as a schema break:
//! the database and on-disk exports depend on these spellings.

use super::*;

/// Serialize a value to its JSON wire string (JSON-quoted for enums).
fn wire<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn session_type_wire_formats() {
    // Doc 06 `sessions.type` column: 'ssh', 'rdp', 'vnc', 'serial', 'local'.
    assert_eq!(wire(&SessionType::SSH), "\"ssh\"");
    assert_eq!(wire(&SessionType::RDP), "\"rdp\"");
    assert_eq!(wire(&SessionType::VNC), "\"vnc\"");
    assert_eq!(wire(&SessionType::Serial), "\"serial\"");
    // Not snake_case "local_shell" — doc 06 stores 'local'.
    assert_eq!(wire(&SessionType::LocalShell), "\"local\"");

    // Wire strings deserialize back, and only the exact spelling is accepted.
    assert_eq!(
        serde_json::from_str::<SessionType>("\"ssh\"").unwrap(),
        SessionType::SSH
    );
    assert_eq!(
        serde_json::from_str::<SessionType>("\"local\"").unwrap(),
        SessionType::LocalShell
    );
    assert!(serde_json::from_str::<SessionType>("\"local_shell\"").is_err());
}

#[test]
fn auth_method_wire_formats() {
    // Doc 06 `sessions.auth_method` column: 'password', 'key', 'agent',
    // 'keyboard-interactive', 'none'.
    assert_eq!(wire(&AuthMethod::Password), "\"password\"");
    assert_eq!(wire(&AuthMethod::Key), "\"key\"");
    assert_eq!(wire(&AuthMethod::Agent), "\"agent\"");
    // Hyphen, not snake_case "keyboard_interactive" — doc 06 stores
    // 'keyboard-interactive'.
    assert_eq!(
        wire(&AuthMethod::KeyboardInteractive),
        "\"keyboard-interactive\""
    );
    assert_eq!(wire(&AuthMethod::None), "\"none\"");

    assert_eq!(
        serde_json::from_str::<AuthMethod>("\"keyboard-interactive\"").unwrap(),
        AuthMethod::KeyboardInteractive
    );
    assert!(serde_json::from_str::<AuthMethod>("\"keyboard_interactive\"").is_err());
}

#[test]
fn host_key_policy_wire_formats() {
    // Doc 06 `sessions.host_key_policy` column: 'strict', 'accept-new',
    // 'off' — doc 11 matches OpenSSH's StrictHostKeyChecking=accept-new.
    assert_eq!(wire(&HostKeyPolicy::Strict), "\"strict\"");
    assert_eq!(wire(&HostKeyPolicy::AcceptNew), "\"accept-new\"");
    assert_eq!(wire(&HostKeyPolicy::Off), "\"off\"");

    assert_eq!(
        serde_json::from_str::<HostKeyPolicy>("\"accept-new\"").unwrap(),
        HostKeyPolicy::AcceptNew
    );
    assert!(serde_json::from_str::<HostKeyPolicy>("\"accept_new\"").is_err());
}

#[test]
fn session_config_json_roundtrip() {
    let cfg = SessionConfig {
        id: 42,
        name: "prod-db".to_string(),
        session_type: SessionType::SSH,
        folder_id: Some(7),
        host: Some("db.prod.lan".to_string()),
        port: Some(22),
        username: Some("admin".to_string()),
        auth_method: Some(AuthMethod::Key),
        key_path: Some("~/.ssh/id_ed25519".to_string()),
        host_key_policy: Some(HostKeyPolicy::AcceptNew),
        jump_session_id: Some(3),
        forward_rules: Some(vec![ForwardRule {
            kind: ForwardKind::Local,
            bind_addr: "127.0.0.1".to_string(),
            bind_port: 8080,
            remote_host: "web1.lan".to_string(),
            remote_port: 80,
            enabled: true,
        }]),
        tmux_enabled: true,
        tmux_session_name: Some("wayterm-main".to_string()),
        ssh_options: Some(SshOptions {
            compression: true,
            keepalive_interval: Some(30),
            keepalive_count: Some(3),
            ssh_w_fallback: false,
        }),
        rdp_domain: Some("CORP".to_string()),
        rdp_width: Some(1920),
        rdp_height: Some(1080),
        rdp_color_depth: Some(32),
        rdp_compression: true,
        rdp_options: Some(RdpOptions {
            audio: true,
            clipboard: false,
            fonts: true,
            printer: false,
        }),
        vnc_color_depth: Some(24),
        vnc_options: Some(VncOptions {
            quality: 7,
            scaling: true,
            view_only: false,
        }),
        serial_device: Some("/dev/ttyUSB0".to_string()),
        serial_baud_rate: Some(115200),
        serial_data_bits: Some(8),
        serial_parity: Some("none".to_string()),
        serial_stop_bits: Some(1),
        serial_flow_control: Some("none".to_string()),
        local_shell: Some("/bin/bash".to_string()),
        local_working_dir: Some("/srv".to_string()),
        preferred_backend: Some("vte".to_string()),
        color_scheme: Some("Dark".to_string()),
        font_family: Some("Monospace".to_string()),
        font_size: Some(12.5),
        opacity: Some(0.9),
        icon: Some("wayterm-homelab".to_string()),
        notes: Some("primary db".to_string()),
        last_connected: Some("2026-10-02T12:00:00Z".to_string()),
        connect_count: 17,
    };

    let json = serde_json::to_string(&cfg).unwrap();
    let rt: SessionConfig = serde_json::from_str(&json).unwrap();

    // Field-by-field: SessionConfig deliberately has no PartialEq (float
    // fields font_size/opacity), so every field is asserted individually.
    assert_eq!(rt.id, cfg.id);
    assert_eq!(rt.name, cfg.name);
    assert_eq!(rt.session_type, cfg.session_type);
    assert_eq!(rt.folder_id, cfg.folder_id);
    assert_eq!(rt.host, cfg.host);
    assert_eq!(rt.port, cfg.port);
    assert_eq!(rt.username, cfg.username);
    assert_eq!(rt.auth_method, cfg.auth_method);
    assert_eq!(rt.key_path, cfg.key_path);
    assert_eq!(rt.host_key_policy, cfg.host_key_policy);
    assert_eq!(rt.jump_session_id, cfg.jump_session_id);
    assert_eq!(rt.forward_rules, cfg.forward_rules);
    assert_eq!(rt.tmux_enabled, cfg.tmux_enabled);
    assert_eq!(rt.tmux_session_name, cfg.tmux_session_name);
    assert_eq!(rt.ssh_options, cfg.ssh_options);
    assert_eq!(rt.rdp_domain, cfg.rdp_domain);
    assert_eq!(rt.rdp_width, cfg.rdp_width);
    assert_eq!(rt.rdp_height, cfg.rdp_height);
    assert_eq!(rt.rdp_color_depth, cfg.rdp_color_depth);
    assert_eq!(rt.rdp_compression, cfg.rdp_compression);
    assert_eq!(rt.rdp_options, cfg.rdp_options);
    assert_eq!(rt.vnc_color_depth, cfg.vnc_color_depth);
    assert_eq!(rt.vnc_options, cfg.vnc_options);
    assert_eq!(rt.serial_device, cfg.serial_device);
    assert_eq!(rt.serial_baud_rate, cfg.serial_baud_rate);
    assert_eq!(rt.serial_data_bits, cfg.serial_data_bits);
    assert_eq!(rt.serial_parity, cfg.serial_parity);
    assert_eq!(rt.serial_stop_bits, cfg.serial_stop_bits);
    assert_eq!(rt.serial_flow_control, cfg.serial_flow_control);
    assert_eq!(rt.local_shell, cfg.local_shell);
    assert_eq!(rt.local_working_dir, cfg.local_working_dir);
    assert_eq!(rt.preferred_backend, cfg.preferred_backend);
    assert_eq!(rt.color_scheme, cfg.color_scheme);
    assert_eq!(rt.font_family, cfg.font_family);
    assert_eq!(rt.font_size, cfg.font_size);
    assert_eq!(rt.opacity, cfg.opacity);
    assert_eq!(rt.icon, cfg.icon);
    assert_eq!(rt.notes, cfg.notes);
    assert_eq!(rt.last_connected, cfg.last_connected);
    assert_eq!(rt.connect_count, cfg.connect_count);
}

#[test]
fn forward_rule_roundtrip() {
    let rule = ForwardRule {
        kind: ForwardKind::Local,
        bind_addr: "127.0.0.1".to_string(),
        bind_port: 8080,
        remote_host: "web1.lan".to_string(),
        remote_port: 80,
        enabled: true,
    };

    // `kind: Local` wires as "local" — doc 06's forward_rules JSON example.
    let json = serde_json::to_string(&rule).unwrap();
    assert!(
        json.contains(r#""kind":"local""#),
        "kind must wire as \"local\", got: {json}"
    );

    let rt: ForwardRule = serde_json::from_str(&json).unwrap();
    assert_eq!(rt, rule);

    // v2 kinds: wire format fixed from day one so the JSON column never
    // shifts under stored rows (doc 11 §Port forwarding).
    assert_eq!(wire(&ForwardKind::Remote), "\"remote\"");
    assert_eq!(wire(&ForwardKind::Dynamic), "\"dynamic\"");
}

#[test]
fn ssh_w_fallback_defaults_off_for_old_rows() {
    // Rows written before `ssh_w_fallback` existed (doc 11 §Jump hosts)
    // must load as `false`, never fail to parse.
    let options: SshOptions =
        serde_json::from_str(r#"{"compression":true,"keepalive_interval":30,"keepalive_count":3}"#)
            .unwrap();
    assert!(!options.ssh_w_fallback);

    let on: SshOptions = serde_json::from_str(
        r#"{"compression":false,"keepalive_interval":null,"keepalive_count":null,"ssh_w_fallback":true}"#,
    )
    .unwrap();
    assert!(on.ssh_w_fallback);
}

#[test]
fn default_is_sane() {
    let cfg = SessionConfig::default();

    assert_eq!(cfg.session_type, SessionType::SSH);
    assert_eq!(cfg.connect_count, 0);
    assert!(!cfg.tmux_enabled);
    assert_eq!(cfg.folder_id, None);
}

// ---------------------------------------------------------------------------
// Per-type value vocabularies (used by the session dialog's DropDowns)
// ---------------------------------------------------------------------------

#[test]
fn vocab_defaults_are_members_of_their_vocabulary() {
    // Doc 06 names these defaults in the schema comments; each must be
    // selectable in the dialog's DropDown for that column.
    assert!(SERIAL_PARITIES.contains(&SERIAL_PARITY_DEFAULT));
    assert!(SERIAL_FLOW_CONTROLS.contains(&SERIAL_FLOW_CONTROL_DEFAULT));
    assert!(SERIAL_DATA_BITS.contains(&SERIAL_DATA_BITS_DEFAULT));
    assert!(SERIAL_STOP_BITS.contains(&SERIAL_STOP_BITS_DEFAULT));
    assert!(RDP_COLOR_DEPTHS.contains(&RDP_COLOR_DEPTH_DEFAULT));

    // Doc 06: serial_data_bits DEFAULT 8, serial_stop_bits DEFAULT 1,
    // serial_parity/serial_flow_control DEFAULT 'none'.
    assert_eq!(SERIAL_DATA_BITS_DEFAULT, 8);
    assert_eq!(SERIAL_STOP_BITS_DEFAULT, 1);
    assert_eq!(SERIAL_PARITY_DEFAULT, "none");
    assert_eq!(SERIAL_FLOW_CONTROL_DEFAULT, "none");

    // Doc 08 §RDP passes `/bpp:{rdp_color_depth.unwrap_or(32)}` to xfreerdp,
    // so 32 is the fallback the dialog must default to.
    assert_eq!(RDP_COLOR_DEPTH_DEFAULT, 32);
}

#[test]
fn vocab_value_resolves_every_index() {
    for (index, expected) in SERIAL_PARITIES.iter().enumerate() {
        assert_eq!(
            vocab_value(&SERIAL_PARITIES, index as u32, SERIAL_PARITY_DEFAULT),
            *expected
        );
    }
    assert_eq!(
        vocab_value(&SERIAL_FLOW_CONTROLS, 1, SERIAL_FLOW_CONTROL_DEFAULT),
        "hardware"
    );
    assert_eq!(
        vocab_value(&SERIAL_DATA_BITS, 3, SERIAL_DATA_BITS_DEFAULT),
        8
    );
    assert_eq!(
        vocab_value(&SERIAL_STOP_BITS, 1, SERIAL_STOP_BITS_DEFAULT),
        2
    );
    assert_eq!(
        vocab_value(&RDP_COLOR_DEPTHS, 2, RDP_COLOR_DEPTH_DEFAULT),
        32
    );
}

#[test]
fn vocab_value_out_of_range_falls_back_to_default() {
    assert_eq!(
        vocab_value(&SERIAL_PARITIES, 99, SERIAL_PARITY_DEFAULT),
        "none"
    );
    assert_eq!(
        vocab_value(&SERIAL_DATA_BITS, 99, SERIAL_DATA_BITS_DEFAULT),
        8
    );
    assert_eq!(
        vocab_value(&RDP_COLOR_DEPTHS, 7, RDP_COLOR_DEPTH_DEFAULT),
        32
    );
}

#[test]
fn vocab_index_round_trips_every_value() {
    for (index, expected) in SERIAL_PARITIES.iter().enumerate() {
        assert_eq!(
            vocab_index(&SERIAL_PARITIES, Some(*expected), SERIAL_PARITY_DEFAULT),
            index as u32
        );
    }
    for (index, expected) in SERIAL_FLOW_CONTROLS.iter().enumerate() {
        assert_eq!(
            vocab_index(
                &SERIAL_FLOW_CONTROLS,
                Some(*expected),
                SERIAL_FLOW_CONTROL_DEFAULT
            ),
            index as u32
        );
    }
    for (index, expected) in RDP_COLOR_DEPTHS.iter().enumerate() {
        assert_eq!(
            vocab_index(&RDP_COLOR_DEPTHS, Some(*expected), RDP_COLOR_DEPTH_DEFAULT),
            index as u32
        );
    }
}

#[test]
fn vocab_index_falls_back_to_the_default_selection() {
    // NULL column (doc 06's nullable per-type columns) selects the default.
    assert_eq!(
        vocab_index(&SERIAL_PARITIES, None, SERIAL_PARITY_DEFAULT),
        0
    );
    assert_eq!(
        vocab_index(&RDP_COLOR_DEPTHS, None, RDP_COLOR_DEPTH_DEFAULT),
        2
    );
    // A value written by a newer/foreign writer that this build does not know
    // normalises to the documented default rather than staying unselectable.
    assert_eq!(
        vocab_index(&SERIAL_PARITIES, Some("weird"), SERIAL_PARITY_DEFAULT),
        0
    );
    assert_eq!(
        vocab_index(&RDP_COLOR_DEPTHS, Some(64), RDP_COLOR_DEPTH_DEFAULT),
        2
    );
}

// ---------------------------------------------------------------------------
// Wire compatibility: rows written before per-type fields existed
// ---------------------------------------------------------------------------

#[test]
fn pre_per_type_field_json_still_deserializes() {
    // A stored payload with none of the per-type groups present — every
    // serial/rdp/vnc/local field takes its default rather than failing.
    let json = r#"{
        "id": 3,
        "name": "legacy",
        "session_type": "ssh",
        "host": "old.example",
        "port": 22,
        "username": "root",
        "auth_method": "password"
    }"#;

    let cfg: SessionConfig = serde_json::from_str(json).unwrap();

    assert_eq!(cfg.id, 3);
    assert_eq!(cfg.name, "legacy");
    assert_eq!(cfg.session_type, SessionType::SSH);
    assert_eq!(cfg.host.as_deref(), Some("old.example"));
    assert_eq!(cfg.port, Some(22));
    assert_eq!(cfg.auth_method, Some(AuthMethod::Password));

    // Per-type groups default to "unset" so a later save writes NULL and the
    // doc-06 column defaults (8 / 1 / 'none') keep applying in SQLite.
    assert_eq!(cfg.serial_device, None);
    assert_eq!(cfg.serial_baud_rate, None);
    assert_eq!(cfg.serial_data_bits, None);
    assert_eq!(cfg.serial_parity, None);
    assert_eq!(cfg.serial_stop_bits, None);
    assert_eq!(cfg.serial_flow_control, None);
    assert_eq!(cfg.rdp_domain, None);
    assert_eq!(cfg.rdp_width, None);
    assert_eq!(cfg.rdp_height, None);
    assert_eq!(cfg.rdp_color_depth, None);
    assert!(!cfg.rdp_compression);
    assert_eq!(cfg.rdp_options, None);
    assert_eq!(cfg.vnc_color_depth, None);
    assert_eq!(cfg.vnc_options, None);
    assert_eq!(cfg.local_shell, None);
    assert_eq!(cfg.local_working_dir, None);
    assert_eq!(cfg.connect_count, 0);
}

#[test]
fn pre_per_type_field_json_round_trips_with_per_type_fields_preserved() {
    // The full pre-P1.5 wire format, spelled out field-by-field: the dialog
    // work adds no column, so this payload must keep its exact spelling and
    // round-trip through the struct unchanged.
    let json = r#"{
        "id": 11,
        "name": "console",
        "session_type": "serial",
        "folder_id": null,
        "host": null,
        "port": null,
        "username": null,
        "auth_method": null,
        "key_path": null,
        "host_key_policy": null,
        "jump_session_id": null,
        "forward_rules": null,
        "tmux_enabled": false,
        "tmux_session_name": null,
        "ssh_options": null,
        "rdp_domain": null,
        "rdp_width": null,
        "rdp_height": null,
        "rdp_color_depth": null,
        "rdp_compression": false,
        "rdp_options": null,
        "vnc_color_depth": null,
        "vnc_options": null,
        "serial_device": "/dev/ttyUSB0",
        "serial_baud_rate": 115200,
        "serial_data_bits": 8,
        "serial_parity": "none",
        "serial_stop_bits": 1,
        "serial_flow_control": "none",
        "local_shell": null,
        "local_working_dir": null,
        "preferred_backend": null,
        "color_scheme": null,
        "font_family": null,
        "font_size": null,
        "opacity": null,
        "notes": null,
        "last_connected": null,
        "connect_count": 0
    }"#;

    let cfg: SessionConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.session_type, SessionType::Serial);
    assert_eq!(cfg.serial_device.as_deref(), Some("/dev/ttyUSB0"));
    assert_eq!(cfg.serial_baud_rate, Some(115200));
    assert_eq!(cfg.serial_data_bits, Some(8));
    assert_eq!(cfg.serial_parity.as_deref(), Some("none"));
    assert_eq!(cfg.serial_stop_bits, Some(1));
    assert_eq!(cfg.serial_flow_control.as_deref(), Some("none"));

    // Re-serializing produces exactly the same payload: no field was added,
    // renamed, or reordered by the per-type dialog work.
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        value,
        "SessionConfig wire format changed — that is a schema break"
    );
}
