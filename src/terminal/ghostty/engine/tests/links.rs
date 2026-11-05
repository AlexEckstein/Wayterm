use super::*;

/// `text` as an OSC 8 hyperlink to `uri`.
fn hyperlink(uri: &str, text: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
}

#[test]
fn hyperlink_cells_resolve_to_their_uri() {
    let e = engine();
    let link = hyperlink("https://example.com/a", "ab");
    e.write(format!("x{link}y").as_bytes());
    let f = frame(&e);
    assert_eq!(f.link(0, 0), None);
    assert_eq!(f.link(0, 1), Some("https://example.com/a"));
    assert_eq!(f.link(0, 2), Some("https://example.com/a"));
    assert_eq!(f.link(0, 3), None);
    assert_eq!(f.row_text(0).trim_end(), "xaby");
}

#[test]
fn plain_text_that_looks_like_a_url_is_no_hyperlink() {
    let e = engine();
    e.write(b"https://example.com/a");
    let f = frame(&e);
    assert_eq!(f.cell(0, 3).map(|cell| cell.link), Some(0));
    assert_eq!(f.link(0, 3), None);
}

#[test]
fn a_link_shares_one_entry_and_each_uri_keeps_its_own() {
    let e = engine();
    let a = hyperlink("https://a.example/", "aaa");
    let b = hyperlink("https://b.example/", "bbb");
    e.write(format!("{a} {b}").as_bytes());
    let f = frame(&e);
    let entry = |col| f.cell(0, col).map(|cell| cell.link);
    assert_eq!(entry(0), entry(2));
    assert_ne!(entry(0), entry(4));
    assert_eq!(f.link(0, 2), Some("https://a.example/"));
    assert_eq!(f.link(0, 4), Some("https://b.example/"));
}

#[test]
fn a_wrapped_link_is_one_entry_on_both_rows() {
    let e = Engine::new(4, 3).expect("engine");
    e.write(hyperlink("https://example.com/", "abcdef").as_bytes());
    let f = frame(&e);
    assert_eq!(f.cell(0, 0).map(|c| c.link), f.cell(1, 1).map(|c| c.link));
    assert_eq!(f.link(1, 1), Some("https://example.com/"));
}

#[test]
fn a_wide_character_keeps_its_link_on_both_cells() {
    let e = engine();
    e.write(hyperlink("https://example.com/", "日本").as_bytes());
    let f = frame(&e);
    for col in 0..4 {
        assert_eq!(f.link(0, col), Some("https://example.com/"), "column {col}");
    }
}

#[test]
fn links_follow_the_viewport_through_scrollback() {
    let e = engine();
    e.write(format!("{}\r\n", hyperlink("https://example.com/", "top")).as_bytes());
    for line in 0..40 {
        e.write(format!("line {line}\r\n").as_bytes());
    }
    assert_eq!(frame(&e).link(0, 0), None);
    e.scroll(Scroll::Top);
    assert_eq!(frame(&e).link(0, 0), Some("https://example.com/"));
}

#[test]
fn a_frame_reused_after_a_reset_holds_no_links() {
    let e = engine();
    e.write(hyperlink("https://example.com/", "ab").as_bytes());
    let mut f = frame(&e);
    assert!(f.link(0, 0).is_some());
    e.reset();
    e.write(b"ab");
    assert!(e.snapshot(&mut f));
    assert_eq!(f.link(0, 0), None);
}

#[test]
fn a_screenful_of_long_distinct_links_all_resolve() {
    let e = Engine::new(20, 24).expect("engine");
    let uri = |n: usize| format!("https://example.com/{}/{n}", "x".repeat(200));
    for n in 0..23 {
        e.write(format!("{}\r\n", hyperlink(&uri(n), "link")).as_bytes());
    }
    let f = frame(&e);
    for n in 0..23 {
        assert_eq!(f.link(n, 0), Some(uri(n).as_str()), "row {n}");
    }
}

#[test]
fn a_uri_that_is_not_utf8_is_no_link() {
    let e = engine();
    e.write(b"\x1b]8;;https://ex\xffample/\x1b\\ab\x1b]8;;\x1b\\");
    assert_eq!(frame(&e).link(0, 0), None);
}
