//! Session import and export (doc 10).
//!
//! Importers are pure readers: source text in, `(folders, sessions,
//! report)` out — no store access, no network, no exec, never a key file.
//! Only `~/.ssh/config`'s `Include` chain is read from disk. [`apply_import`]
//! is the one place an import writes to the store. No secret ever lands in
//! a [`SessionConfig`]; secret fields in a source become warnings.
//!
//! Parsed sessions refer to each other by position, not by id: a parsed
//! session's `folder_id` is an index into the returned folders and its
//! `jump_session_id` an index into the returned sessions. [`apply_import`]
//! remaps both to the ids the store assigns.

mod asbru;
mod json;
mod mobaxterm;
mod remmina;
mod ssh_config;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::store::model::SessionConfig;
use crate::store::SessionStore;

pub use json::export_json;

/// Where an import's text comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportSource {
    /// `~/.ssh/config` (its `Include`s are followed).
    SshConfig,
    /// One `.remmina` file.
    Remmina,
    /// A MobaXterm `.ini` / `.mxtsessions` export.
    MobaXterm,
    /// An Asbru-CM YAML export.
    Asbru,
    /// Wayterm's own JSON export ([`export_json`]).
    Wayterm,
}

/// What an import found and did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportReport {
    /// Sessions written to the store (templates included).
    pub imported: usize,
    /// Source entries that never became a session (negated patterns,
    /// unsupported protocols).
    pub skipped: usize,
    /// Template sessions (from wildcard `Host` patterns).
    pub templates: usize,
    pub warnings: Vec<String>,
}

/// A folder to create. `parent` is the parent's path — its ancestors'
/// names and its own, joined with `/` — or `None` for the root. Folders
/// come parent-first; a path names the nearest earlier folder with that
/// path, so duplicate sibling names stay apart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderSpec {
    pub name: String,
    pub parent: Option<String>,
    pub icon: Option<String>,
}

