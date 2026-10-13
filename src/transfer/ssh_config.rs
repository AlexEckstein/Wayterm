//! `~/.ssh/config` → sessions: a read-only block scanner (doc 06
//! §Import and export; russh-config cannot enumerate `Host` blocks).
//!
//! Each exact alias on a `Host` line is one session; wildcard patterns
//! become template sessions; negated patterns never do; `Match` blocks are
//! skipped. Like OpenSSH, the first value obtained for an alias wins, and
//! an `Include` is read where it appears. Key files are never read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{read_source, ImportReport, Parsed};
use crate::store::model::{AuthMethod, SessionConfig, SshOptions};

/// `Include` nesting limit.
const MAX_INCLUDE_DEPTH: usize = 5;

pub fn parse(text: &str, base_dir: Option<&Path>) -> Parsed {
    let mut scanner = Scanner {
        base_dir: base_dir.map(Path::to_path_buf),
        home: std::env::var_os("HOME").map(PathBuf::from),
        ..Scanner::default()
    };
    scanner.scan(text, Context::Global);
    scanner.finish()
}

/// Which sessions the lines being read apply to.
#[derive(Clone)]
enum Context {
    /// Before any `Host`/`Match` line.
    Global,
    /// Inside a `Host` block: the sessions of its non-negated patterns.
    Hosts(Vec<usize>),
    /// Inside a skipped `Match` block.
    Match,
}

#[derive(Default)]
struct Scanner {
    base_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    sessions: Vec<SessionConfig>,
    by_name: HashMap<String, usize>,
    /// Raw `ProxyJump` per session, resolved in [`Scanner::finish`].
    proxy_jump: Vec<Option<String>>,
    /// Canonical paths of the `Include` chain being read.
    include_stack: Vec<PathBuf>,
    match_blocks: usize,
    global_options: usize,
    report: ImportReport,
}

impl Scanner {
    /// Scan one file's text starting in `context`; returns the context at
    /// its end.
    fn scan(&mut self, text: &str, mut context: Context) -> Context {
        for line in logical_lines(text) {
            let Some((key, args)) = split_line(&line) else {
                continue;
            };
            match key.as_str() {
                "host" => context = self.host_line(&args),
                "match" => {
                    self.match_blocks += 1;
                    context = Context::Match;
                }
                _ => match &context {
                    Context::Match => {}
                    _ if key == "include" => self.include(&args, &context),
                    Context::Global => {
                        if MAPPED_KEYS.contains(&key.as_str()) {
                            self.global_options += 1;
                        }
                    }
                    Context::Hosts(targets) => {
                        for &index in targets {
                            self.apply(index, &key, &args);
                        }
                    }
                },
            }
        }
        context
    }

    fn host_line(&mut self, patterns: &[String]) -> Context {
        if patterns.is_empty() {
            self.report
                .warnings
                .push("a Host line with no pattern was ignored".to_string());
        }
        let mut targets = Vec::new();
        for pattern in patterns {
            if pattern.starts_with('!') {
                self.report.skipped += 1;
                continue;
            }
            let index = match self.by_name.get(pattern) {
                Some(&index) => index,
                None => {
                    if pattern.contains(['*', '?']) {
                        self.report.templates += 1;
                    }
                    self.sessions.push(SessionConfig {
                        name: pattern.clone(),
                        ..SessionConfig::default()
                    });
                    self.proxy_jump.push(None);
                    self.by_name
                        .insert(pattern.clone(), self.sessions.len() - 1);
                    self.sessions.len() - 1
                }
            };
            targets.push(index);
        }
        Context::Hosts(targets)
    }

