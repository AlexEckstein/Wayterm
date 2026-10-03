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
fn default_is_sane() {
    let cfg = SessionConfig::default();

    assert_eq!(cfg.session_type, SessionType::SSH);
    assert_eq!(cfg.connect_count, 0);
    assert!(!cfg.tmux_enabled);
    assert_eq!(cfg.folder_id, None);
}
