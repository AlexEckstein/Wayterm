//! End-to-end hotkey persistence through the public API: rebind, save to a
//! real SQLite file, start a fresh engine, and get the layout back. The
//! per-rule coverage lives in the `hotkeys` unit tests.

use wayterm::hotkeys::{ConflictPolicy, HotkeyEngine, HotkeyExport};
use wayterm::store::db::SessionStore;

fn file_store(dir: &std::path::Path) -> SessionStore {
    SessionStore::open(&dir.join("wayterm.db")).expect("open the store")
}

fn bindings(engine: &HotkeyEngine) -> Vec<(String, String, bool)> {
    engine
        .registry()
        .ordered_actions()
        .into_iter()
        .map(|a| (a.id.clone(), a.current_key.clone(), a.enabled))
        .collect()
}

#[test]
fn override_then_save_then_load_keeps_the_layout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = file_store(dir.path());

    let mut first = HotkeyEngine::new();
    assert_eq!(
        first
            .rebind_with("paste", "Ctrl+Shift+C", ConflictPolicy::Override)
            .expect("override"),
        vec!["copy".to_string()]
    );
    first.rebind("new_tab", "Ctrl+Alt+T").expect("free chord");
    // Three rows: the two rebinds plus `copy`'s own unbound state, which
    // is what makes the override survive a restart.
    assert_eq!(first.save_to_db(&store).expect("save"), 3);

    let mut second = HotkeyEngine::new();
    let report = second.load_from_db(&store).expect("load");
    assert_eq!(report.applied.len(), 3);
    assert!(report.skipped.is_empty());
    assert_eq!(second.action("paste").unwrap().current_key, "Ctrl+Shift+C");
    assert_eq!(second.action("new_tab").unwrap().current_key, "Ctrl+Alt+T");
    assert_eq!(second.action("copy").unwrap().current_key, "");
    assert!(!second.action("copy").unwrap().enabled);
}

#[test]
fn a_layout_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = file_store(dir.path());

    let mut engine = HotkeyEngine::new();
    engine.rebind("copy", "Ctrl+Y").expect("rebind");
    engine.rebind("goto_tab_3", "Alt+F3").expect("rebind");
    engine.set_enabled("sftp_upload", false).expect("disable");
    let before = bindings(&engine);
    engine.save_to_db(&store).expect("save");

    // A fresh process: new registry, same database file.
    let mut restarted = HotkeyEngine::new();
    restarted.load_from_db(&store).expect("load");
    assert_eq!(bindings(&restarted), before);
    assert!(!restarted.action("sftp_upload").unwrap().enabled);
}

#[test]
fn an_export_document_is_bindings_only() {
    let mut engine = HotkeyEngine::new();
    engine.rebind("copy", "Ctrl+Y").expect("rebind");
    let json = engine.export_json().expect("export");
    let export: HotkeyExport = serde_json::from_str(&json).expect("export parses");
    assert_eq!(export.version, 1);
    for binding in &export.bindings {
        // No connection parameters, no credential material — doc 06
        // keeps those out of everything but the keyring.
        let json = serde_json::to_string(binding).expect("serialize");
        for forbidden in ["password", "key_path", "username", "host", "session_id"] {
            assert!(
                !json.contains(forbidden),
                "an exported binding must not mention {forbidden}"
            );
        }
    }
}

