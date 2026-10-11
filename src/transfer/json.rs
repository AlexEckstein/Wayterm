//! Wayterm's own JSON export (Phase 9 decision 3):
//! `{"version":1,"folders":[{"name","icon","children":[…],"sessions":[…]}]}`.
//! Folders own their sessions by nesting; sessions never carry ids. A
//! jump host is named by `jump_session`. Root sessions, which have no
//! folder, go in a top-level `sessions` array. No secret is ever in it:
//! `SessionConfig` holds none.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{FolderSpec, ImportError, ImportReport, Parsed};
use crate::store::model::SessionConfig;
use crate::store::SessionStore;

/// Export document schema version.
pub const EXPORT_VERSION: u32 = 1;

/// Store-local fields a session export leaves out.
const LOCAL_FIELDS: [&str; 5] = [
    "id",
    "folder_id",
    "jump_session_id",
    "last_connected",
    "connect_count",
];

#[derive(Serialize)]
struct ExportDoc {
    version: u32,
    folders: Vec<ExportFolder>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<Map<String, Value>>,
}

#[derive(Serialize)]
struct ExportFolder {
    name: String,
    icon: Option<String>,
    children: Vec<ExportFolder>,
    sessions: Vec<Map<String, Value>>,
}

/// Every folder and session in `store`, as an export document.
pub fn export_json(store: &SessionStore) -> serde_json::Result<String> {
    let doc = ExportDoc {
        version: EXPORT_VERSION,
        folders: export_folders(store, None)?,
        sessions: export_sessions(store, None)?,
    };
    serde_json::to_string_pretty(&doc)
}

fn export_folders(
    store: &SessionStore,
    parent: Option<i64>,
) -> serde_json::Result<Vec<ExportFolder>> {
    store
        .list_folders(parent)
        .map_err(store_error)?
        .into_iter()
        .map(|folder| {
            Ok(ExportFolder {
                children: export_folders(store, Some(folder.id))?,
                sessions: export_sessions(store, Some(folder.id))?,
                name: folder.name,
                icon: folder.icon,
            })
        })
        .collect()
}

fn export_sessions(
    store: &SessionStore,
    folder: Option<i64>,
) -> serde_json::Result<Vec<Map<String, Value>>> {
    let sessions = store.list_sessions(folder).map_err(store_error)?;
    sessions
        .into_iter()
        .map(|session| {
            let jump = session
                .jump_session_id
                .and_then(|id| store.get_session(id).ok())
                .map(|peer| peer.name);
            let Value::Object(mut fields) = serde_json::to_value(&session)? else {
                unreachable!("SessionConfig serializes to an object");
            };
            for key in LOCAL_FIELDS {
                fields.remove(key);
            }
            if let Some(jump) = jump {
                fields.insert("jump_session".to_string(), Value::String(jump));
            }
            Ok(fields)
        })
        .collect()
}

/// A store read failing mid-export surfaces as a serialization error.
fn store_error(e: rusqlite::Error) -> serde_json::Error {
    serde::ser::Error::custom(e)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ImportDoc {
    version: u32,
    folders: Vec<ImportFolder>,
    sessions: Vec<ImportSession>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ImportFolder {
    name: String,
    icon: Option<String>,
    children: Vec<ImportFolder>,
    sessions: Vec<ImportSession>,
}

#[derive(Deserialize)]
struct ImportSession {
    #[serde(flatten)]
    config: SessionConfig,
    #[serde(default)]
    jump_session: Option<String>,
}

/// Parse an export document. A version above [`EXPORT_VERSION`] is an
/// error; unknown fields are ignored.
pub fn parse(text: &str) -> Result<Parsed, ImportError> {
    let doc: ImportDoc =
        serde_json::from_str(text).map_err(|e| ImportError::Parse(e.to_string()))?;
    if doc.version > EXPORT_VERSION {
        return Err(ImportError::UnsupportedVersion(doc.version));
    }
    let mut out = Collector::default();
    out.sessions(doc.sessions, None);
    for folder in doc.folders {
        out.folder(folder, None);
    }
    Ok(out.finish())
}

#[derive(Default)]
struct Collector {
    folders: Vec<FolderSpec>,
    sessions: Vec<SessionConfig>,
    jumps: Vec<Option<String>>,
}

impl Collector {
    fn folder(&mut self, folder: ImportFolder, parent: Option<String>) {
        let spec = FolderSpec {
            name: folder.name,
            parent,
            icon: folder.icon,
        };
        let path = spec.path();
        self.folders.push(spec);
        self.sessions(folder.sessions, Some(self.folders.len() - 1));
        for child in folder.children {
            self.folder(child, Some(path.clone()));
        }
    }

    fn sessions(&mut self, sessions: Vec<ImportSession>, folder: Option<usize>) {
        for ImportSession {
            mut config,
            jump_session,
        } in sessions
        {
            config.id = 0;
            config.folder_id = super::folder_ref(folder);
            config.jump_session_id = None;
            self.sessions.push(config);
            self.jumps.push(jump_session);
        }
    }

    /// Resolve `jump_session` names to positions (first session of that
    /// name wins).
    fn finish(mut self) -> Parsed {
        let mut report = ImportReport::default();
        let mut by_name = HashMap::new();
        for (index, session) in self.sessions.iter().enumerate() {
            by_name.entry(session.name.clone()).or_insert(index);
        }
        for (index, jump) in self.jumps.into_iter().enumerate() {
            let Some(jump) = jump else { continue };
            match by_name.get(&jump) {
                Some(&peer) => self.sessions[index].jump_session_id = Some(peer as i64),
                None => report.warnings.push(format!(
                    "{}: jump session \"{jump}\" is not in this import, dropped",
                    self.sessions[index].name
                )),
            }
        }
        (self.folders, self.sessions, report)
    }
}
