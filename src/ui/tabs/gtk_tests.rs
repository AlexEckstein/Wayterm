//! Widget checks; they need a display and run from `ui::gtk_tests`.

use std::time::Duration;

use gtk4::glib;

use super::ssh::follow_terminal_directory;
use super::terminal::{focus_after_connect, focus_terminal, terminal_state};
use super::*;
use crate::hotkeys::engine::focus_within;
use crate::terminal::color_scheme::{Brightness, Rgb, DARK, LIGHT};
use crate::terminal::create_terminal;
use vte4::prelude::*;

/// Focusing the current tab hands keyboard focus from the session tree
/// to its terminal; a tab in the background never takes it.
pub(in crate::ui) fn focusing_a_tab_moves_focus_to_its_terminal() {
    let notebook = gtk4::Notebook::new();
    // Stands in for the session tree, which holds focus after a
    // double-click.
    let tree = gtk4::Entry::new();
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&tree);
    root.append(&notebook);
    let window = gtk4::Window::new();
    window.set_child(Some(&root));

    let tabs: Vec<_> = (1..=2)
        .map(|id| {
            let session = SessionConfig {
                id,
                name: format!("local-{id}"),
                session_type: SessionType::LocalShell,
                ..Default::default()
            };
            let page = page_content(&Rc::new(session.clone()));
            let backend = create_terminal().backend;
            set_terminal_area(&page, &backend.widget());
            terminal_state(&page)
                .terminal
                .replace(Some(backend.clone()));
            notebook.append_page(&page, None::<&gtk4::Widget>);
            (session, page, backend.widget())
        })
        .collect();
    window.present();
    for _ in 0..10 {
        glib::MainContext::default().iteration(false);
    }

    let [(_, first_page, first_terminal), (_, _, second_terminal)] = &tabs[..] else {
        unreachable!();
    };
    notebook.set_current_page(Some(1));
    tree.grab_focus();
    focus_terminal(&notebook, first_page);
    assert!(focus_within(&tree), "a background tab took focus");

    notebook.set_current_page(Some(0));
    focus_terminal(&notebook, first_page);
    assert!(focus_within(first_terminal), "the opened tab's terminal");
    assert!(!focus_within(second_terminal));
    window.destroy();
}

/// A reconnect finishing on the current tab leaves focus where the user
/// put it; it takes focus only when nothing holds it, while a
/// user-started connect always does.
pub(in crate::ui) fn reconnect_does_not_steal_focus() {
    let notebook = gtk4::Notebook::new();
    let tree = gtk4::Entry::new();
    // Stands in for the SFTP sidebar's path entry, in the side dock.
    let sidebar_entry = gtk4::Entry::new();
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&tree);
    root.append(&sidebar_entry);
    root.append(&notebook);
    let window = gtk4::Window::new();
    window.set_child(Some(&root));

    let session = SessionConfig {
        id: 1,
        name: "local-1".into(),
        session_type: SessionType::LocalShell,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session));
    let backend = create_terminal().backend;
    let terminal = backend.widget();
    set_terminal_area(&page, &terminal);
    terminal_state(&page).terminal.replace(Some(backend));
    notebook.append_page(&page, None::<&gtk4::Widget>);
    window.present();
    for _ in 0..10 {
        glib::MainContext::default().iteration(false);
    }

    // Reconnect while typing in the sidebar, then in the tree.
    for holder in [
        sidebar_entry.upcast_ref::<gtk4::Widget>(),
        tree.upcast_ref(),
    ] {
        holder.grab_focus();
        state(&page).focus_on_connect.set(false);
        focus_after_connect(&notebook, &page);
        assert!(focus_within(holder), "a reconnect stole focus");
    }

    // Reconnect after the focused terminal went away with the connection.
    gtk4::prelude::GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
    focus_after_connect(&notebook, &page);
    assert!(focus_within(&terminal), "an unclaimed focus is taken back");

    // A user-started connect takes focus from the tree.
    tree.grab_focus();
    state(&page).focus_on_connect.set(true);
    focus_after_connect(&notebook, &page);
    assert!(focus_within(&terminal), "a user-started connect");
    assert!(
        !state(&page).focus_on_connect.get(),
        "the request is one-shot"
    );
    window.destroy();
}