    /// Set one option on a session unless an earlier value is in place.
    fn apply(&mut self, index: usize, key: &str, args: &[String]) {
        let Some(value) = args.first() else {
            return;
        };
        let home = self.home.clone();
        let session = &mut self.sessions[index];
        match key {
            "hostname" => {
                // A template's HostName usually holds %h tokens: no host.
                if session.host.is_none() && !session.name.contains(['*', '?']) {
                    session.host = Some(value.clone());
                }
            }
            "user" => {
                session.username.get_or_insert_with(|| value.clone());
            }
            "port" => {
                if session.port.is_none() {
                    match value.parse::<u16>() {
                        Ok(port) => session.port = Some(port),
                        Err(_) => self.report.warnings.push(format!(
                            "{}: Port \"{value}\" is not a port number, ignored",
                            session.name
                        )),
                    }
                }
            }
            "identityfile" => {
                if session.key_path.is_none() && !value.eq_ignore_ascii_case("none") {
                    session.key_path = Some(expand_tilde(value, home.as_deref()));
                }
            }
            "identitiesonly" => {
                if session.auth_method.is_none() && value.eq_ignore_ascii_case("yes") {
                    session.auth_method = Some(AuthMethod::Key);
                }
            }
            "serveraliveinterval" => {
                if session.ssh_options.is_none() {
                    match value.parse::<u32>() {
                        Ok(interval) => {
                            session.ssh_options = Some(SshOptions {
                                compression: false,
                                keepalive_interval: Some(interval),
                                keepalive_count: None,
                                ssh_w_fallback: false,
                            })
                        }
                        Err(_) => self.report.warnings.push(format!(
                            "{}: ServerAliveInterval \"{value}\" is not a number, ignored",
                            session.name
                        )),
                    }
                }
            }
            "proxyjump" => {
                self.proxy_jump[index].get_or_insert_with(|| value.clone());
            }
            _ => {}
        }
    }

    /// Follow an `Include` line: each argument is a path or a file-name
    /// glob, read in place with the including block's context.
    fn include(&mut self, args: &[String], context: &Context) {
        for arg in args {
            let Some(path) = self.resolve_include(arg) else {
                continue;
            };
            for file in self.expand_glob(&path) {
                self.include_file(&file, context.clone());
            }
        }
    }

    fn resolve_include(&mut self, arg: &str) -> Option<PathBuf> {
        if let Some(rest) = arg.strip_prefix("~/") {
            match &self.home {
                Some(home) => return Some(home.join(rest)),
                None => {
                    self.report
                        .warnings
                        .push(format!("Include {arg}: HOME is not set, skipped"));
                    return None;
                }
            }
        }
        let path = Path::new(arg);
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        match &self.base_dir {
            Some(base) => Some(base.join(path)),
            None => {
                self.report.warnings.push(format!(
                    "Include {arg}: relative path with no base directory, skipped"
                ));
                None
            }
        }
    }

    /// The files `path` names: itself, or — when its file name holds `*`
    /// or `?` — the matching entries of its directory, sorted. A matchless
    /// glob names nothing, as in OpenSSH.
    fn expand_glob(&mut self, path: &Path) -> Vec<PathBuf> {
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !file_name.contains(['*', '?']) {
            return vec![path.to_path_buf()];
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        if dir.to_string_lossy().contains(['*', '?']) {
            self.report.warnings.push(format!(
                "Include {}: wildcards in directory names are not supported, skipped",
                path.display()
            ));
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut matches: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| wildcard_match(file_name, n))
            })
            .map(|e| e.path())
            .collect();
        matches.sort();
        matches
    }

    fn include_file(&mut self, path: &Path, context: Context) {
        let shown = path.display();
        if self.include_stack.len() >= MAX_INCLUDE_DEPTH {
            self.report.warnings.push(format!(
                "Include {shown}: nested deeper than {MAX_INCLUDE_DEPTH}, skipped"
            ));
            return;
        }
        let canonical = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) => {
                self.report
                    .warnings
                    .push(format!("Include {shown}: file not found, skipped"));
                return;
            }
        };
        if self.include_stack.contains(&canonical) {
            self.report
                .warnings
                .push(format!("Include {shown}: include cycle, skipped"));
            return;
        }
        let text = match read_source(&canonical) {
            Ok(text) => text,
            Err(e) => {
                self.report.warnings.push(format!("Include: {e}, skipped"));
                return;
            }
        };
        self.include_stack.push(canonical);
        // The included file's Host lines end at its end; the including
        // block resumes afterwards.
        self.scan(&text, context);
        self.include_stack.pop();
    }

    fn finish(mut self) -> Parsed {
        for index in 0..self.sessions.len() {
            let Some(target) = self.proxy_jump[index].take() else {
                continue;
            };
            if target.eq_ignore_ascii_case("none") {
                continue;
            }
            let name = &self.sessions[index].name;
            if target.contains(',') {
                self.report.warnings.push(format!(
                    "{name}: ProxyJump {target} has several hops (one is supported), dropped"
                ));
                continue;
            }
            match self.by_name.get(&target) {
                Some(&peer) if peer != index && !target.contains(['*', '?']) => {
                    self.sessions[index].jump_session_id = Some(peer as i64);
                }
                _ => self.report.warnings.push(format!(
                    "{name}: ProxyJump {target} is not a Host in this import, dropped"
                )),
            }
        }
        if self.match_blocks > 0 {
            self.report
                .warnings
                .push(format!("{} Match blocks skipped", self.match_blocks));
        }
        if self.global_options > 0 {
            self.report.warnings.push(format!(
                "{} option(s) outside any Host block not imported",
                self.global_options
            ));
        }
        (Vec::new(), self.sessions, self.report)
    }
}

