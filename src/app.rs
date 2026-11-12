use std::cell::RefCell;
use std::fs::{DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::hotkeys::HotkeyEngine;
use crate::store::SessionStore;
use crate::ui::{tray, MainWindow};

pub const APP_ID: &str = "com.alex-eckstein.wayterm";

pub fn run() -> glib::ExitCode {
    install_tokio_runtime();
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    // Local copies of files opened from an SFTP sidebar (doc 07 §Opening
    // files) do not outlive the run, nor a run that was killed.
    app.connect_startup(|_| crate::ui::panels::sftp_sidebar::discard_edit_copies());
    app.connect_shutdown(|_| crate::ui::panels::sftp_sidebar::discard_edit_copies());
    app.connect_activate(build_ui);
    app.run()
}

/// Enter a process-wide multi-threaded tokio runtime on the GTK main
/// thread, so `tokio::spawn` works from GTK callbacks. Both the runtime and
/// its guard are leaked on purpose: they must outlive every connection.
fn install_tokio_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    std::mem::forget(runtime.enter());
    std::mem::forget(runtime);
}

/// `$XDG_DATA_HOME/wayterm` (default `~/.local/share/wayterm`).
pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wayterm")
}

/// `$XDG_DATA_HOME/wayterm/wayterm.sqlite3`, in a directory made, and
/// kept, private to its owner.
pub fn default_db_path() -> PathBuf {
    let dir = data_dir();
    // Best effort: opening the store reports an unusable path.
    let _ = private_dir(&dir);
    dir.join("wayterm.sqlite3")
}

/// Create `dir` if it is missing and set it to mode 0700, one made by an
/// earlier version included: the store in it names hosts and users, and
/// its macros can hold typed passwords (doc 09 §Database).
fn private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
}

fn build_ui(app: &gtk4::Application) {
    if let Some(display) = gtk4::gdk::Display::default() {
        crate::ui::icons::register_icons(&display);
    }
    // Every window's title bar (and the compositor's, via the toplevel
    // icon) shows the app icon; `register_icons` embeds it.
    gtk4::Window::set_default_icon_name(APP_ID);
    let store =
        Rc::new(SessionStore::open(&default_db_path()).expect("failed to open session store"));

    // Compiled-in defaults first, then the user's customized rows (doc 11).
    let mut hotkeys = HotkeyEngine::new();
    match hotkeys.load_from_db(&store) {
        Ok(report) if !report.applied.is_empty() || !report.skipped.is_empty() => eprintln!(
            "Wayterm: loaded {} custom hotkey(s), skipped {}{}",
            report.applied.len(),
            report.skipped.len(),
            if report.displaced.is_empty() {
                String::new()
            } else {
                format!(
                    "; unbound by a colliding chord: {}",
                    report.displaced.join(", ")
                )
            }
        ),
        Ok(_) => {}
        Err(err) => eprintln!("Wayterm: failed to load custom hotkeys: {err}"),
    }

    let main_window = MainWindow::new(app, store.clone(), Rc::new(RefCell::new(hotkeys)));
    let window = main_window.window();
    let tray_available = match tray::init_tray(app) {
        Ok(handle) => {
            // The handle keeps the D-Bus service alive with the window.
            crate::ui::widgets::set_data(window, "wayterm-tray-handle", handle);
            true
        }
        Err(err) => {
            // Headless or no tray host: closing the window quits.
            eprintln!("Wayterm: tray unavailable ({err}); close will quit");
            false
        }
    };
    tray::install_close_handler(window, tray_available, store);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_directory_is_private_new_or_not() {
        let mode = |dir: &Path| std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        let home = tempfile::tempdir().unwrap();

        let fresh = home.path().join("share/wayterm");
        private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);

        let older = home.path().join("older");
        std::fs::create_dir(&older).unwrap();
        std::fs::set_permissions(&older, Permissions::from_mode(0o755)).unwrap();
        private_dir(&older).unwrap();
        assert_eq!(mode(&older), 0o700);
    }
}
