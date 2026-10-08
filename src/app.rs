use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::hotkeys::HotkeyEngine;
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

    // Hotkey persistence (doc 07 §"Persistence"): the engine starts from
    // the compiled-in defaults, then user-customized rows
    // (`WHERE is_default = 0`) load over the *current* bindings. Before
    // this task nothing called `load_from_db` — the engine-side tests
    // were its only caller — so startup wires it here.
    let engine = Rc::new(RefCell::new(HotkeyEngine::new()));
    match engine.borrow_mut().load_from_db(&store) {
        Ok(report) => {
            if !report.applied.is_empty() || !report.skipped.is_empty() {
                eprintln!(
                    "Wayterm: loaded {} custom hotkey(s), skipped {}",
                    report.applied.len(),
                    report.skipped.len()
                );
            }
        }
        Err(err) => eprintln!("Wayterm: failed to load custom hotkeys: {err}"),
    }
    // The settings window needs its own connection: `MainWindow::new`
    // takes `store` by value, and `SessionStore` hands out extra
    // connections via `duplicate`.
    let settings_store = match store.duplicate() {
        Ok(duplicate) => Rc::new(duplicate),
        Err(err) => {
            eprintln!("Wayterm: failed to duplicate store for hotkey settings: {err}");
            Rc::new(SessionStore::in_memory().expect("failed to create fallback store"))
        }
    };

    let main_window = MainWindow::new(app, store);
    // Appended after construction so P3-3's connect-flow hunks inside
    // `MainWindow::new` stay untouched.
    main_window.attach_hotkey_settings(Rc::clone(&engine), Rc::clone(&settings_store));
}
