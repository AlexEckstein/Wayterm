use std::path::PathBuf;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::ui::MainWindow;

pub const APP_ID: &str = "com.alex-eckstein.wayterm";

pub fn run() -> glib::ExitCode {
    install_tokio_runtime();
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

/// Install the process-wide tokio runtime used for SSH dial/auth and the
/// transport pump.
///
/// GTK owns the main thread, so long-running network work is spawned with
/// `tokio::spawn` and awaited from `glib::spawn_local` callbacks. Entering
/// the runtime on this thread makes `tokio::spawn` work from GTK callbacks;
/// both the runtime and its enter-guard are leaked deliberately so worker
/// threads and in-flight connections survive for the whole process
/// lifetime (dropping either would tear tasks down while GTK still runs).
fn install_tokio_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let entered = runtime.enter();
    std::mem::forget(entered);
    std::mem::forget(runtime);
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