/// `HotkeyEngine::install` against real GTK objects: Tier-1 accels on the
/// application, a Tier-2 controller on the host widget, live rebuilds of
/// exactly one binding, and the focus check that keeps a terminal chord
/// from firing outside the terminal. Skipped by default — display-free CI
/// never runs it. To verify:
///
/// ```text
/// xvfb-run -a cargo test --test hotkeys -- --ignored
/// ```
#[test]
#[ignore = "needs a display: xvfb-run -a cargo test --test hotkeys -- --ignored"]
fn install_binds_both_tiers_and_rebuilds_one_binding() {
    use std::cell::Cell;
    use std::rc::Rc;

    use gtk4::prelude::*;
    use gtk4::{gio, glib};
    use wayterm::hotkeys::engine::{action_name, ContextHost};
    use wayterm::hotkeys::ActionContext;

    gtk4::init().expect("GTK must initialize under xvfb");
    // GTK renders accels in its own modifier order.
    let canon = |accel: &str| {
        let (key, mods) = gtk4::accelerator_parse(accel).expect("a valid accel");
        gtk4::accelerator_name(key, mods).to_string()
    };
    let app = gtk4::Application::new(
        Some("com.alex-eckstein.wayterm.hotkeys-test"),
        gio::ApplicationFlags::NON_UNIQUE,
    );

    // A stand-in window: `win.copy` counts activations; the host widget is
    // the terminal context's controller host.
    let copies = Rc::new(Cell::new(0));
    let group = gio::SimpleActionGroup::new();
    let copy = gio::SimpleAction::new("copy", None);
    let for_copy = copies.clone();
    copy.connect_activate(move |_, _| for_copy.set(for_copy.get() + 1));
    group.add_action(&copy);
    let host = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let window = gtk4::Window::new();
    window.insert_action_group("win", Some(&group));
    window.set_child(Some(&host));

    let focused = Rc::new(Cell::new(false));
    let for_focus = focused.clone();
    let mut engine = HotkeyEngine::new();
    engine.install(
        &app,
        vec![ContextHost::new(ActionContext::Terminal, &host)
            .with_focus_check(move || for_focus.get())],
    );

    let accels = |engine: &HotkeyEngine| -> Vec<(String, Vec<String>)> {
        engine
            .registry()
            .ordered_actions()
            .into_iter()
            .map(|a| {
                let accels = app.accels_for_action(&action_name(&a.id));
                (a.id.clone(), accels.iter().map(|s| s.to_string()).collect())
            })
            .collect()
    };
    let controller = (0..host.observe_controllers().n_items())
        .filter_map(|i| host.observe_controllers().item(i))
        .find_map(|c| c.downcast::<gtk4::ShortcutController>().ok())
        .expect("the terminal host carries a shortcut controller");
    assert_eq!(
        controller.propagation_phase(),
        gtk4::PropagationPhase::Capture
    );
    assert_eq!(controller.scope(), gtk4::ShortcutScope::Local);
    let shortcuts = || -> Vec<gtk4::Shortcut> {
        (0..controller.n_items())
            .filter_map(|i| controller.item(i))
            .map(|s| s.downcast::<gtk4::Shortcut>().expect("a shortcut"))
            .collect()
    };
    let triggers = || -> Vec<String> {
        shortcuts()
            .iter()
            .map(|s| s.trigger().expect("a trigger").to_str().to_string())
            .collect()
    };
    let shortcut_for = |accel: &str| {
        shortcuts()
            .into_iter()
            .find(|s| s.trigger().is_some_and(|t| t.to_str() == accel))
    };

    // Tier 1: every global holds its accel, `toggle_sftp`'s former manual
    // one included; terminal actions hold none (they live in Tier 2).
    let before = accels(&engine);
    let get = |all: &[(String, Vec<String>)], id: &str| {
        all.iter().find(|(a, _)| a == id).unwrap().1.clone()
    };
    assert_eq!(
        get(&before, "toggle_sftp"),
        vec![canon("<Control><Shift>b")]
    );
    assert_eq!(get(&before, "reconnect"), vec![canon("<Control><Shift>r")]);
    assert_eq!(get(&before, "disconnect"), vec![canon("<Control><Shift>x")]);
    assert!(get(&before, "copy").is_empty());

    // Tier 2: one shortcut per terminal action.
    assert_eq!(shortcuts().len(), 9);
    let tier2_before = triggers();
    assert!(tier2_before.contains(&canon("<Control><Shift>c")));

    // The focus check: outside the terminal the chord propagates untouched.
    let activate = |s: &gtk4::Shortcut| {
        s.action().expect("an action").activate(
            gtk4::ShortcutActionFlags::empty(),
            &host,
            None::<&glib::Variant>,
        )
    };
    let copy_shortcut = shortcut_for(&canon("<Control><Shift>c")).expect("copy's shortcut");
    assert!(!activate(&copy_shortcut));
    assert_eq!(copies.get(), 0);
    focused.set(true);
    assert!(activate(&copy_shortcut));
    assert_eq!(copies.get(), 1);

    // A Tier-1 rebind changes exactly that accel, live.
    engine.rebind("new_tab", "Ctrl+Alt+T").expect("free chord");
    let after = accels(&engine);
    let changed: Vec<&String> = before
        .iter()
        .zip(&after)
        .filter(|(b, a)| b != a)
        .map(|(_, (id, _))| id)
        .collect();
    assert_eq!(changed, vec!["new_tab"]);
    assert_eq!(get(&after, "new_tab"), vec![canon("<Control><Alt>t")]);
    assert_eq!(triggers(), tier2_before, "Tier 2 untouched");

    // A Tier-2 rebind swaps exactly that trigger, on the same shortcut.
    engine.rebind("copy", "Ctrl+Alt+C").expect("free chord");
    let tier2_after = triggers();
    let changed = tier2_before
        .iter()
        .zip(&tier2_after)
        .filter(|(b, a)| b != a)
        .count();
    assert_eq!(changed, 1);
    let rebound = shortcut_for(&canon("<Control><Alt>c")).expect("rebound trigger");
    assert_eq!(rebound, copy_shortcut, "rebuilt in place, not reinstalled");
    assert_eq!(accels(&engine), after, "Tier 1 untouched");

    // Disable frees the chord; reset restores the default.
    engine.set_enabled("copy", false).expect("disable");
    assert_eq!(copy_shortcut.trigger().unwrap().to_str(), "never");
    engine.reset_to_default("copy").expect("reset");
    assert_eq!(triggers(), tier2_before);
    engine.set_enabled("toggle_sftp", false).expect("disable");
    assert!(app.accels_for_action("win.toggle_sftp").is_empty());
    engine.reset_to_default("toggle_sftp").expect("reset");
    assert_eq!(
        get(&accels(&engine), "toggle_sftp"),
        vec![canon("<Control><Shift>b")]
    );

    // Installing again replaces the controller instead of stacking one.
    engine.install(&app, vec![ContextHost::new(ActionContext::Terminal, &host)]);
    let controllers = (0..host.observe_controllers().n_items())
        .filter_map(|i| host.observe_controllers().item(i))
        .filter(|c| c.is::<gtk4::ShortcutController>())
        .count();
    assert_eq!(controllers, 1);
}
