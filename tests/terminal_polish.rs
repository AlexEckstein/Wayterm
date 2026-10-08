//! P4-PAR-2 (terminal polish) — display-free tests.
//!
//! Three contracts are locked here:
//!
//! 1. **doc 07's chord table** for the six terminal actions — the hotkey
//!    engine's `current_key` bindings must match exactly.
//! 2. **The action-to-terminal routing table** — each doc-07 id resolves to
//!    the right [`TerminalBackend`] call, recorded by [`FakeBackend`].
//! 3. **The tab-title state machine** (placeholder → set → renamed) behind
//!    double-click rename.
//!
//! Widget-dependent VTE behavior is not display-free; the xvfb path is the
//! ignored `vte_scrollback_roundtrip` test at the bottom.

use wayterm::terminal::fake::{FakeBackend, Interaction};
use wayterm::terminal::{TerminalBackend, VteBackend};
use wayterm::ui::main_window::{
    initial_search_reverse, route_terminal_action, TerminalAction, TerminalActionInputs,
    TERMINAL_ACTION_TABLE,
};
use wayterm::ui::tab_label::{TabTitle, TabTitleState};

/// Lock doc 07's §Default Actions rows for the terminal context — the
/// table in `main_window` is what the hotkey-core lane binds against.
#[test]
fn terminal_action_chords_match_doc07_table_exactly() {
    let doc_07 = [
        ("copy", "Ctrl+Shift+C"),
        ("paste", "Ctrl+Shift+V"),
        ("select_all", "Ctrl+Shift+A"),
        ("clear", "Ctrl+Shift+L"),
        ("search", "Ctrl+Shift+F"),
        ("search_scrollback", "Ctrl+Shift+K"),
    ];
    assert_eq!(TERMINAL_ACTION_TABLE, doc_07);

    // Doc 07 de-conflicted the table: every chord appears once.
    let mut chords: Vec<&str> = TERMINAL_ACTION_TABLE
        .iter()
        .map(|(_, chord)| *chord)
        .collect();
    chords.sort_unstable();
    let unique = chords.clone();
    chords.dedup();
    assert_eq!(
        chords, unique,
        "duplicate chord in the terminal action table"
    );
}

/// Route each of the six ids with its payload and assert the backend call
/// that lands on a [`FakeBackend`] — the action-to-terminal routing table.
#[test]
fn all_six_actions_route_to_their_backend_call() {
    let cases: [(
        &str,
        TerminalActionInputs<'static>,
        TerminalAction,
        Interaction,
    ); 6] = [
        (
            "copy",
            TerminalActionInputs::default(),
            TerminalAction::Copy,
            Interaction::CopySelection,
        ),
        (
            "paste",
            TerminalActionInputs {
                clipboard: Some("uname -a"),
                ..TerminalActionInputs::default()
            },
            TerminalAction::Paste("uname -a".to_string()),
            Interaction::Paste("uname -a".to_string()),
        ),
        (
            "select_all",
            TerminalActionInputs::default(),
            TerminalAction::SelectAll,
            Interaction::SelectAll,
        ),
        (
            "clear",
            TerminalActionInputs::default(),
            TerminalAction::Clear,
            Interaction::Clear,
        ),
        (
            "search",
            TerminalActionInputs {
                search: Some("needle"),
                ..TerminalActionInputs::default()
            },
            TerminalAction::Search {
                text: "needle".to_string(),
                reverse: false,
            },
            Interaction::Search {
                text: "needle".to_string(),
                reverse: false,
            },
        ),
        (
            "search_scrollback",
            TerminalActionInputs {
                search: Some("needle"),
                search_reverse: true,
                ..TerminalActionInputs::default()
            },
            TerminalAction::Search {
                text: "needle".to_string(),
                reverse: true,
            },
            Interaction::Search {
                text: "needle".to_string(),
                reverse: true,
            },
        ),
    ];

    for (id, inputs, expected_action, expected_interaction) in cases {
        let action = route_terminal_action(id, &inputs)
            .unwrap_or_else(|| panic!("action '{id}' must route"));
        assert_eq!(action, expected_action, "route for '{id}'");

        let backend = FakeBackend::new();
        action.apply(&backend);
        assert_eq!(
            backend.interactions(),
            vec![expected_interaction],
            "backend call for '{id}'"
        );
    }
}