/// The keywords [`Scanner::apply`] maps.
const MAPPED_KEYS: [&str; 7] = [
    "hostname",
    "user",
    "port",
    "identityfile",
    "identitiesonly",
    "serveraliveinterval",
    "proxyjump",
];

/// Physical lines with backslash continuations joined.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        match line.strip_suffix('\\') {
            Some(head) => {
                pending.push_str(head);
                pending.push(' ');
            }
            None => {
                pending.push_str(line);
                lines.push(std::mem::take(&mut pending));
            }
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }
    lines
}

/// `Key value…` or `Key=value…` → (lowercased key, arguments). `None` for
/// blank and comment lines.
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let key_end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (key, rest) = line.split_at(key_end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    Some((key.to_ascii_lowercase(), split_args(rest)))
}

/// Whitespace-separated arguments; `"…"` or `'…'` quote spaces; `#`
/// outside quotes starts a comment.
fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_arg = true;
            }
            None if c == '#' => break,
            None if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            None => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    args
}

fn expand_tilde(path: &str, home: Option<&Path>) -> String {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

/// Glob match of `*` and `?` against a whole name.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_line_handles_separators_quotes_and_comments() {
        type Case<'a> = (&'a str, Option<(&'a str, &'a [&'a str])>);
        let cases: &[Case] = &[
            ("", None),
            ("   # a comment", None),
            ("HostName example.com", Some(("hostname", &["example.com"]))),
            ("Port=2222", Some(("port", &["2222"]))),
            ("Port = 2222", Some(("port", &["2222"]))),
            ("  USER\tadmin # trailing", Some(("user", &["admin"]))),
            (
                "IdentityFile \"/keys/my key\"",
                Some(("identityfile", &["/keys/my key"])),
            ),
            ("Host a b  c", Some(("host", &["a", "b", "c"]))),
            ("User 'x # y'", Some(("user", &["x # y"]))),
            ("Host", Some(("host", &[]))),
        ];
        for (line, want) in cases {
            let got = split_line(line);
            let want = want.map(|(k, a)| {
                (
                    k.to_string(),
                    a.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                )
            });
            assert_eq!(got, want, "line {line:?}");
        }
    }

    #[test]
    fn continuation_lines_join() {
        assert_eq!(
            logical_lines("Host a \\\n  b\nUser x\n"),
            vec!["Host a    b".to_string(), "User x".to_string()]
        );
    }

    #[test]
    fn wildcard_match_table() {
        let cases = [
            ("*", "anything", true),
            ("*.conf", "a.conf", true),
            ("*.conf", "a.conf.bak", false),
            ("h?st", "host", true),
            ("h?st", "hst", false),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
        ];
        for (pattern, name, want) in cases {
            assert_eq!(wildcard_match(pattern, name), want, "{pattern} vs {name}");
        }
    }

    #[test]
    fn tilde_expands_against_home() {
        let home = Path::new("/home/u");
        assert_eq!(expand_tilde("~/.ssh/id", Some(home)), "/home/u/.ssh/id");
        assert_eq!(expand_tilde("/abs/id", Some(home)), "/abs/id");
        assert_eq!(expand_tilde("~/.ssh/id", None), "~/.ssh/id");
    }
}
