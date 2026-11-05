use super::selection::drag_select;
use super::*;

/// 30 lines on a 5-row screen, then the cursor's empty line: lines 0-25
/// are scrollback, 26-29 on screen. "Fox" is on lines 3, 12 (both
/// scrollback) and 27 (on screen), "fox" on line 20.
fn foxes() -> Engine {
    let engine = engine();
    for line in 0..30 {
        let word = match line {
            3 | 12 | 27 => " Fox",
            20 => " fox",
            _ => "",
        };
        engine.write(format!("line {line}{word}\r\n").as_bytes());
    }
    engine
}

/// The viewport row the current match is on, as text.
fn current_line(engine: &Engine) -> Option<String> {
    let span = engine.search_spans().into_iter().find(|s| s.current != 0)?;
    Some(
        frame(engine)
            .row_text(span.row.into())
            .trim_end()
            .to_string(),
    )
}

/// Search `needle` `steps` times; the line of each current match.
fn walk(engine: &Engine, needle: &str, backward: bool, steps: usize) -> Vec<String> {
    (0..steps)
        .map(|_| {
            assert!(engine.search(needle, backward), "{needle:?} not found");
            current_line(engine).expect("a highlighted current match")
        })
        .collect()
}

#[test]
fn forward_search_starts_at_the_viewport_top_and_wraps() {
    let engine = foxes();
    assert_eq!(
        walk(&engine, "Fox", false, 4),
        ["line 27 Fox", "line 3 Fox", "line 12 Fox", "line 27 Fox"]
    );
}

#[test]
fn backward_search_starts_at_the_viewport_bottom_and_wraps() {
    let engine = foxes();
    assert_eq!(
        walk(&engine, "Fox", true, 4),
        ["line 27 Fox", "line 12 Fox", "line 3 Fox", "line 27 Fox"]
    );
    // Turning around steps back the way it came.
    assert_eq!(walk(&engine, "Fox", false, 1), ["line 3 Fox"]);
}

/// The same answers as the VTE widget on the same screen.
#[test]
fn a_new_search_starts_from_a_scrolled_viewport() {
    for backward in [false, true] {
        let engine = foxes();
        engine.scroll(Scroll::Top);
        assert_eq!(walk(&engine, "Fox", backward, 1), ["line 3 Fox"]);

        // Lines 10-14 on screen.
        let engine = foxes();
        engine.scroll(Scroll::Top);
        engine.scroll(Scroll::By(10));
        assert_eq!(walk(&engine, "Fox", backward, 1), ["line 12 Fox"]);
        assert_eq!(frame(&engine).row_text(0).trim_end(), "line 10");
    }
}

#[test]
fn search_is_case_sensitive() {
    let engine = foxes();
    assert_eq!(
        walk(&engine, "fox", true, 2),
        ["line 20 fox", "line 20 fox"]
    );
    assert!(!engine.search("FOX", false));
    assert_eq!(engine.search_spans(), []);
}

#[test]
fn a_match_in_the_scrollback_is_scrolled_to_the_middle() {
    let engine = foxes();
    let bottom = frame(&engine).row_text(0);
    assert!(engine.search("line 3 Fox", false));
    assert_ne!(frame(&engine).row_text(0), bottom, "the viewport moved");
    let current = Span {
        row: 2,
        start: 0,
        end: 10,
        current: 1,
    };
    assert_eq!(engine.search_spans(), [current]);
    assert_eq!(frame(&engine).row_text(2).trim_end(), "line 3 Fox");
}

#[test]
fn a_visible_match_does_not_scroll() {
    let engine = foxes();
    let bottom = frame(&engine).row_text(0);
    assert!(engine.search("line 28", true));
    assert_eq!(frame(&engine).row_text(0), bottom);
}

#[test]
fn an_empty_query_clears_the_highlight_and_restarts() {
    let engine = foxes();
    assert_eq!(
        walk(&engine, "Fox", true, 2),
        ["line 27 Fox", "line 12 Fox"]
    );
    assert!(!engine.search("", true));
    assert_eq!(engine.search_spans(), []);
    // The next search starts again from the viewport, where line 12
    // sits in the middle: from its bottom, backward, line 12 is next.
    assert_eq!(walk(&engine, "Fox", true, 1), ["line 12 Fox"]);
    engine.clear_search();
    assert_eq!(engine.search_spans(), []);
}

#[test]
fn every_visible_match_is_highlighted_and_the_current_one_marked() {
    let engine = engine();
    // "Ab" differs in case; "wrapped" wraps from row 1 onto row 2.
    engine.write(format!("ab Ab ab\r\n{}wrapped", "x".repeat(18)).as_bytes());
    let span = |row, start, end, current| Span {
        row,
        start,
        end,
        current,
    };
    assert!(engine.search("ab", false));
    let mut spans = engine.search_spans();
    spans.sort_by_key(|s| (s.row, s.start));
    assert_eq!(spans, [span(0, 0, 2, 1), span(0, 6, 8, 0)]);
    assert!(engine.search("ab", false));
    let mut spans = engine.search_spans();
    spans.sort_by_key(|s| (s.row, s.start));
    assert_eq!(spans, [span(0, 0, 2, 0), span(0, 6, 8, 1)]);

    assert!(engine.search("wrapped", false));
    let mut spans = engine.search_spans();
    spans.sort_by_key(|s| (s.row, s.start));
    assert_eq!(spans, [span(1, 18, 20, 1), span(2, 0, 5, 1)]);
}