/// Missing or empty payloads are "nothing to do", never a panic — this is
/// what keeps `paste` with an empty clipboard and an empty search entry safe.
#[test]
fn payloads_gate_the_route() {
    let none = |id: &str, inputs: &TerminalActionInputs<'_>| {
        assert_eq!(
            route_terminal_action(id, inputs),
            None,
            "'{id}' should not route"
        );
    };

    none("paste", &TerminalActionInputs::default());
    none(
        "paste",
        &TerminalActionInputs {
            clipboard: Some(""),
            ..TerminalActionInputs::default()
        },
    );
    none("search", &TerminalActionInputs::default());
    none(
        "search_scrollback",
        &TerminalActionInputs {
            search: Some(""),
            ..TerminalActionInputs::default()
        },
    );
    none("rename_tab", &TerminalActionInputs::default());
    none("", &TerminalActionInputs::default());

    // Payload-less ids ignore stray inputs entirely.
    assert_eq!(
        route_terminal_action(
            "copy",
            &TerminalActionInputs {
                clipboard: Some("ignored"),
                ..TerminalActionInputs::default()
            }
        ),
        Some(TerminalAction::Copy),
    );
}

/// Doc 07 / deliverable 5: `search` and `search_scrollback` share the
/// popover and differ only in the initial search direction.
#[test]
fn search_actions_differ_only_in_initial_direction() {
    assert!(!initial_search_reverse("search"));
    assert!(initial_search_reverse("search_scrollback"));

    // The shared route stays identical for both: direction comes from the
    // inputs (Enter/Shift+Enter), not from the id.
    let inputs = TerminalActionInputs {
        search: Some("q"),
        ..TerminalActionInputs::default()
    };
    assert_eq!(
        route_terminal_action("search", &inputs),
        route_terminal_action("search_scrollback", &inputs),
    );
}

/// Tab rename state machine: placeholder (store name) → set (programmatic
/// `TitleChanged`) → renamed (user double-click), with a user rename
/// locking out later programmatic titles (doc 02 §TitleChanged seam).
#[test]
fn tab_title_state_machine_placeholder_set_renamed() {
    let mut title = TabTitle::new("web-prod");
    assert_eq!(title.state(), &TabTitleState::Placeholder);
    assert_eq!(title.value(), "web-prod");

    // Programmatic title lands.
    assert!(title.set("root@bastion:2222"));
    assert_eq!(title.state(), &TabTitleState::Set);
    assert_eq!(title.value(), "root@bastion:2222");
    // Same value again: no change reported, state stays Set.
    assert!(!title.set("root@bastion:2222"));
    assert_eq!(title.state(), &TabTitleState::Set);

    // User double-click rename.
    assert!(title.rename("Bastion"));
    assert_eq!(title.state(), &TabTitleState::Renamed);
    assert_eq!(title.value(), "Bastion");

    // Terminal OSC title churn no longer clobbers the user's name.
    assert!(!title.set("root@bastion:2222"));
    assert_eq!(title.state(), &TabTitleState::Renamed);
    assert_eq!(title.value(), "Bastion");

    // Empty names are rejected; the tab keeps a label.
    assert!(!title.rename(""));
    assert_eq!(title.state(), &TabTitleState::Renamed);
    assert_eq!(title.value(), "Bastion");
}

/// The FakeBackend interaction log records in call order — this is the
/// recorder the routing assertions above rely on.
#[test]
fn fake_backend_records_interactions_in_order() {
    let backend = FakeBackend::new();
    backend.copy_selection();
    backend.paste("x");
    backend.select_all();
    backend.clear();
    backend.search("a", true);
    backend.scroll_to_top();
    backend.scroll_to_bottom();

    assert_eq!(
        backend.interactions(),
        vec![
            Interaction::CopySelection,
            Interaction::Paste("x".to_string()),
            Interaction::SelectAll,
            Interaction::Clear,
            Interaction::Search {
                text: "a".to_string(),
                reverse: true
            },
            Interaction::ScrollToTop,
            Interaction::ScrollToBottom,
        ]
    );
}

/// xvfb verification path for the widget-dependent `VteBackend` scrollback
/// extraction (doc 02 §Scrollback, `text_range_format`). Skipped by default
/// — display-free CI never runs it. To verify:
///
/// ```text
/// xvfb-run -a cargo test --test terminal_polish -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs a display: xvfb-run -a cargo test --test terminal_polish -- --ignored"]
fn vte_scrollback_roundtrip() {
    gtk4::init().expect("GTK must initialize under xvfb");
    let (tx, _rx) = tokio::sync::mpsc::channel(256);
    let backend = VteBackend::new(tx);
    backend.set_size(24, 80);
    backend.set_scrollback_limit(1000);

    for i in 0..100 {
        // CR+LF: plain LF leaves the cursor column, soft-wrapping rows and
        // distorting the buffer geometry this test is verifying.
        backend.write(format!("line-{i}\r\n").as_bytes());
    }

    // `feed` lands through VTE's decode queue — pump the main context so
    // the bytes reach the screen/scrollback ring.
    let ctx = gtk4::glib::MainContext::default();
    for _ in 0..100 {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let text = backend
        .scrollback_text()
        .expect("scrollback_text must extract the buffer");
    assert!(
        text.contains("line-0"),
        "oldest scrollback line missing (rows assumption wrong): {text:?}"
    );
    assert!(text.contains("line-99"), "newest line missing: {text:?}");
}
