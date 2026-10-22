use std::cell::RefCell;
use std::path::PathBuf;
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

/// `$XDG_DATA_HOME/wayterm/wayterm.sqlite3`, creating the directory if
/// needed.
pub fn default_db_path() -> PathBuf {
    let dir = data_dir();
    // Best effort: opening the store reports an unusable path.
    let _ = std::fs::create_dir_all(&dir);
    dir.join("wayterm.sqlite3")
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
