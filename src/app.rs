use std::path::PathBuf;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::MainWindow;

pub const APP_ID: &str = "com.alex-eckstein.wayterm";

pub fn run() -> glib::ExitCode {
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

/// Return the default SQLite path following XDG data-home semantics.
///
/// Resolves `$XDG_DATA_HOME/wayterm/wayterm.sqlite3`, falling back to
/// `$HOME/.local/share/wayterm/wayterm.sqlite3`. Creates the parent
/// directory if it does not exist.
pub fn default_db_path() -> PathBuf {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".local");
                path.push("share");
                path
            })
        })
        .unwrap_or_else(|| PathBuf::from("."));

    let mut path = data_home;
    path.push("wayterm");
    // Best-effort directory creation; SessionStore::open will fail
    // explicitly if the path is still unusable.
    let _ = std::fs::create_dir_all(&path);
    path.push("wayterm.sqlite3");
    path
}

fn build_ui(app: &gtk4::Application) {
    let db_path = default_db_path();
    let store = SessionStore::open(&db_path).expect("failed to open session store");
    MainWindow::new(app, store);
}