#[test]
fn a_wide_character_is_highlighted_across_both_cells() {
    let engine = engine();
    engine.write("a日本b".as_bytes());
    assert!(engine.search("日本", false));
    let span = Span {
        row: 0,
        start: 1,
        end: 5,
        current: 1,
    };
    assert_eq!(engine.search_spans(), [span]);
    // Letters make the match be read back, wide cells included.
    assert!(engine.search("本b", false));
    let span = Span {
        start: 3,
        end: 6,
        ..span
    };
    assert_eq!(engine.search_spans(), [span]);
}

#[test]
fn spaces_in_a_query_are_read_back_as_typed() {
    let engine = engine();
    engine.write(b"Ab cd Ab");
    assert!(engine.search("Ab ", false));
    let span = Span {
        row: 0,
        start: 0,
        end: 3,
        current: 1,
    };
    assert_eq!(engine.search_spans(), [span]);
}

#[test]
fn a_lone_exact_match_among_case_variants_is_found_quickly() {
    let engine = engine();
    engine.resize(120, 40, 8, 16);
    engine.write(b"the only Exact one\r\n");
    let line = "e ".repeat(59);
    for _ in 0..600 {
        engine.write(format!("{line}\r\n").as_bytes());
    }
    let started = std::time::Instant::now();
    // 35 000 candidates; stepping through them took close to a minute.
    assert!(engine.search("E", true));
    assert!(!engine.search("Z", true));
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
}

#[test]
fn the_current_match_survives_a_resize() {
    let engine = foxes();
    assert_eq!(
        walk(&engine, "Fox", true, 2),
        ["line 27 Fox", "line 12 Fox"]
    );
    // Narrower than the line: the match wraps onto a second row.
    assert!(engine.resize(9, 5, 8, 16));
    let current: Vec<_> = engine
        .search_spans()
        .into_iter()
        .filter(|s| s.current != 0)
        .map(|s| (s.end - s.start, s.row))
        .collect();
    assert_eq!(current.iter().map(|c| c.0).sum::<u16>(), 3, "{current:?}");
    // It goes on from there, not from the viewport.
    assert!(engine.resize(20, 5, 8, 16));
    assert_eq!(walk(&engine, "Fox", true, 1), ["line 3 Fox"]);
}

#[test]
fn the_alternate_screen_has_its_own_matches() {
    let engine = foxes();
    assert_eq!(walk(&engine, "Fox", true, 1), ["line 27 Fox"]);
    engine.write(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[3Bfull-screen Fox");
    // Lit there too, but the current match stayed behind.
    let lit = Span {
        row: 3,
        start: 12,
        end: 15,
        current: 0,
    };
    assert_eq!(engine.search_spans(), [lit]);
    assert_eq!(walk(&engine, "Fox", true, 2), ["full-screen Fox"; 2]);
    engine.write(b"\x1b[?1049l");
    assert!(engine.search_spans().iter().all(|s| s.current == 0));
    assert_eq!(walk(&engine, "Fox", true, 1), ["line 27 Fox"]);
}

#[test]
fn overwritten_text_is_no_longer_lit() {
    let engine = engine();
    engine.write(b"Fox\r\nFox");
    assert!(engine.search("Fox", true));
    assert_eq!(engine.search_spans().len(), 2);
    engine.write(b"\x1b[2;1Hdog\x1b[1;1Hcat");
    assert_eq!(engine.search_spans(), []);
}

#[test]
fn the_current_match_follows_new_output() {
    let engine = foxes();
    assert_eq!(walk(&engine, "Fox", true, 1), ["line 27 Fox"]);
    for line in 30..40 {
        engine.write(format!("line {line}\r\n").as_bytes());
    }
    // Line 27 scrolled off: nothing current on screen, but the search
    // goes on from it rather than starting over at the bottom.
    assert!(engine.search_spans().iter().all(|s| s.current == 0));
    assert_eq!(walk(&engine, "Fox", true, 1), ["line 12 Fox"]);
}

#[test]
fn search_leaves_the_mouse_selection_alone() {
    let engine = engine();
    engine.write(b"abc abc");
    drag_select(&engine, (0, 0), (2, 0));
    assert!(engine.search("abc", true));
    assert_eq!(engine.selection_text().as_deref(), Some("abc"));
    let frame = frame(&engine);
    let selected = |col| frame.cell(0, col).unwrap().flags & ffi::CELL_SELECTED != 0;
    assert!(selected(0) && !selected(4));
    assert!(!engine.search_spans().is_empty());
    engine.clear_selection();
    assert!(!engine.search_spans().is_empty(), "the highlight stays");
}

#[test]
fn reset_ends_the_search() {
    let engine = foxes();
    assert!(engine.search("Fox", true));
    engine.reset();
    assert_eq!(engine.search_spans(), []);
    engine.write(b"Fox");
    assert_eq!(engine.search_spans(), [], "no search to follow");
}
