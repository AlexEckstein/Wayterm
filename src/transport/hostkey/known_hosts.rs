//! What `known_hosts` records about one host (doc 04 §Host key
//! verification), read as OpenSSH reads it: fields split on blanks, host
//! names compared without case, hashed names, `*` and `?` patterns, `!`
//! negation, and the `@revoked` marker.

use std::io;
use std::path::Path;

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// Where a presented key stands against a host's entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Standing {
    /// An entry holds this key.
    Trusted,
    /// The host has entries and none holds this key, whatever its type:
    /// the man-in-the-middle signal.
    Changed,
    /// An `@revoked` entry holds this key.
    Revoked,
    /// No entry names the host.
    Unknown,
}

/// The entries that name one host.
#[derive(Debug, Default)]
pub(super) struct HostEntries {
    /// Each entry's key type and base64 key, padding dropped.
    keys: Vec<(String, String)>,
    /// The base64 keys of its `@revoked` entries.
    revoked: Vec<String>,
}

impl HostEntries {
    /// The entries of `host:port` in the file at `path`. A missing file
    /// holds none; any other failure is an error, never an empty answer,
    /// which would turn a host with a recorded key into an unknown one.
    pub(super) fn read(path: &Path, host: &str, port: u16) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Self::parse(&String::from_utf8_lossy(&bytes), host, port)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }

    fn parse(text: &str, host: &str, port: u16) -> Self {
        let name = entry_name(host, port);
        let mut entries = Self::default();
        for line in text.lines() {
            let mut fields = line.split_ascii_whitespace().peekable();
            let marker = fields.next_if(|field| field.starts_with('@'));
            let (Some(hosts), Some(key_type), Some(key)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if hosts.starts_with('#') || !names_host(hosts, &name) {
                continue;
            }
            match marker {
                None => entries
                    .keys
                    .push((key_type.to_string(), unpadded(key).to_string())),
                Some("@revoked") => entries.revoked.push(unpadded(key).to_string()),
                // `@cert-authority`: host certificates are not supported.
                Some(_) => {}
            }
        }
        entries
    }

    /// Where `key`, the presented key's base64 form, stands.
    pub(super) fn standing(&self, key: &str) -> Standing {
        let key = unpadded(key);
        if self.revoked.iter().any(|revoked| revoked == key) {
            Standing::Revoked
        } else if self.keys.iter().any(|(_, recorded)| recorded == key) {
            Standing::Trusted
        } else if self.keys.is_empty() {
            Standing::Unknown
        } else {
            Standing::Changed
        }
    }

    /// Whether an entry holds a key of `key_type` (`ssh-ed25519`, …).
    pub(super) fn has_key_type(&self, key_type: &str) -> bool {
        self.keys.iter().any(|(recorded, _)| recorded == key_type)
    }
}

/// The name a host goes by in the file: `host`, or `[host]:port` off
/// port 22, in lower case.
fn entry_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

/// `base64` without its padding, the form keys are compared in.
pub(super) fn unpadded(base64: &str) -> &str {
    base64.trim_end_matches('=')
}

/// Whether a line's comma-separated host field names `name`: one of its
/// patterns matches and no negated one does.
fn names_host(patterns: &str, name: &str) -> bool {
    let mut named = false;
    for pattern in patterns.split(',') {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob(&negated.to_ascii_lowercase(), name) {
                return false;
            }
        } else if let Some(hashed) = pattern.strip_prefix("|1|") {
            named |= hashed_name(hashed, name);
        } else {
            named |= glob(&pattern.to_ascii_lowercase(), name);
        }
    }
    named
}

/// Whether `salt|hash`, both base64, is the HMAC-SHA1 of `name`.
fn hashed_name(entry: &str, name: &str) -> bool {
    let Some((salt, hash)) = entry.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    Hmac::<Sha1>::new_from_slice(&salt)
        .is_ok_and(|mac| mac.chain_update(name).verify_slice(&hash).is_ok())
}