/// Closing the focused tab hands focus to the terminal of the tab left
/// current, not the notebook GTK leaves it on; closing a tab while the
/// tree holds focus leaves it there.
pub(in crate::ui) fn closing_a_tab_focuses_the_next_terminal() {
    let notebook = gtk4::Notebook::new();
    let tree = gtk4::Entry::new();
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&tree);
    root.append(&notebook);
    let window = gtk4::Window::new();
    window.set_child(Some(&root));

    let terminals: Vec<_> = (1..=3)
        .map(|id| {
            let session = SessionConfig {
                id,
                name: format!("local-{id}"),
                session_type: SessionType::LocalShell,
                ..Default::default()
            };
            let page = page_content(&Rc::new(session));
            let backend = create_terminal().backend;
            set_terminal_area(&page, &backend.widget());
            terminal_state(&page)
                .terminal
                .replace(Some(backend.clone()));
            notebook.append_page(&page, None::<&gtk4::Widget>);
            backend.widget()
        })
        .collect();
    install_teardown_on_remove(&notebook);
    window.present();
    for _ in 0..10 {
        glib::MainContext::default().iteration(false);
    }

    notebook.set_current_page(Some(2));
    terminals[2].grab_focus();
    notebook.remove_page(Some(2));
    assert_eq!(notebook.current_page(), Some(1));
    assert!(focus_within(&terminals[1]), "the next tab's terminal");

    tree.grab_focus();
    notebook.remove_page(Some(1));
    assert!(focus_within(&tree), "a close stole focus from the tree");
    assert!(!focus_within(&terminals[0]));
    window.destroy();
}

/// The shell's OSC 7 report moves the page's sidebar — through the real
/// VTE signal — while a report from another host (a nested ssh) does not.
pub(in crate::ui) fn sftp_sidebar_follows_the_shell_directory() {
    let session = SessionConfig {
        id: 1,
        name: "ssh-1".into(),
        session_type: SessionType::SSH,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session));
    let backend = create_terminal().backend;
    let terminal = backend.widget();
    set_terminal_area(&page, &terminal);
    follow_terminal_directory(&page, &terminal);
    let sidebar = SessionTab::sidebar_for_page(&page).expect("an SSH page's sidebar");

    let report = |uri: &str| {
        backend.write(format!("\x1b]7;{uri}\x07").as_bytes());
        // VTE parses fed bytes on a timer, not inline.
        for _ in 0..100 {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    report("file://box/srv/my%20site");
    assert_eq!(sidebar.path_text(), "/srv/my site");
    report("file://inner/tmp");
    assert_eq!(sidebar.path_text(), "/srv/my site", "another host");
    report("file://box/var/log");
    assert_eq!(sidebar.path_text(), "/var/log");
}

/// The connect flow end to end on a local shell: opening the tab wires
/// a terminal and its input tap, the page reports Connected, and
/// closing the tab lets the page go. The terminal opens in its session's
/// color scheme.
pub(in crate::ui) fn a_local_shell_tab_connects_and_closes() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _entered = runtime.enter();
    let notebook = gtk4::Notebook::new();
    let window = gtk4::Window::new();
    window.set_child(Some(&notebook));
    window.present();
    let session = SessionConfig {
        id: 7_001,
        name: "local".into(),
        session_type: SessionType::LocalShell,
        local_shell: Some("/bin/sh".into()),
        color_scheme: Some("Light".into()),
        ..Default::default()
    };
    let store = Rc::new(SessionStore::in_memory().expect("in-memory store"));
    crate::ui::appearance::install_default_scheme(Some("Dark".into()));

    let page = SessionTab::open(&notebook, &session, &store);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while SessionTab::page_status(&page).map(|status| status.state)
        != Some(TransportState::Connected)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the local shell never connected"
        );
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(current_session_id(&notebook), Some(7_001));
    assert!(SessionTab::active_terminal(&notebook).is_some());
    assert!(SessionTab::active_input_tap(&notebook).is_some());
    let terminal = SessionTab::active_terminal(&notebook).unwrap().widget();
    assert_eq!(
        background(&terminal),
        rgb(LIGHT.background),
        "the session's override, applied on open"
    );
    assert!(
        SessionTab::sidebar_for_page(&page).is_none(),
        "only SSH pages carry a sidebar"
    );
    let duplicate = SessionTab::open(&notebook, &session, &store);
    assert_ne!(duplicate, page, "a second open adds a tab");
    assert_eq!(notebook.n_pages(), 2);
    assert_ne!(tab_id_of(&duplicate), tab_id_of(&page));
    close_tab(&notebook, &duplicate);
    drop(duplicate);

    let gone = page.downgrade();
    close_tab(&notebook, &page);
    drop(page);
    for _ in 0..20 {
        glib::MainContext::default().iteration(false);
    }
    assert_eq!(notebook.n_pages(), 0);
    assert!(gone.upgrade().is_none(), "a closed tab's page is freed");

    // A failed connect shows the error view, whose buttons hold the
    // page weakly: closing that tab frees its page too.
    let broken = SessionConfig {
        id: 7_002,
        name: "broken".into(),
        local_shell: Some("/nonexistent/wayterm-shell".into()),
        ..session
    };
    let page = SessionTab::open(&notebook, &broken, &store);
    let shows_error = |page: &gtk4::Widget| {
        page.first_child()
            .and_then(|view| view.first_child())
            .and_downcast::<gtk4::Label>()
            .is_some_and(|heading| heading.text() == "Connection failed")
    };
    while !shows_error(&page) {
        assert!(
            std::time::Instant::now() < deadline,
            "the missing shell never failed"
        );
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
    for _ in 0..50 {
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        matches!(
            SessionTab::page_status(&page).map(|status| status.state),
            Some(TransportState::Error(ref message)) if message.contains("shell not found")
        ),
        "the status ends on the error, not on the transport's Disconnected"
    );
    let gone = page.downgrade();
    close_tab(&notebook, &page);
    drop(page);
    for _ in 0..20 {
        glib::MainContext::default().iteration(false);
    }
    assert!(
        gone.upgrade().is_none(),
        "a tab closed on its error is freed"
    );
    window.destroy();
}

