//! A MobaXterm `.ini` / `.mxtsessions` export → sessions. Every
//! `[Bookmarks…]` section is one folder, its `SubRep=` path
//! (`\`-separated) the nesting; each other line is a bookmark,
//! `name=#type#field%field%…#…`. Fields 1–3 of an SSH, RDP, or VNC
//! bookmark are host, port, and user; nothing else is mapped.
//! `[Passwords]` and `[Credentials]` are never read.

use super::{FolderSet, ImportReport, Parsed};
use crate::store::model::{SessionConfig, SessionType};

/// Sections that hold saved secrets.
const SECRET_SECTIONS: [&str; 2] = ["passwords", "credentials"];

/// Bookmark keys that describe the section, not a session.
const SECTION_KEYS: [&str; 2] = ["subrep", "imgnum"];

pub fn parse(text: &str) -> Parsed {
    let mut report = ImportReport::default();
    let mut folders = FolderSet::default();
    let mut sessions = Vec::new();
    let mut ssh_partial = 0;
    for section in sections(text) {
        let lowered = section.name.to_ascii_lowercase();
        if SECRET_SECTIONS.contains(&lowered.as_str()) {
            report.warnings.push(format!(
                "[{}] section skipped: secrets are never imported",
                section.name
            ));
            continue;
        }
        if !lowered.starts_with("bookmarks") {
            continue;
        }
        let folder = section
            .entries
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("subrep"))
            .and_then(|(_, path)| folders.ensure(path.split('\\')));
        for (name, value) in &section.entries {
            if SECTION_KEYS.contains(&name.to_ascii_lowercase().as_str()) {
                continue;
            }
            match bookmark(name, value, folder) {
                Ok(session) => {
                    if session.session_type == SessionType::SSH {
                        ssh_partial += 1;
                    }
                    sessions.push(session);
                }
                Err(reason) => {
                    report.skipped += 1;
                    report.warnings.push(format!("{name}: {reason}, skipped"));
                }
            }
        }
    }
    if ssh_partial > 0 {
        report.warnings.push(format!(
            "{ssh_partial} MobaXterm SSH session(s): only host, port and user imported \
             (key, agent, gateway and terminal settings are not)"
        ));
    }
    (folders.folders, sessions, report)
}

/// One bookmark line → a session, or why not.
fn bookmark(name: &str, value: &str, folder: Option<usize>) -> Result<SessionConfig, String> {
    let mut parts = value.split('#');
    let (Some(""), Some(kind), Some(fields)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("not a MobaXterm bookmark".to_string());
    };
    let session_type = match kind {
        "109" => SessionType::SSH,
        "91" => SessionType::RDP,
        "128" => SessionType::VNC,
        other => return Err(format!("session type {other} is not supported")),
    };
    let fields: Vec<&str> = fields.split('%').collect();
    let field = |i: usize| fields.get(i).map(|f| f.trim()).filter(|f| !f.is_empty());
    let host = field(1).ok_or("no host")?;
    let port = match field(2) {
        Some(port) => Some(
            port.parse::<u16>()
                .map_err(|_| format!("port \"{port}\" is not a number"))?,
        ),
        None => None,
    };
    Ok(SessionConfig {
        name: name.to_string(),
        session_type,
        folder_id: super::folder_ref(folder),
        host: Some(host.to_string()),
        port,
        username: field(3).map(str::to_string),
        ..SessionConfig::default()
    })
}

struct Section {
    name: String,
    entries: Vec<(String, String)>,
}

/// A minimal ini reader: `[name]` headers, `key=value` lines, `;`/`#`
/// comments. Lines before the first header are dropped.
fn sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push(Section {
                name: name.trim().to_string(),
                entries: Vec::new(),
            });
            continue;
        }
        if let (Some(section), Some((key, value))) = (sections.last_mut(), line.split_once('=')) {
            section
                .entries
                .push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_rejects_unsupported_and_malformed() {
        assert!(bookmark("t", "#98#0%host%23%%", None)
            .unwrap_err()
            .contains("98"));
        assert!(bookmark("t", "plain", None).is_err());
        assert!(bookmark("t", "#109#0%%22%", None).is_err());
        assert!(bookmark("t", "#109#0%h%notaport%", None).is_err());
    }

    #[test]
    fn ini_reader_keeps_section_order() {
        let got = sections("junk=1\n[A]\nk = v\n; c\n[B]\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].entries, vec![("k".to_string(), "v".to_string())]);
        assert!(got[1].entries.is_empty());
    }
}
