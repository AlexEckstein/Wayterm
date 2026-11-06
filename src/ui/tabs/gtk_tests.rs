//! Widget checks; they need a display and run from `ui::gtk_tests`.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use gtk4::glib;

use super::ssh::{follow_terminal_directory, mark_sidebar_attached};
use super::terminal::{
    focus_after_connect, focus_terminal, keep_shell_directory, show_session_ended,
    spawn_notification_drain, terminal_state,
};
use super::*;
use crate::hotkeys::engine::focus_within;
use crate::terminal::color_scheme::{Brightness, Rgb, DARK, LIGHT};
use crate::terminal::options::{DefaultColors, SelectionColors, VteBlink, VteCursor, VteOptions};
use crate::terminal::{create_terminal, TerminalDefaults};
use crate::ui::translucency::gtk_tests::pixel_at;
use vte4::prelude::*;

/// A tab out of view posts through the drain, named by its session when
/// the program gave no title; a burst posts once; the drain ends with the
/// channel. The window here is never active, so every tab is out of view
/// (`tray::should_notify` covers the rest without a display).
pub(in crate::ui) fn terminal_notifications_reach_the_desktop_hook() {
    let notebook = gtk4::Notebook::new();
    let window = gtk4::Window::new();
    window.set_child(Some(&notebook));
    let session = SessionConfig {
        id: 1,
        name: "prod-web".into(),
        session_type: SessionType::LocalShell,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session.clone()));
    notebook.append_page(&page, None::<&gtk4::Widget>);

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let posted: Rc<RefCell<Vec<crate::ui::tray::Alert>>> = Rc::default();
    let sink = posted.clone();
    spawn_notification_drain(&notebook, &page, &session, rx, move |alert| {
        sink.borrow_mut().push(alert.clone());
    });
    for body in ["done", "dropped by the limit"] {
        tx.try_send((String::new(), body.to_string())).unwrap();
    }
    drop(tx);
    let context = glib::MainContext::default();
    while context.iteration(false) {}

    let posted = posted.borrow();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].title, "prod-web");
    assert_eq!(posted[0].body, "done");
    window.close();
}

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
/// VTE signal, and the Ghostty view's hook when built — while a report
/// from another host (a nested ssh) does not.
pub(in crate::ui) fn sftp_sidebar_follows_the_shell_directory() {
    sidebar_follows_the_shell_directory("vte");
    #[cfg(feature = "backend-ghostty")]
    {
        use crate::terminal::{install_backend_preference, BackendKind};
        install_backend_preference(BackendKind::Ghostty);
        sidebar_follows_the_shell_directory("ghostty");
        install_backend_preference(BackendKind::Vte);
    }
}

fn sidebar_follows_the_shell_directory(backend_name: &str) {
    let session = SessionConfig {
        id: 1,
        name: "ssh-1".into(),
        session_type: SessionType::SSH,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session));
    let backend = create_terminal().backend;
    assert_eq!(backend.name(), backend_name);
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

/// A connected tab keeps the directory its shell reports, decoded, through
/// the real connect path on each backend.
pub(in crate::ui) fn a_terminal_tab_keeps_the_shell_directory() {
    tab_keeps_the_shell_directory("vte");
    #[cfg(feature = "backend-ghostty")]
    {
        use crate::terminal::{install_backend_preference, BackendKind};
        install_backend_preference(BackendKind::Ghostty);
        tab_keeps_the_shell_directory("ghostty");
        install_backend_preference(BackendKind::Vte);
    }
}

fn tab_keeps_the_shell_directory(backend_name: &str) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _entered = runtime.enter();
    let dir = tempfile::tempdir().expect("temp dir");
    let shell = dir.path().join("reporting-shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nprintf '\\033]7;file://box/srv/my%%20site\\007'\nexec sleep 30\n",
    )
    .expect("write the shell");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let notebook = gtk4::Notebook::new();
    let window = gtk4::Window::new();
    window.set_child(Some(&notebook));
    window.present();
    let session = SessionConfig {
        id: 7_002,
        name: "local".into(),
        session_type: SessionType::LocalShell,
        local_shell: Some(shell.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let store = Rc::new(SessionStore::in_memory().expect("in-memory store"));

    let page = SessionTab::open(&notebook, &session, &store);
    assert_eq!(SessionTab::shell_directory_for_page(&page), None);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while SessionTab::shell_directory_for_page(&page).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "{backend_name}: the tab never kept the report"
        );
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        SessionTab::active_terminal(&notebook).unwrap().name(),
        backend_name
    );
    assert_eq!(
        SessionTab::shell_directory_for_page(&page).as_deref(),
        Some("/srv/my site")
    );
    close_tab(&notebook, &page);
    window.destroy();
    for _ in 0..20 {
        glib::MainContext::default().iteration(false);
    }
}