impl FolderSpec {
    /// This folder's path, the form a child's `parent` uses.
    pub fn path(&self) -> String {
        match &self.parent {
            Some(parent) => format!("{parent}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot parse the import: {0}")]
    Parse(String),
    #[error("export version {0} is newer than this Wayterm understands (1)")]
    UnsupportedVersion(u32),
    #[error("cannot read the import: {0}")]
    Io(String),
}

/// Parsed folders and sessions, plus what parsing skipped or warned about.
pub type Parsed = (Vec<FolderSpec>, Vec<SessionConfig>, ImportReport);

/// Parse `text` from `source`. `base_dir` resolves `~/.ssh/config`'s
/// relative `Include`s (normally `~/.ssh`); other sources ignore it.
pub fn parse_source(
    source: ImportSource,
    text: &str,
    base_dir: Option<&Path>,
) -> Result<Parsed, ImportError> {
    match source {
        ImportSource::SshConfig => Ok(ssh_config::parse(text, base_dir)),
        ImportSource::Remmina => Ok(remmina::parse(text)),
        ImportSource::MobaXterm => Ok(mobaxterm::parse(text)),
        ImportSource::Asbru => asbru::parse(text),
        ImportSource::Wayterm => json::parse(text),
    }
}

/// Read an import source file. Only regular files are read (a symlink to
/// one is fine); a FIFO is opened non-blocking and refused, never waited
/// on.
pub fn read_source(path: &Path) -> Result<String, ImportError> {
    let io_err = |e: std::io::Error| ImportError::Io(format!("{}: {e}", path.display()));
    let mut file: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(io_err)?;
    if !file.metadata().map_err(io_err)?.is_file() {
        return Err(ImportError::Io(format!(
            "{}: not a regular file",
            path.display()
        )));
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io_err)?;
    Ok(text)
}

/// Write a parse result to `store`: folders parent-first, then sessions
/// with their folder and jump-host positions remapped to store ids.
/// `report` is the parse's; the returned one adds the import count and
/// any warnings from writing.
pub fn apply_import(
    store: &SessionStore,
    folders: Vec<FolderSpec>,
    sessions: Vec<SessionConfig>,
    mut report: ImportReport,
) -> rusqlite::Result<ImportReport> {
    let mut by_path: HashMap<String, i64> = HashMap::new();
    let mut folder_ids = Vec::with_capacity(folders.len());
    for spec in &folders {
        let parent_id = match &spec.parent {
            Some(parent) => {
                let id = by_path.get(parent).copied();
                if id.is_none() {
                    report.warnings.push(format!(
                        "folder \"{}\": parent \"{parent}\" not found, created at the root",
                        spec.name
                    ));
                }
                id
            }
            None => None,
        };
        let id = store.insert_folder(&spec.name, parent_id)?;
        if let Some(icon) = &spec.icon {
            store.update_folder(id, &spec.name, Some(icon))?;
        }
        by_path.insert(spec.path(), id);
        folder_ids.push(id);
    }

    // Jump targets go in first so their ids exist; one hop only (doc 04),
    // so a target that itself jumps is never a target.
    let mut session_ids: Vec<Option<i64>> = vec![None; sessions.len()];
    let (direct, jumping): (Vec<usize>, Vec<usize>) =
        (0..sessions.len()).partition(|&i| sessions[i].jump_session_id.is_none());
    for index in direct.into_iter().chain(jumping) {
        let mut config = sessions[index].clone();
        config.id = 0;
        config.folder_id = config.folder_id.and_then(|f| {
            let id = usize::try_from(f)
                .ok()
                .and_then(|f| folder_ids.get(f).copied());
            if id.is_none() {
                report.warnings.push(format!(
                    "session \"{}\": folder not found, imported at the root",
                    config.name
                ));
            }
            id
        });
        config.jump_session_id = config.jump_session_id.and_then(|j| {
            let target = usize::try_from(j).ok();
            let id = target
                .filter(|&t| sessions.get(t).is_some_and(|s| s.jump_session_id.is_none()))
                .and_then(|t| session_ids.get(t).copied().flatten());
            if id.is_none() {
                let reason = match target.and_then(|t| sessions.get(t)) {
                    Some(peer) => format!("\"{}\" itself jumps (one hop only)", peer.name),
                    None => "target not found".to_string(),
                };
                report.warnings.push(format!(
                    "session \"{}\": jump host dropped — {reason}",
                    config.name
                ));
            }
            id
        });
        session_ids[index] = Some(store.insert_session(&config)?);
        report.imported += 1;
    }
    Ok(report)
}

/// Folders built from path strings (Remmina groups, MobaXterm `SubRep`,
/// Asbru groups): each distinct path becomes one folder.
#[derive(Default)]
struct FolderSet {
    folders: Vec<FolderSpec>,
    by_path: HashMap<String, usize>,
}

impl FolderSet {
    /// The index of the folder at `path` (creating it and its ancestors),
    /// or `None` for the root (an empty path). Blank segments are dropped.
    fn ensure<'a>(&mut self, path: impl IntoIterator<Item = &'a str>) -> Option<usize> {
        let mut parent: Option<String> = None;
        let mut index = None;
        for segment in path.into_iter().map(str::trim).filter(|s| !s.is_empty()) {
            let spec = FolderSpec {
                name: segment.to_string(),
                parent: parent.clone(),
                icon: None,
            };
            let key = spec.path();
            let next = self.folders.len();
            let i = *self.by_path.entry(key.clone()).or_insert(next);
            if i == next {
                self.folders.push(spec);
            }
            index = Some(i);
            parent = Some(key);
        }
        index
    }
}

/// `folder_id` holding a folder position (see the module docs).
fn folder_ref(index: Option<usize>) -> Option<i64> {
    index.map(|i| i as i64)
}

/// Split `[user@]host[:port]` (IPv6 as `[addr]:port`). A port that does
/// not parse is returned as `Err` with the full text.
fn split_server(server: &str) -> (Option<String>, String, Result<Option<u16>, String>) {
    let (user, rest) = match server.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_string()), rest),
        Some((_, rest)) => (None, rest),
        None => (None, server),
    };
    let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':')),
            None => (bracketed, None),
        }
    } else {
        match rest.split_once(':') {
            // More than one colon: a bare IPv6 address, no port.
            Some((_, tail)) if tail.contains(':') => (rest, None),
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    let port = match port {
        None | Some("") => Ok(None),
        Some(p) => p.parse().map(Some).map_err(|_| server.to_string()),
    };
    (user, host.to_string(), port)
}

#[cfg(test)]
mod tests;