/// The background VTE paints, as 8-bit RGB.
fn background(terminal: &gtk4::Widget) -> (u8, u8, u8) {
    let color = terminal
        .downcast_ref::<vte4::Terminal>()
        .expect("a VTE terminal")
        .color_background_for_draw();
    let channel = |c: f32| (c * 255.0).round() as u8;
    (
        channel(color.red()),
        channel(color.green()),
        channel(color.blue()),
    )
}

fn rgb(Rgb(r, g, b): Rgb) -> (u8, u8, u8) {
    (r, g, b)
}

/// Every open terminal follows the global scheme and, under System, the
/// desktop theme; a session override holds; an unknown name changes
/// nothing.
pub(in crate::ui) fn terminals_follow_the_color_scheme_and_system_theme() {
    let backend = create_terminal().backend;
    let widget = backend.widget();
    backend.set_color_scheme("Light");
    assert_eq!(background(&widget), rgb(LIGHT.background));
    backend.set_color_scheme("Dark");
    assert_eq!(background(&widget), rgb(DARK.background));
    backend.set_color_scheme("Solarized");
    assert_eq!(background(&widget), rgb(DARK.background), "unknown: no-op");

    let notebook = gtk4::Notebook::new();
    let tab = |id, color_scheme: Option<&str>| {
        let session = SessionConfig {
            id,
            name: format!("local-{id}"),
            session_type: SessionType::LocalShell,
            color_scheme: color_scheme.map(str::to_string),
            ..Default::default()
        };
        let page = page_content(&Rc::new(session));
        let backend = create_terminal().backend;
        set_terminal_area(&page, &backend.widget());
        terminal_state(&page)
            .terminal
            .replace(Some(backend.clone()));
        notebook.append_page(&page, None::<&gtk4::Widget>);
        backend.widget()
    };
    let follows = tab(1, None);
    let pinned = tab(2, Some("Dark"));
    crate::ui::appearance::track(&notebook);

    crate::ui::appearance::install_default_scheme(Some("Light".into()));
    assert_eq!(background(&follows), rgb(LIGHT.background));
    assert_eq!(background(&pinned), rgb(DARK.background));

    let settings = gtk4::Settings::default().expect("GTK settings");
    let (dark_before, theme_before) = (
        settings.is_gtk_application_prefer_dark_theme(),
        settings.gtk_theme_name(),
    );
    settings.set_gtk_theme_name(Some("Adwaita"));
    settings.set_gtk_application_prefer_dark_theme(false);
    crate::ui::appearance::install_default_scheme(Some("System".into()));
    assert_eq!(background(&follows), rgb(LIGHT.background), "light desktop");

    // The theme switch alone re-colors open terminals.
    settings.set_gtk_application_prefer_dark_theme(true);
    assert_eq!(
        background(&follows),
        rgb(DARK.background),
        "dark preference"
    );
    settings.set_gtk_application_prefer_dark_theme(false);
    assert_eq!(background(&follows), rgb(LIGHT.background));
    settings.set_gtk_theme_name(Some("Adwaita-dark"));
    assert_eq!(
        background(&follows),
        rgb(DARK.background),
        "dark theme name"
    );
    assert_eq!(background(&pinned), rgb(DARK.background));

    // The portal's preference beats GTK's (KDE keeps `Breeze` in dark
    // mode, with a stale dark flag from `settings.ini`), and the chrome's
    // theme variant moves with it.
    use crate::ui::appearance::{chrome_preference, set_desktop_preference};
    settings.set_gtk_theme_name(Some("Breeze"));
    settings.set_gtk_application_prefer_dark_theme(true);
    set_desktop_preference(Some(Brightness::Light));
    assert_eq!(
        background(&follows),
        rgb(LIGHT.background),
        "portal light over the dark flag"
    );
    assert_ne!(chrome_preference(), Some(Brightness::Dark), "chrome light");
    set_desktop_preference(Some(Brightness::Dark));
    assert_eq!(
        background(&follows),
        rgb(DARK.background),
        "portal dark over a light theme name"
    );
    assert_eq!(chrome_preference(), Some(Brightness::Dark), "chrome dark");
    assert_eq!(background(&pinned), rgb(DARK.background));
    // Withdrawn, the GTK settings decide again.
    set_desktop_preference(None);
    settings.set_gtk_application_prefer_dark_theme(false);
    assert_eq!(background(&follows), rgb(LIGHT.background), "back to GTK");

    settings.set_gtk_theme_name(theme_before.as_deref());
    settings.set_gtk_application_prefer_dark_theme(dark_before);
    crate::ui::appearance::install_default_scheme(None);
}