/// Shell-style match: `*` is any run of characters, `?` any one.
fn glob(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    // Where to resume after the last `*`, and the text it has taken so far.
    let mut star = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p + 1, t));
                p += 1;
            }
            Some(&byte) if byte == b'?' || byte == text[t] => {
                p += 1;
                t += 1;
            }
            _ => {
                let Some((after, taken)) = star else {
                    return false;
                };
                star = Some((after, taken + 1));
                p = after;
                t = taken + 1;
            }
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn standing(file: &str, host: &str, port: u16, key: &str) -> Standing {
        HostEntries::parse(file, host, port).standing(key)
    }

    #[test]
    fn a_key_is_trusted_only_on_a_line_naming_the_host() {
        let file = format!("web.example ssh-ed25519 {KEY_A} a comment\n");
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Trusted);
        assert_eq!(standing(&file, "WEB.Example", 22, KEY_A), Standing::Trusted);
        assert_eq!(standing(&file, "web.example", 22, KEY_B), Standing::Changed);
        assert_eq!(standing(&file, "db.example", 22, KEY_A), Standing::Unknown);
        // Another port is another host.
        assert_eq!(
            standing(&file, "web.example", 2222, KEY_A),
            Standing::Unknown
        );
        let file = format!("[web.example]:2222 ssh-ed25519 {KEY_A}\n");
        assert_eq!(
            standing(&file, "web.example", 2222, KEY_A),
            Standing::Trusted
        );
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Unknown);
    }

    #[test]
    fn a_key_of_another_type_is_a_changed_key() {
        let file = "web.example ecdsa-sha2-nistp256 AAAAE2VjZHNh\n";
        assert_eq!(standing(file, "web.example", 22, KEY_A), Standing::Changed);
    }

    #[test]
    fn any_matching_entry_makes_the_key_trusted() {
        let file = format!(
            "web.example ssh-rsa AAAAB3NzaC1yc2E\nweb.example,10.0.0.1 ssh-ed25519 {KEY_A}\n"
        );
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Trusted);
        assert_eq!(standing(&file, "10.0.0.1", 22, KEY_A), Standing::Trusted);
        let entries = HostEntries::parse(&file, "web.example", 22);
        assert!(entries.has_key_type("ssh-rsa") && entries.has_key_type("ssh-ed25519"));
        assert!(!entries.has_key_type("ecdsa-sha2-nistp256"));
    }

    #[test]
    fn fields_split_on_tabs_and_runs_of_blanks() {
        for file in [
            format!("web.example\tssh-ed25519\t{KEY_A}\n"),
            format!("  web.example   ssh-ed25519  {KEY_A}\r\n"),
            // No newline at the end, padding or not.
            format!("web.example ssh-ed25519 {KEY_A}=="),
        ] {
            assert_eq!(
                standing(&file, "web.example", 22, KEY_A),
                Standing::Trusted,
                "{file:?}"
            );
            assert_eq!(
                standing(&file, "web.example", 22, KEY_B),
                Standing::Changed,
                "{file:?}"
            );
        }
    }

    #[test]
    fn comments_and_short_lines_name_no_host() {
        let file = format!(
            "# web.example ssh-ed25519 {KEY_A}\n#web.example ssh-ed25519 {KEY_A}\n\nweb.example ssh-ed25519\n"
        );
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Unknown);
    }

    #[test]
    fn patterns_match_and_negation_excludes() {
        let file = format!("*.example,!bad.example ssh-ed25519 {KEY_A}\n");
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Trusted);
        assert_eq!(standing(&file, "web.example", 22, KEY_B), Standing::Changed);
        assert_eq!(standing(&file, "bad.example", 22, KEY_A), Standing::Unknown);
        assert_eq!(standing(&file, "example", 22, KEY_A), Standing::Unknown);
        let file = format!("[web?.example]:2222 ssh-ed25519 {KEY_A}\n");
        assert_eq!(
            standing(&file, "web1.example", 2222, KEY_A),
            Standing::Trusted
        );
        assert_eq!(
            standing(&file, "web.example", 2222, KEY_A),
            Standing::Unknown
        );
    }

    #[test]
    fn hashed_names_match_the_lower_case_name() {
        let key = "AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";
        let file = format!(
            "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 {key}\n"
        );
        assert_eq!(standing(&file, "example.com", 22, key), Standing::Trusted);
        assert_eq!(standing(&file, "Example.COM", 22, key), Standing::Trusted);
        assert_eq!(standing(&file, "example.com", 22, KEY_A), Standing::Changed);
        assert_eq!(standing(&file, "example.org", 22, key), Standing::Unknown);
        // Not base64, or not a pair: no host.
        let broken = format!("|1|nope ssh-ed25519 {key}\n|1|a|b ssh-ed25519 {key}\n");
        assert_eq!(standing(&broken, "example.com", 22, key), Standing::Unknown);
    }

    #[test]
    fn a_revoked_key_is_refused_even_where_it_is_also_listed() {
        let file =
            format!("web.example ssh-ed25519 {KEY_A}\n@revoked * ssh-ed25519 {KEY_A} leaked\n");
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Revoked);
        assert_eq!(standing(&file, "db.example", 22, KEY_A), Standing::Revoked);
        // Revoking one key says nothing about another.
        assert_eq!(standing(&file, "db.example", 22, KEY_B), Standing::Unknown);
    }

    #[test]
    fn a_certificate_authority_line_is_no_host_key() {
        let file = format!("@cert-authority *.example ssh-ed25519 {KEY_A}\n");
        assert_eq!(standing(&file, "web.example", 22, KEY_A), Standing::Unknown);
        assert_eq!(standing(&file, "web.example", 22, KEY_B), Standing::Unknown);
    }

    #[test]
    fn bytes_that_are_not_utf8_do_not_hide_the_other_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let mut file = b"# caf\xe9\n".to_vec();
        file.extend_from_slice(format!("web.example ssh-ed25519 {KEY_A}\n").as_bytes());
        std::fs::write(&path, file).unwrap();
        let entries = HostEntries::read(&path, "web.example", 22).unwrap();
        assert_eq!(entries.standing(KEY_B), Standing::Changed);
    }

    #[test]
    fn a_missing_file_is_empty_and_an_unreadable_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let entries = HostEntries::read(&dir.path().join("none"), "web.example", 22).unwrap();
        assert_eq!(entries.standing(KEY_A), Standing::Unknown);
        assert!(HostEntries::read(dir.path(), "web.example", 22).is_err());
    }

    #[test]
    fn glob_matches_runs_and_single_characters() {
        for (pattern, text, expected) in [
            ("", "", true),
            ("*", "", true),
            ("*", "anything", true),
            ("a*c", "abbbc", true),
            ("a*c", "ac", true),
            ("a*c", "abd", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*.example", "a.b.example", true),
            ("*.example", "example", false),
            ("a*b*c", "a1b2b3c", true),
            ("a*b*c", "a1b2b3", false),
            ("abc", "abcd", false),
            ("abcd", "abc", false),
        ] {
            assert_eq!(glob(pattern, text), expected, "{pattern:?} on {text:?}");
        }
    }
}
