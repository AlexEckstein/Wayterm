//! Search on a real VTE widget, under xvfb. Alone in its binary because GTK
//! binds to the first thread that initializes it. To run:
//!
//! ```text
//! xvfb-run -a cargo test --test vte_search -- --ignored
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use wayterm::terminal::{TerminalBackend, VteBackend};

fn write(backend: &VteBackend, text: &str) {
    backend.write(text.as_bytes());
    pump();
}

/// `feed` lands through VTE's decode queue.
fn pump() {
    let ctx = glib::MainContext::default();
    for _ in 0..100 {
        ctx.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn walk(backend: &VteBackend, query: &str, reverse: bool, steps: usize) -> Vec<String> {
    (0..steps)
        .map(|_| {
            backend.search(query, reverse);
            backend.selected_text().unwrap_or_default()
        })
        .collect()
}

#[test]
#[ignore = "needs a display: xvfb-run -a cargo test --test vte_search -- --ignored"]
fn vte_search_finds_steps_and_clears() {
    gtk4::init().expect("GTK must initialize under xvfb");
    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = logged.clone();
    glib::log_set_handler(
        Some("VTE"),
        glib::LogLevels::LEVEL_CRITICAL | glib::LogLevels::LEVEL_WARNING,
        false,
        false,
        move |_, _, message| sink.lock().unwrap().push(message.to_string()),
    );

    let (tx, _rx) = tokio::sync::mpsc::channel(256);
    let backend = VteBackend::new(tx);
    backend.set_size(24, 80);
    backend.set_scrollback_limit(1000);
    // A match is the selection, and VTE needs a realized widget to select.
    let window = gtk4::Window::new();
    window.set_child(Some(&backend.widget()));
    window.present();
    pump();
    let lines: String = (0..60)
        .map(|i| {
            let word = if i % 20 == 5 { "needle" } else { "hay" };
            format!("line {i} {word}\r\n")
        })
        .collect();
    write(&backend, &lines);

    let matches = r"line \d+ needle";
    assert_eq!(
        walk(&backend, matches, false, 4),
        [
            "line 45 needle",
            "line 5 needle",
            "line 25 needle",
            "line 45 needle"
        ],
        "forward from the viewport, wrapping"
    );
    assert_eq!(
        walk(&backend, matches, true, 3),
        ["line 25 needle", "line 5 needle", "line 45 needle"],
        "backward, wrapping"
    );

    assert_eq!(
        walk(&backend, "line 5 needle", false, 3),
        ["line 5 needle"; 3],
        "a lone match stays selected"
    );
    assert_eq!(
        walk(&backend, "line 5 needle", true, 3),
        ["line 5 needle"; 3],
        "a lone match stays selected going backward"
    );

    assert_eq!(walk(&backend, "LINE 5 NEEDLE", false, 1), [""], "case");
    assert_eq!(walk(&backend, "absent", false, 2), ["", ""]);

    for clearing in ["", "broken("] {
        backend.search("line 5 needle", false);
        assert!(backend.selected_text().is_some());
        backend.search(clearing, false);
        assert_eq!(backend.selected_text(), None, "query {clearing:?}");
    }

    assert_eq!(*logged.lock().unwrap(), Vec::<String>::new(), "VTE log");
}
