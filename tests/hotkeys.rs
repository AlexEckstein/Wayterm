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

/// `HotkeyEngine::install` against real GTK objects (doc 07 §Dispatch):
/// one capture-phase controller on the window, each shortcut firing its
/// window action only while its context is active and the action is
/// enabled, live rebuilds of exactly one trigger, and the report of
/// actions with no window action. Skipped by default — display-free CI
/// never runs it. To verify:
///
/// ```text
/// xvfb-run -a cargo test --test hotkeys -- --ignored
/// ```
#[test]
#[ignore = "needs a display: xvfb-run -a cargo test --test hotkeys -- --ignored"]
fn install_dispatches_by_context_and_rebuilds_one_binding() {
    use std::cell::Cell;
    use std::rc::Rc;

    use gtk4::prelude::*;
    use gtk4::{gio, glib};
    use wayterm::hotkeys::engine::{focus_owns_keys, precedence, ContextHost, DISPATCHER_NAME};
    use wayterm::hotkeys::{ActionContext, DEFAULT_ACTIONS};

    gtk4::init().expect("GTK must initialize under xvfb");
    // GTK renders accels in its own modifier order.
    let canon = |accel: &str| {
        let (key, mods) = gtk4::accelerator_parse(accel).expect("a valid accel");
        gtk4::accelerator_name(key, mods).to_string()
    };

    // A stand-in main window: `copy` and `new_tab` count activations,
    // `disconnect` exists but is disabled; nothing else is registered.
    let window = gtk4::ApplicationWindow::builder().build();
    let counter = |id: &str| {
        let count = Rc::new(Cell::new(0));
        let action = gio::SimpleAction::new(id, None);
        let for_action = count.clone();
        action.connect_activate(move |_, _| for_action.set(for_action.get() + 1));
        window.add_action(&action);
        (action, count)
    };
    let (_, copies) = counter("copy");
    let (_, new_tabs) = counter("new_tab");
    let (disconnect, _) = counter("disconnect");
    disconnect.set_enabled(false);
    // A panel (an entry and a button) and a button outside it.
    let panel = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let entry = gtk4::Entry::new();
    let inside = gtk4::Button::with_label("inside");
    panel.append(&entry);
    panel.append(&inside);
    let outside = gtk4::Button::with_label("outside");
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&panel);
    root.append(&outside);
    window.set_child(Some(&root));
    window.present();

    let terminal_live = Rc::new(Cell::new(false));
    let for_terminal = terminal_live.clone();
    let mut engine = HotkeyEngine::new();
    let unhandled = engine.install(
        &window,
        vec![ContextHost::new(ActionContext::Terminal, move || {
            for_terminal.get()
        })],
    );
    let mut expected_unhandled: Vec<String> = DEFAULT_ACTIONS
        .iter()
        .map(|d| d.id.to_string())
        .filter(|id| !["copy", "new_tab", "disconnect"].contains(&id.as_str()))
        .collect();
    expected_unhandled.sort_by_key(|id| {
        let action = engine.action(id).unwrap();
        (action.context, action.id.clone())
    });
    assert_eq!(unhandled, expected_unhandled);

    let controllers = || -> Vec<gtk4::ShortcutController> {
        (0..window.observe_controllers().n_items())
            .filter_map(|i| window.observe_controllers().item(i))
            .filter_map(|c| c.downcast::<gtk4::ShortcutController>().ok())
            .filter(|c| c.name().as_deref() == Some(DISPATCHER_NAME))
            .collect()
    };
    let controller = controllers()
        .into_iter()
        .next()
        .expect("the window carries the dispatcher");
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
    let activate = |s: &gtk4::Shortcut| {
        s.action().expect("an action").activate(
            gtk4::ShortcutActionFlags::empty(),
            &window,
            None::<&glib::Variant>,
        )
    };

    // One shortcut per action of a hosted context (global, terminal): the
    // session tree and SFTP were given no host. Narrow contexts first.
    let hosted: Vec<ActionContext> = engine
        .registry()
        .ordered_actions()
        .into_iter()
        .filter(|a| matches!(a.context, ActionContext::Global | ActionContext::Terminal))
        .map(|a| a.context)
        .collect();
    assert_eq!(shortcuts().len(), hosted.len());
    let first_global = hosted
        .iter()
        .filter(|c| precedence(**c) < precedence(ActionContext::Global))
        .count();
    let tier_order = triggers();
    assert!(tier_order[..first_global].contains(&canon("<Control><Shift>c")));
    assert!(tier_order[first_global..].contains(&canon("<Control><Shift>t")));

    // A global fires whatever has focus; a terminal action only while its
    // context is active.
    let new_tab = shortcut_for(&canon("<Control><Shift>t")).expect("new_tab's shortcut");
    assert!(activate(&new_tab));
    assert_eq!(new_tabs.get(), 1);
    let copy = shortcut_for(&canon("<Control><Shift>c")).expect("copy's shortcut");
    assert!(!activate(&copy), "no terminal: the key passes through");
    assert_eq!(copies.get(), 0);
    terminal_live.set(true);
    assert!(activate(&copy));
    assert_eq!(copies.get(), 1);

    // A disabled or missing window action lets the key through.
    let disconnect_shortcut =
        shortcut_for(&canon("<Control><Shift>x")).expect("disconnect's shortcut");
    assert!(!activate(&disconnect_shortcut));
    let paste = shortcut_for(&canon("<Control><Shift>v")).expect("paste's shortcut");
    assert!(!activate(&paste), "no win.paste registered");

    // A rebind swaps exactly that trigger, in place.
    let before = triggers();
    engine.rebind("new_tab", "Ctrl+Alt+T").expect("free chord");
    let after = triggers();
    let changed = before.iter().zip(&after).filter(|(b, a)| b != a).count();
    assert_eq!(changed, 1);
    let rebound = shortcut_for(&canon("<Control><Alt>t")).expect("rebound trigger");
    assert_eq!(rebound, new_tab, "rebuilt in place, not reinstalled");

    // Disable frees the chord; reset restores the default.
    engine.set_enabled("copy", false).expect("disable");
    assert_eq!(copy.trigger().unwrap().to_str(), "never");
    engine.reset_to_default("copy").expect("reset");
    engine.reset_to_default("new_tab").expect("reset");
    assert_eq!(triggers(), before);

    // A focus host: active with focus inside the panel, except in its
    // text field, where typing wins.
    let host = ContextHost::focus_within(ActionContext::SessionTree, &panel);
    inside.grab_focus();
    assert!(host.is_active());
    assert!(!focus_owns_keys(&window));
    entry.grab_focus();
    assert!(focus_owns_keys(&window));
    assert!(!host.is_active(), "typing in the panel's entry");
    outside.grab_focus();
    assert!(!host.is_active(), "focus outside the panel");

    // Installing again replaces the controller instead of stacking one.
    engine.install(&window, Vec::new());
    assert_eq!(controllers().len(), 1);
    window.destroy();
}
