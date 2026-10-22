//! The Backup / Restore settings page (doc 10): one
//! row per import source with a file picker, and an Export button. The
//! work is `crate::transfer`'s; this page picks files, confirms, and
//! reports. The page is also offered once on first run
//! ([`take_first_run_offer`]).

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};

use super::{boxed_list, described, heading, row, scrolling_page};
use crate::store::SessionStore;
use crate::transfer::{
    apply_import, export_json, parse_source, read_source, ImportError, ImportReport, ImportSource,
    Parsed,
};
use crate::ui::widgets::confirm;

/// Settings row: set once the first-run import offer has been shown.
pub const IMPORT_OFFERED_KEY: &str = "import_offered";

/// Warnings listed in the status line before the rest become "…".
const SHOWN_WARNINGS: usize = 3;

/// The sources, in the order the page lists them (doc 10 priority).
const SOURCES: [(ImportSource, &str); 5] = [
    (ImportSource::SshConfig, "OpenSSH config"),
    (ImportSource::Remmina, "Remmina (.remmina)"),
    (
        ImportSource::MobaXterm,
        "MobaXterm export (.mxtsessions / .ini)",
    ),
    (ImportSource::Asbru, "Asbru-CM YAML"),
    (ImportSource::Wayterm, "Wayterm JSON export"),
];

/// `$HOME`, or `None` when unset.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/.ssh`, where `~/.ssh/config`'s relative `Include`s resolve.
fn ssh_dir() -> Option<PathBuf> {
    home().map(|home| home.join(".ssh"))
}

/// Read and parse `path` as `source`. Relative `Include`s resolve against
/// the file's own directory, falling back to `~/.ssh`.
fn parse_file(source: ImportSource, path: &Path) -> Result<Parsed, ImportError> {
    let text = read_source(path)?;
    let base_dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(ssh_dir);
    parse_source(source, &text, base_dir.as_deref())
}

