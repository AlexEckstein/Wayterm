//! Paths as the sidebar handles them: joining and walking remote paths,
//! a local tab's typed path, `text/uri-list` drops, and the shell's OSC 7
//! directory reports.

/// `dir/name`; an absolute `name` or an empty `dir` yields `name`.
pub fn join_path(dir: &str, name: &str) -> String {
    if name.starts_with('/') || dir.is_empty() {
        return name.to_string();
    }
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The [`PARENT_ROW`] target; `None` at the root or for an empty path.
/// A relative path that names no directory goes up with `..` steps, for
/// the server to resolve: `.`, where browsing starts, has `..` above it.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let (head, last) = match trimmed.rsplit_once('/') {
        Some((head, last)) => (Some(head), last),
        None => (None, trimmed),
    };
    Some(match (head, last) {
        (_, "..") => format!("{trimmed}/.."),
        (Some(head), ".") => format!("{head}/.."),
        (None, ".") => "..".to_string(),
        (Some(""), _) => "/".to_string(),
        (Some(head), _) => head.to_string(),
        (None, _) => ".".to_string(),
    })
}

/// Where a local-shell tab's sidebar goes for `typed` (doc 07 §Local
/// tabs): `~` is `home`, a relative path starts at `current`, and `.` and
/// `..` steps are taken by name, as a shell's `cd` takes them.
pub fn local_target(current: &str, typed: &str, home: &str) -> String {
    let base = if current.starts_with('/') {
        current
    } else {
        home
    };
    let joined = match typed.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{home}{rest}"),
        _ => join_path(base, typed),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => _ = parts.pop(),
            name => parts.push(name),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The last path component.
pub fn file_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Local paths from a `text/uri-list` drop: comments and blanks skipped,
/// only `file://` URIs with an empty or `localhost` host, percent-decoded.
pub fn parse_uri_list(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(path) = uri_to_path(line) {
            paths.push(path);
        }
    }
    paths
}

/// One `file://` URI to a local path, or `None` when it is not one.
fn uri_to_path(uri: &str) -> Option<String> {
    let (host, path) = split_file_uri(uri)?;
    (host.is_empty() || host.eq_ignore_ascii_case("localhost")).then_some(path)
}

/// A `file://` URI's host and percent-decoded path.
fn split_file_uri(uri: &str) -> Option<(&str, String)> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let path = percent_decode(&rest[slash..]);
    (!path.is_empty()).then(|| (&rest[..slash], path))
}

/// Reads the shell's working directory from the terminal's OSC 7 reports
/// (`file://host/path`, doc 07 §Following the shell).
///
/// The first report pins the host: a nested `ssh` inside the session
/// reports another one, and its paths mean nothing to this session's SFTP
/// channel. A local shell's follower knows its host from the start
/// ([`local`](Self::local)): its own shell may never report, and a nested
/// `ssh` must not be taken for it. Whether the directory changed is
/// [`SftpView::shell_moved`]'s call, since probes find it too.
#[derive(Clone, Debug, Default)]
pub struct DirectoryFollower {
    host: Option<String>,
    /// This machine's name, for a local shell.
    local: Option<String>,
}

impl DirectoryFollower {
    /// For a shell on this machine, named `hostname`: only its own reports
    /// count — no host, `localhost`, or `hostname`, where a name with no
    /// domain matches the same name with one.
    pub fn local(hostname: &str) -> Self {
        Self {
            host: None,
            local: Some(hostname.to_string()),
        }
    }

    /// The reported directory, or `None` when the report is not a
    /// `file://` URI for this session's host.
    pub fn report(&mut self, uri: &str) -> Option<String> {
        let (host, path) = split_file_uri(uri)?;
        if let Some(own) = &self.local {
            return is_this_machine(host, own).then_some(path);
        }
        match &self.host {
            Some(pinned) if !pinned.eq_ignore_ascii_case(host) => return None,
            Some(_) => {}
            None => self.host = Some(host.to_string()),
        }
        Some(path)
    }
}

fn is_this_machine(host: &str, own: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let (host, own) = (host.to_ascii_lowercase(), own.to_ascii_lowercase());
    match (host.split_once('.'), own.split_once('.')) {
        (Some((short, _)), None) => short == own,
        (None, Some((short, _))) => host == short,
        _ => host == own,
    }
}

/// Decode `%XX` escapes in a URI path; invalid escapes stay verbatim.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