/// A new terminal on the page starts with no directory, and a report the
/// one it replaced still had in flight does not land on it.
pub(in crate::ui) fn a_replaced_terminal_no_longer_reports_to_the_tab() {
    let session = SessionConfig {
        id: 1,
        name: "ssh-1".into(),
        session_type: SessionType::SSH,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session));
    let report = |backend: &Arc<dyn TerminalBackend>, uri: &str| {
        backend.write(format!("\x1b]7;{uri}\x07").as_bytes());
        // VTE parses fed bytes on a timer, not inline.
        for _ in 0..100 {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    };

    let old = create_terminal().backend;
    set_terminal_area(&page, &old.widget());
    keep_shell_directory(&page, &old.widget());
    report(&old, "file://box/srv");
    assert_eq!(
        SessionTab::shell_directory_for_page(&page).as_deref(),
        Some("/srv")
    );

    let new = create_terminal().backend;
    set_terminal_area(&page, &new.widget());
    keep_shell_directory(&page, &new.widget());
    assert_eq!(SessionTab::shell_directory_for_page(&page), None);
    report(&old, "file://box/tmp");
    assert_eq!(SessionTab::shell_directory_for_page(&page), None);
    report(&new, "file://box/var/log");
    assert_eq!(
        SessionTab::shell_directory_for_page(&page).as_deref(),
        Some("/var/log")
    );
}

/// The shell exiting lets go of the page's SFTP sidebar: an empty one
/// takes its place, keeping the follow setting, and the old one is freed
/// with its SFTP task.
pub(in crate::ui) fn the_sftp_session_ends_with_the_shell() {
    let session = SessionConfig {
        id: 1,
        name: "ssh-1".into(),
        session_type: SessionType::SSH,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session));
    let backend = create_terminal().backend;
    set_terminal_area(&page, &backend.widget());
    terminal_state(&page).terminal.replace(Some(backend));
    let notebook = gtk4::Notebook::new();
    notebook.append_page(&page, None::<&gtk4::Widget>);
    let old = SessionTab::sidebar_for_page(&page).expect("an SSH page's sidebar");
    old.set_following(false);
    mark_sidebar_attached(&page);
    let old_widget = old.widget().downgrade();
    drop(old);

    show_session_ended(&notebook, &page);
    let new = SessionTab::sidebar_for_page(&page).expect("a sidebar after the end");
    assert!(old_widget.upgrade().is_none(), "the old sidebar lives on");
    assert!(!new.is_following(), "the follow setting carries over");
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
        font_size: Some(15.0),
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
    let vte = terminal.downcast_ref::<vte4::Terminal>().unwrap();
    assert_eq!(
        vte.font().map(|font| font.to_string()).as_deref(),
        Some("Monospace 15")
    );
    assert_eq!(vte.scrollback_lines(), 10_000);
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
    tabs_honor_the_session_backend(&notebook, &store);
    window.destroy();
}