/// `a; b; c; …` — at most [`SHOWN_WARNINGS`] warnings.
fn warnings_text(warnings: &[String]) -> String {
    let mut text = warnings
        .iter()
        .take(SHOWN_WARNINGS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    if warnings.len() > SHOWN_WARNINGS {
        text.push_str("; …");
    }
    text
}

/// What a parse found, for the confirmation before writing.
fn parsed_summary(sessions: usize, report: &ImportReport) -> String {
    format!(
        "{sessions} session(s) to import ({} template(s)), {} skipped, {} warning(s).",
        report.templates,
        report.skipped,
        report.warnings.len()
    )
}

/// The status line after an import.
fn final_summary(report: &ImportReport) -> String {
    let mut text = format!(
        "Imported {} session(s) ({} template(s)), skipped {}.",
        report.imported, report.templates, report.skipped
    );
    if !report.warnings.is_empty() {
        text.push_str(&format!(" Warnings: {}", warnings_text(&report.warnings)));
    }
    text
}

/// Whether the first-run import offer is due: nothing in the store and
/// never offered before.
fn first_run_offer_due(store: &SessionStore) -> rusqlite::Result<bool> {
    Ok(store.get_setting(IMPORT_OFFERED_KEY)?.is_none()
        && store.list_sessions(None)?.is_empty()
        && store.list_folders(None)?.is_empty())
}

/// On first run (empty store, never offered), mark the offer made and
/// return `true`: the caller then opens this page. Call after the main
/// window is presented.
pub fn take_first_run_offer(store: &SessionStore) -> bool {
    match first_run_offer_due(store) {
        Ok(false) => false,
        Ok(true) => {
            // Offered = shown, whatever the user does next: never nag.
            if let Err(err) = store.set_setting(IMPORT_OFFERED_KEY, "1") {
                eprintln!("Wayterm: failed to record the import offer: {err}");
            }
            true
        }
        Err(err) => {
            eprintln!("Wayterm: failed to check for a first run: {err}");
            false
        }
    }
}

/// The page. `on_import` runs after each import is written (the main
/// window refreshes its session tree).
pub fn page(store: Rc<SessionStore>, on_import: Rc<dyn Fn()>) -> gtk4::Widget {
    let status = gtk4::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_selectable(true);

    // The column the page is built in; its window is the dialogs' parent.
    let anchor = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let ctx = Ctx {
        store,
        on_import,
        anchor: anchor.clone().upcast(),
        status: status.clone(),
    };

    let sources = boxed_list();
    for (source, label) in SOURCES {
        let choose = gtk4::Button::with_label("Choose File…");
        let for_choose = ctx.clone();
        choose.connect_clicked(move |_| for_choose.choose_and_import(source, label));
        let text = described(label, None, &choose);
        let mut children: Vec<gtk4::Widget> = vec![text.upcast()];
        if source == ImportSource::SshConfig {
            if let Some(path) = ssh_dir().map(|dir| dir.join("config")) {
                let default = gtk4::Button::with_label("Use ~/.ssh/config");
                let ctx = ctx.clone();
                default.connect_clicked(move |_| ctx.import(source, &path));
                children.push(default.upcast());
            }
        }
        children.push(choose.upcast());
        let children: Vec<&gtk4::Widget> = children.iter().collect();
        sources.append(&row(&children));
    }

    let export_button = gtk4::Button::with_label("Export…");
    export_button.connect_clicked(move |_| ctx.export());
    let export_text = described(
        "Export all sessions as JSON",
        Some("Folders and sessions, without credentials."),
        &export_button,
    );
    let backup = boxed_list();
    backup.append(&row(&[
        export_text.upcast_ref(),
        export_button.upcast_ref(),
    ]));

    let restore_heading = heading("Restore");
    let restore_hint = gtk4::Label::new(Some("Import sessions from another client or a backup."));
    restore_hint.set_xalign(0.0);
    restore_hint.add_css_class("dim-label");
    for child in [
        heading("Backup").upcast_ref::<gtk4::Widget>(),
        backup.upcast_ref(),
        restore_heading.upcast_ref(),
        restore_hint.upcast_ref(),
        sources.upcast_ref(),
        status.upcast_ref(),
    ] {
        anchor.append(child);
    }
    anchor.set_spacing(6);
    scrolling_page(&[anchor.upcast_ref()])
}

/// What the page's buttons act on.
#[derive(Clone)]
struct Ctx {
    store: Rc<SessionStore>,
    on_import: Rc<dyn Fn()>,
    anchor: gtk4::Widget,
    status: gtk4::Label,
}

impl Ctx {
    /// The settings window the page is in, the dialogs' parent.
    fn window(&self) -> Option<gtk4::Window> {
        self.anchor.root().and_downcast()
    }

    fn choose_and_import(&self, source: ImportSource, label: &str) {
        let dialog = gtk4::FileDialog::builder()
            .title(format!("Import from {label}"))
            .modal(true)
            .build();
        if source == ImportSource::SshConfig {
            if let Some(dir) = ssh_dir() {
                dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
            }
        }
        let ctx = self.clone();
        dialog.open(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            move |file| {
                if let Some(path) = picked_path(file) {
                    ctx.import(source, &path);
                }
            },
        );
    }

    /// Parse, confirm, then write and report.
    fn import(&self, source: ImportSource, path: &Path) {
        let (folders, sessions, report) = match parse_file(source, path) {
            Ok(parsed) => parsed,
            Err(err) => return self.status.set_text(&err.to_string()),
        };
        if sessions.is_empty() {
            let mut text = format!("Nothing to import from {}.", path.display());
            if !report.warnings.is_empty() {
                text.push_str(&format!(" Warnings: {}", warnings_text(&report.warnings)));
            }
            return self.status.set_text(&text);
        }
        let detail = parsed_summary(sessions.len(), &report);
        let ctx = self.clone();
        confirm(
            self.window().as_ref(),
            &format!("Import from {}?", path.display()),
            &detail,
            "Import",
            move || match apply_import(&ctx.store, folders, sessions, report) {
                Ok(report) => {
                    (ctx.on_import)();
                    ctx.status.set_text(&final_summary(&report));
                }
                Err(err) => ctx.status.set_text(&format!("Import failed: {err}")),
            },
        );
    }

    fn export(&self) {
        let json = match export_json(&self.store) {
            Ok(json) => json,
            Err(err) => return self.status.set_text(&format!("Export failed: {err}")),
        };
        let dialog = gtk4::FileDialog::builder()
            .title("Export Sessions")
            .initial_name("wayterm-sessions.json")
            .modal(true)
            .build();
        let status = self.status.clone();
        dialog.save(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            move |file| {
                let Some(path) = picked_path(file) else {
                    return;
                };
                match write_private(&path, json.as_bytes()) {
                    Ok(()) => status.set_text(&format!("Exported to {}.", path.display())),
                    Err(err) => {
                        status.set_text(&format!("Export to {} failed: {err}", path.display()))
                    }
                }
            },
        );
    }
}

/// The local path of a file-dialog answer; `None` when cancelled.
fn picked_path(file: Result<gio::File, glib::Error>) -> Option<PathBuf> {
    match file {
        Ok(file) => file.path(),
        Err(err) if err.matches(gtk4::DialogError::Dismissed) => None,
        Err(err) => {
            eprintln!("Wayterm: file dialog failed: {err}");
            None
        }
    }
}

/// Write `bytes` to `path` readable by its owner only (0o600, also when
/// overwriting a file that had wider permissions).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picked `~/.ssh/config` follows its `Include` relative to its own
    /// directory, and wildcard hosts count as templates.
    #[test]
    fn parse_file_reads_ssh_config_with_includes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config"),
            "Include extra\nHost *.lab\n  User ops\nHost web\n  HostName web.example\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("extra"), "Host db\n  Port 2222\n").unwrap();

        let (folders, sessions, report) =
            parse_file(ImportSource::SshConfig, &dir.path().join("config")).unwrap();
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["db", "*.lab", "web"]);
        assert_eq!((folders.len(), report.templates), (0, 1));
    }

    #[test]
    fn warnings_are_truncated_after_three() {
        let warnings: Vec<String> = (1..=5).map(|i| format!("w{i}")).collect();
        assert_eq!(warnings_text(&warnings), "w1; w2; w3; …");
        assert_eq!(warnings_text(&warnings[..2]), "w1; w2");
    }

    #[test]
    fn first_run_offer_needs_an_empty_store_and_no_earlier_offer() {
        let store = SessionStore::in_memory().unwrap();
        assert!(first_run_offer_due(&store).unwrap());
        store.insert_folder("A", None).unwrap();
        assert!(!first_run_offer_due(&store).unwrap());

        let store = SessionStore::in_memory().unwrap();
        store.set_setting(IMPORT_OFFERED_KEY, "1").unwrap();
        assert!(!first_run_offer_due(&store).unwrap());
    }

    #[test]
    fn export_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{}").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }
}
