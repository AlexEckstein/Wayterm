//! One Remmina `.remmina` file → one session. `group=` (`/`-nested) is
//! the folder; `server=` splits as `[user@]host[:port]`. Password and
//! passphrase lines are never copied — each is a "secret skipped"
//! warning.

use std::collections::HashMap;

use super::{split_server, FolderSet, ImportReport, Parsed};
use crate::store::model::{AuthMethod, SessionConfig, SessionType};

/// Remmina keys that hold a secret (encrypted or not).
const SECRET_KEYS: [&str; 6] = [
    "password",
    "ssh_password",
    "ssh_passphrase",
    "ssh_tunnel_password",
    "ssh_tunnel_passphrase",
    "gateway_password",
];

/// Parse one `.remmina` file.
pub fn parse(text: &str) -> Parsed {
    let mut report = ImportReport::default();
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut section: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(name.to_ascii_lowercase());
            continue;
        }
        if section.as_deref().is_some_and(|s| s != "remmina") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if SECRET_KEYS.contains(&key.as_str()) {
            if !value.is_empty() {
                report.warnings.push(format!("{key}: secret skipped"));
            }
            continue;
        }
        fields.entry(key).or_insert_with(|| value.to_string());
    }

    let get = |key: &str| {
        fields
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    };
    let server = get("server").or(get("ssh_server"));
    let name = get("name")
        .or(server)
        .unwrap_or("Remmina session")
        .to_string();
    let protocol = get("protocol").unwrap_or("").to_ascii_uppercase();
    let session_type = match protocol.as_str() {
        "SSH" | "SFTP" => SessionType::SSH,
        "RDP" => SessionType::RDP,
        "VNC" => SessionType::VNC,
        other => {
            report.skipped += 1;
            report.warnings.push(format!(
                "{name}: protocol \"{other}\" is not supported, skipped"
            ));
            return (Vec::new(), Vec::new(), report);
        }
    };
    if protocol == "SFTP" {
        report
            .warnings
            .push(format!("{name}: SFTP profile imported as an SSH session"));
    }
    let Some(server) = server else {
        report.skipped += 1;
        report.warnings.push(format!("{name}: no server, skipped"));
        return (Vec::new(), Vec::new(), report);
    };

    let (server_user, host, port) = split_server(server);
    let port = port.unwrap_or_else(|text| {
        report.warnings.push(format!(
            "{name}: port in \"{text}\" is not a number, ignored"
        ));
        None
    });
    let mut folders = FolderSet::default();
    let folder = get("group")
        .or(get("groups"))
        .and_then(|g| folders.ensure(g.split('/')));
    let mut session = SessionConfig {
        name: name.clone(),
        session_type: session_type.clone(),
        folder_id: super::folder_ref(folder),
        host: Some(host),
        port,
        username: server_user
            .or(get("username").map(str::to_string))
            .or(get("ssh_username").map(str::to_string)),
        ..SessionConfig::default()
    };
    match session_type {
        SessionType::SSH => {
            session.auth_method = match get("ssh_auth") {
                None => None,
                Some("0") => Some(AuthMethod::Password),
                Some("1") | Some("3") => Some(AuthMethod::Key),
                Some("2") => Some(AuthMethod::Agent),
                Some("5") => Some(AuthMethod::KeyboardInteractive),
                Some(other) => {
                    report.warnings.push(format!(
                        "{name}: ssh_auth {other} has no Wayterm equivalent, ignored"
                    ));
                    None
                }
            };
            session.key_path = get("ssh_privatekey").map(str::to_string);
        }
        SessionType::RDP => {
            session.rdp_domain = get("domain").map(str::to_string);
            session.rdp_width = get("resolution_width").and_then(|v| v.parse().ok());
            session.rdp_height = get("resolution_height").and_then(|v| v.parse().ok());
            session.rdp_color_depth = get("colordepth").and_then(|v| v.parse().ok());
        }
        _ => {
            session.vnc_color_depth = get("colordepth").and_then(|v| v.parse().ok());
        }
    }
    if get("ssh_tunnel_enabled") == Some("1") {
        report
            .warnings
            .push(format!("{name}: SSH tunnel settings not imported"));
    }
    (folders.folders, vec![session], report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_protocol_is_skipped() {
        let (_, sessions, report) = parse("[remmina]\nname=x\nprotocol=SPICE\nserver=h\n");
        assert!(sessions.is_empty());
        assert_eq!(report.skipped, 1);
        assert!(report.warnings[0].contains("SPICE"));
    }

    #[test]
    fn missing_server_is_skipped() {
        let (_, sessions, report) = parse("[remmina]\nname=x\nprotocol=SSH\n");
        assert!(sessions.is_empty());
        assert_eq!(report.skipped, 1);
    }
}
