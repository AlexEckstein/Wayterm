//! An Asbru-CM YAML export → folders and sessions. Entries live under
//! `environments` (or at the top level), keyed by uuid; `_is_group` marks
//! a folder and `parent` names its parent's uuid (`__PAC__ROOT__` = the
//! root). Any field naming a password or passphrase is never copied.

use std::collections::{BTreeSet, HashMap};

use serde_yaml_ng::{Mapping, Value};

use super::{FolderSet, ImportError, ImportReport, Parsed};
use crate::store::model::{AuthMethod, SessionConfig, SessionType};

/// The `parent` of a top-level entry.
const ROOT: &str = "__PAC__ROOT__";

/// Fields read here (or deliberately structural).
const KNOWN_FIELDS: [&str; 10] = [
    "_is_group",
    "name",
    "parent",
    "children",
    "method",
    "ip",
    "port",
    "user",
    "auth type",
    "public key",
];

/// Parse an Asbru-CM YAML export.
pub fn parse(text: &str) -> Result<Parsed, ImportError> {
    let doc: Value =
        serde_yaml_ng::from_str(text).map_err(|e| ImportError::Parse(e.to_string()))?;
    let top = doc
        .as_mapping()
        .ok_or_else(|| ImportError::Parse("not an Asbru export (no mapping)".to_string()))?;
    let entries = match top.get("environments").and_then(Value::as_mapping) {
        Some(environments) => environments,
        None => top,
    };
    let entries: Vec<(String, &Mapping)> = entries
        .iter()
        .filter_map(|(key, value)| Some((key.as_str()?.to_string(), value.as_mapping()?)))
        .filter(|(key, _)| !key.starts_with("__"))
        .collect();
    let groups: HashMap<&str, &Mapping> = entries
        .iter()
        .filter(|(_, entry)| truthy(entry.get("_is_group")))
        .map(|(uuid, entry)| (uuid.as_str(), *entry))
        .collect();

    let mut report = ImportReport::default();
    let mut folders = FolderSet::default();
    let mut sessions = Vec::new();
    let mut secrets = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for (uuid, entry) in &entries {
        let name = text_field(entry, "name").unwrap_or_else(|| uuid.clone());
        let mut path = group_path(&groups, text_field(entry, "parent"));
        if groups.contains_key(uuid.as_str()) {
            // Empty groups are kept too.
            path.push(name);
            folders.ensure(path.iter().map(String::as_str));
            continue;
        }
        let folder = folders.ensure(path.iter().map(String::as_str));
        for (key, value) in entry.iter() {
            let Some(key) = key.as_str() else { continue };
            let lowered = key.to_ascii_lowercase();
            if lowered.contains("pass") || lowered.contains("secret") {
                if !is_blank(value) {
                    secrets.insert(key.to_string());
                }
            } else if !KNOWN_FIELDS.contains(&lowered.as_str()) && !is_blank(value) {
                unknown.insert(key.to_string());
            }
        }
        let method = text_field(entry, "method").unwrap_or_default();
        let session_type = if method.starts_with("SSH") {
            SessionType::SSH
        } else if method.starts_with("RDP") {
            SessionType::RDP
        } else if method.starts_with("VNC") {
            SessionType::VNC
        } else {
            report.skipped += 1;
            report.warnings.push(format!(
                "{name}: method \"{method}\" is not supported, skipped"
            ));
            continue;
        };
        let Some(host) = text_field(entry, "ip") else {
            report.skipped += 1;
            report.warnings.push(format!("{name}: no host, skipped"));
            continue;
        };
        let port = match text_field(entry, "port") {
            Some(port) => match port.parse::<u16>() {
                Ok(port) => Some(port),
                Err(_) => {
                    report
                        .warnings
                        .push(format!("{name}: port \"{port}\" is not a number, ignored"));
                    None
                }
            },
            None => None,
        };
        let mut session = SessionConfig {
            name,
            session_type,
            folder_id: super::folder_ref(folder),
            host: Some(host),
            port,
            username: text_field(entry, "user"),
            ..SessionConfig::default()
        };
        if session.session_type == SessionType::SSH {
            match text_field(entry, "auth type").as_deref() {
                Some("publickey") => {
                    session.auth_method = Some(AuthMethod::Key);
                    session.key_path = text_field(entry, "public key");
                }
                Some("userpass") => session.auth_method = Some(AuthMethod::Password),
                _ => {}
            }
        }
        sessions.push(session);
    }
    if !secrets.is_empty() {
        report.warnings.push(format!(
            "secret skipped: {} (never imported)",
            secrets.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !unknown.is_empty() {
        report.warnings.push(format!(
            "Asbru fields not imported: {}",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok((folders.folders, sessions, report))
}

/// Names of the groups from the root down to `parent`. A dangling or
/// cyclic parent chain stops where it breaks.
fn group_path(groups: &HashMap<&str, &Mapping>, mut parent: Option<String>) -> Vec<String> {
    let mut path = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some(uuid) = parent.filter(|p| p != ROOT) {
        let Some(group) = groups.get(uuid.as_str()) else {
            break;
        };
        if !seen.insert(uuid.clone()) {
            break;
        }
        path.push(text_field(group, "name").unwrap_or_else(|| uuid.clone()));
        parent = text_field(group, "parent");
    }
    path.reverse();
    path
}

/// A scalar field as text; blank → `None`.
fn text_field(entry: &Mapping, key: &str) -> Option<String> {
    let text = match entry.get(key)? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// Whether a YAML value is true (`1`, `true`, `yes`).
fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|n| n != 0),
        Some(Value::String(s)) => matches!(s.trim(), "1" | "true" | "yes"),
        _ => false,
    }
}

/// Null, empty, zero, or false: a field left at its default.
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.trim().is_empty(),
        Value::Sequence(s) => s.is_empty(),
        Value::Mapping(m) => m.is_empty(),
        Value::Tagged(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_a_mapping_is_a_parse_error() {
        assert!(matches!(parse("- a\n- b\n"), Err(ImportError::Parse(_))));
        assert!(matches!(parse(": : :"), Err(ImportError::Parse(_))));
    }

    #[test]
    fn group_cycle_terminates() {
        let (folders, sessions, _) = parse(
            "a: {_is_group: 1, name: A, parent: b}\n\
             b: {_is_group: 1, name: B, parent: a}\n\
             s: {name: s, method: SSH, ip: h, parent: a}\n",
        )
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(!folders.is_empty());
    }
}