/// A session's `preferred_backend` picks its tab's terminal; an empty or
/// unknown value follows the installed row. Ghostty is VTE in a build
/// without it, so only a `backend-ghostty` run tells the two apart.
fn tabs_honor_the_session_backend(notebook: &gtk4::Notebook, store: &Rc<SessionStore>) {
    use crate::terminal::{install_backend_preference, BackendKind};
    use BackendKind::{Ghostty, Vte};

    let ghostty = Ghostty.effective().as_setting();
    for (global, preferred, expected) in [
        (Vte, Some("ghostty"), ghostty),
        (Ghostty, Some("vte"), "vte"),
        (Vte, None, "vte"),
        (Ghostty, None, ghostty),
        (Ghostty, Some(""), ghostty),
        (Vte, Some("alacritty"), "vte"),
        (Ghostty, Some("alacritty"), ghostty),
    ] {
        install_backend_preference(global);
        let session = SessionConfig {
            id: 7_003,
            name: "backend".into(),
            session_type: SessionType::LocalShell,
            local_shell: Some("/bin/sh".into()),
            preferred_backend: preferred.map(str::to_string),
            ..Default::default()
        };
        let page = SessionTab::open(notebook, &session, store);
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
        let terminal = SessionTab::active_terminal(notebook).expect("a connected terminal");
        assert_eq!(
            terminal.name(),
            expected,
            "session {preferred:?} over global {global:?}"
        );
        drop(terminal);
        close_tab(notebook, &page);
    }
    install_backend_preference(Vte);
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

/// Changing a global row re-applies it to every open terminal, under a
/// session's overrides; an unchanged row leaves terminals alone.
pub(in crate::ui) fn terminals_follow_the_font_opacity_and_scrollback_rows() {
    let notebook = gtk4::Notebook::new();
    let tab = |id, font_size| {
        let session = SessionConfig {
            id,
            name: format!("local-{id}"),
            session_type: SessionType::LocalShell,
            font_size,
            ..Default::default()
        };
        let page = page_content(&Rc::new(session));
        let backend = create_terminal().backend;
        let terminal = backend.widget().downcast::<vte4::Terminal>().unwrap();
        set_terminal_area(&page, &terminal);
        terminal_state(&page).terminal.replace(Some(backend));
        notebook.append_page(&page, None::<&gtk4::Widget>);
        terminal
    };
    let follows = tab(1, None);
    let pinned = tab(2, Some(9.0));
    crate::ui::appearance::track(&notebook);
    let font = |terminal: &vte4::Terminal| terminal.font().map(|font| font.to_string());

    let rows = TerminalDefaults {
        font_family: "Monospace".into(),
        font_size: 20.0,
        opacity: 0.5,
        scrollback_lines: 321,
        ..Default::default()
    };
    crate::ui::appearance::install_terminal_defaults(rows.clone());
    assert_eq!(font(&follows).as_deref(), Some("Monospace 20"));
    assert_eq!(font(&pinned).as_deref(), Some("Monospace 9"));
    assert_eq!(follows.scrollback_lines(), 321);
    assert_eq!(pinned.scrollback_lines(), 321);
    assert!(follows.has_css_class(crate::terminal::TRANSLUCENT_CLASS));

    follows.set_scrollback_lines(7);
    crate::ui::appearance::install_terminal_defaults(rows.clone());
    assert_eq!(follows.scrollback_lines(), 7, "nothing changed");

    let vte = VteOptions {
        cursor_shape: VteCursor::IBeam,
        cursor_blink: VteBlink::Off,
        audible_bell: false,
        line_height: 1.5,
        bold_is_bright: true,
        hide_mouse: true,
        ..Default::default()
    };
    crate::ui::appearance::install_terminal_defaults(TerminalDefaults {
        vte,
        ..rows.clone()
    });
    let new_tab = create_terminal().backend.widget();
    for terminal in [&follows, new_tab.downcast_ref().unwrap()] {
        let terminal: &vte4::Terminal = terminal;
        assert_eq!(terminal.cursor_shape(), vte4::CursorShape::Ibeam);
        assert_eq!(terminal.cursor_blink_mode(), vte4::CursorBlinkMode::Off);
        assert!(!terminal.is_audible_bell());
        assert_eq!(terminal.cell_height_scale(), 1.5);
        assert!(terminal.is_bold_is_bright());
        assert!(terminal.is_mouse_autohide());
    }
    assert_eq!(follows.scrollback_lines(), 7, "a VTE option alone");

    crate::ui::appearance::install_terminal_defaults(TerminalDefaults::default());
    assert_eq!(follows.cursor_shape(), vte4::CursorShape::Block);
    assert!(follows.is_audible_bell());
    assert_eq!(font(&follows).as_deref(), Some("Monospace 12"));
    assert_eq!(follows.scrollback_lines(), 10_000);
    assert!(!follows.has_css_class(crate::terminal::TRANSLUCENT_CLASS));
}

/// A palette clears VTE's selection colors, so setting one puts the
/// installed colors back. VTE has no getter for them: the pixels tell.
pub(in crate::ui) fn a_scheme_change_keeps_the_selection_color() {
    let selection = SelectionColors {
        fg: None,
        bg: Some([10, 200, 30]),
    };
    crate::ui::appearance::install_terminal_defaults(TerminalDefaults {
        vte: VteOptions {
            selection,
            ..Default::default()
        },
        ..Default::default()
    });
    let backend = create_terminal().backend;
    let widget = backend.widget();
    let window = gtk4::Window::new();
    window.set_child(Some(&widget));
    window.present();
    let context = glib::MainContext::default();
    let settle = || {
        for _ in 0..50 {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    backend.set_color_scheme("Dark");
    backend.write(b"xxxxxxxx");
    settle();
    backend.select_all();
    settle();
    assert!(backend
        .selected_text()
        .is_some_and(|text| text.contains('x')));
    let lit = [30, 200, 10, 255];
    assert_eq!(pixel_at(&widget, 2, 2), lit);

    backend.set_color_scheme("Light");
    settle();
    assert_eq!(pixel_at(&widget, 2, 2), lit, "a new palette keeps it");

    crate::ui::appearance::install_terminal_defaults(TerminalDefaults::default());
    settle();
    assert_ne!(pixel_at(&widget, 2, 2), lit, "the default gives it back");
    window.destroy();
}

/// Custom colors reach an open tab and a new one, hold through a scheme
/// change and over a session's scheme, keep the opacity, and go at a reset.
pub(in crate::ui) fn custom_colors_replace_the_schemes() {
    let notebook = gtk4::Notebook::new();
    let session = SessionConfig {
        id: 1,
        name: "local-1".into(),
        session_type: SessionType::LocalShell,
        ..Default::default()
    };
    let page = page_content(&Rc::new(session));
    let backend = create_terminal().backend;
    let widget = backend.widget();
    set_terminal_area(&page, &widget);
    terminal_state(&page)
        .terminal
        .replace(Some(backend.clone()));
    notebook.append_page(&page, None::<&gtk4::Widget>);
    crate::ui::appearance::track(&notebook);
    let window = gtk4::Window::new();
    window.set_child(Some(&notebook));
    window.present();
    let context = glib::MainContext::default();
    let settle = || {
        for _ in 0..50 {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    };

    crate::ui::appearance::install_default_scheme(Some("Dark".into()));
    backend.write("\u{2588}\u{2588}".as_bytes());
    settle();
    let text = pixel_at(&widget, 2, 2);
    assert_eq!(background(&widget), rgb(DARK.background));

    let colors = DefaultColors {
        fg: Some([10, 200, 30]),
        bg: Some([40, 50, 60]),
    };
    let custom = TerminalDefaults {
        opacity: 0.5,
        vte: VteOptions {
            colors,
            ..Default::default()
        },
        ..Default::default()
    };
    crate::ui::appearance::install_terminal_defaults(custom.clone());
    settle();
    assert_eq!(background(&widget), (40, 50, 60), "an open tab");
    assert_eq!(pixel_at(&widget, 2, 2), [30, 200, 10, 255]);
    let alpha = widget
        .downcast_ref::<vte4::Terminal>()
        .unwrap()
        .color_background_for_draw()
        .alpha();
    assert_eq!(alpha, 0.5, "the opacity stays");

    backend.set_color_scheme("Light");
    assert_eq!(background(&widget), (40, 50, 60), "over any scheme");
    let new_tab = create_terminal().backend;
    new_tab.set_color_scheme("Light");
    assert_eq!(background(&new_tab.widget()), (40, 50, 60), "a new tab");

    crate::ui::appearance::install_terminal_defaults(TerminalDefaults {
        vte: VteOptions {
            colors: DefaultColors { bg: None, ..colors },
            ..Default::default()
        },
        ..custom
    });
    assert_eq!(background(&widget), rgb(DARK.background), "one reset alone");
    settle();
    assert_eq!(pixel_at(&widget, 2, 2), [30, 200, 10, 255]);

    crate::ui::appearance::install_terminal_defaults(TerminalDefaults::default());
    settle();
    assert_eq!(pixel_at(&widget, 2, 2), text);
    crate::ui::appearance::install_default_scheme(None);
    window.destroy();
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
