//! Links: [`link_at`] reads OSC 8 hyperlinks from the frame, and
//! [`scan_links`] finds URLs in the text on screen, soft-wrapped rows
//! joined. While mouse events are local, hovering one underlines it and
//! shows its URI in a tooltip, and Ctrl+click opens it through the
//! desktop's URI handler unless [`may_open`] refuses it. A program
//! tracking the mouse keeps both until Shift.

use std::ops::Range;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gio, glib};

use super::input::{route, Route};
use super::paint::Painted;
use super::GhosttyView;
use crate::terminal::ghostty::engine::Frame;
use crate::terminal::ghostty::{ffi, Shared};

/// The mail scheme, whose target is an address, not an authority.
const MAILTO: &str = "mailto:";
/// What starts a link, in any case.
const LINK_SCHEMES: [&str; 3] = ["https://", "http://", MAILTO];
/// Punctuation that reads as the sentence's when it ends a link.
const LINK_TRAILING: &str = ".,;:!?'*([";
/// What a link may hold besides letters, digits and brackets: RFC 3986's
/// other unreserved and reserved characters, and `%`.
const LINK_PUNCTUATION: &str = "-._~:/?#@!$&'*+,;=%";
/// The cursor over a link.
pub(super) const LINK_CURSOR: &str = "pointer";

/// Opens a link's URI; [`launch_uri`] unless a test stands in.
pub(super) type Launcher = Rc<dyn Fn(&GhosttyView, &str)>;

impl GhosttyView {
    /// A local release at `cell`: a Ctrl+press on a link opens it, unless
    /// the press became a selection, the link is no longer here, or
    /// [`may_open`] refuses it.
    pub(super) fn open_pressed_link(&self, shared: &Shared, (col, row): (u16, u16)) {
        let imp = self.imp();
        let Some(link) = imp.pressed_link.take() else {
            return;
        };
        let selected = shared
            .engine
            .selection_text()
            .is_some_and(|text| !text.is_empty());
        let here = link_at(&imp.frame.borrow(), row.into(), col.into());
        if selected || here.as_ref() != Some(&link) {
            return;
        }
        if !may_open(&link.uri, &glib::host_name()) {
            eprintln!("Wayterm: refused a link that is not for this machine");
            return;
        }
        let launcher = imp.launcher.borrow().clone();
        if let Some(launch) = launcher {
            launch(self, &link.uri);
        }
    }

    /// The link in `frame` under the pointer. A program tracking the mouse
    /// owns it, so links wait for the Shift that keeps events local.
    pub(super) fn link_under(&self, shared: &Shared, frame: &Frame) -> Option<Link> {
        let state = self.imp().hover.get()?;
        if route(shared.engine.mouse_modes(), state, false) != Route::Local {
            return None;
        }
        let (x, y) = self.imp().pointer.get();
        let (col, row) = self.cell_at(x, y);
        link_at(frame, row.into(), col.into())
    }

    /// Make `link` the one underlined, with the cursor and the tooltip to
    /// match; `true` if that changed anything.
    pub(super) fn show_link(&self, link: Option<Link>) -> bool {
        let current = &self.imp().link;
        if *current.borrow() == link {
            return false;
        }
        self.set_cursor_from_name(link.is_some().then_some(LINK_CURSOR));
        self.set_tooltip_text(link.as_ref().map(|link| link.uri.as_str()));
        current.replace(link);
        true
    }
}

/// A link on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Link {
    uri: String,
    /// Its viewport cells, row-major and ascending: the cells of one
    /// hyperlink need not touch.
    cells: Vec<usize>,
}

impl Link {
    /// Underline this link's cells among `cells`, viewport row `row`.
    pub(super) fn underline(&self, row: usize, cells: &mut [Painted<'_>]) {
        let line = row * cells.len();
        for (col, cell) in cells.iter_mut().enumerate() {
            if self.cells.binary_search(&(line + col)).is_ok() {
                // Single, in the text color; main's renderer draws it per
                // run of equal style.
                cell.underline = ffi::UNDERLINE_SINGLE;
                cell.underline_rgb = None;
            }
        }
    }
}

/// The link covering a viewport cell, if any: a program's OSC 8
/// hyperlink, else a URL [`scan_links`] finds in the logical line, the
/// rows soft-wrapped together around `row`, scanned whole.
pub(super) fn link_at(frame: &Frame, row: usize, col: usize) -> Option<Link> {
    let (cols, rows) = (usize::from(frame.info.cols), usize::from(frame.info.rows));
    if row >= rows || col >= cols {
        return None;
    }
    // A program's OSC 8 hyperlink wins over a URL in the plain text.
    if let Some(uri) = frame.link(row, col) {
        let entry = frame.cell(row, col)?.link;
        let cells = frame
            .cells
            .iter()
            .take(cols * rows)
            .enumerate()
            .filter(|(_, cell)| cell.link == entry)
            .map(|(index, _)| index)
            .collect();
        return Some(Link {
            uri: uri.to_owned(),
            cells,
        });
    }
    let wraps = |row| {
        frame
            .cell(row, cols - 1)
            .is_some_and(|cell| cell.wraps != 0)
    };
    let mut first = row;
    while first > 0 && wraps(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows && wraps(last) {
        last += 1;
    }
    let line: Vec<char> = (first..=last)
        .flat_map(|row| (0..cols).map(move |col| link_char(frame, row, col)))
        .collect();
    let target = (row - first) * cols + col;
    let range = scan_links(&line, wraps(last))
        .into_iter()
        .find(|range| range.contains(&target))?;
    let start = first * cols;
    Some(Link {
        uri: line[range.clone()].iter().collect(),
        cells: (start + range.start..start + range.end).collect(),
    })
}

/// Whether a link may reach the desktop's handler on this machine, `host`
/// by name. Links come from whatever is on the far side of the tab, which
/// the view cannot see, and a `file:` URI there means a file over there. So
/// one opens only if its authority is exactly `host`: an empty authority,
/// or `localhost`, is the sender's own machine. Text without a scheme is
/// nothing a handler can take.
fn may_open(uri: &str, host: &str) -> bool {
    match glib::Uri::parse_scheme(uri) {
        Some(scheme) if scheme.eq_ignore_ascii_case("file") => {
            !host.is_empty() && file_authority(uri) == Some(host)
        }
        Some(_) => true,
        None => false,
    }
}

/// The authority of a `file:` URI, empty in `file:///path`; `None` for the
/// forms with none at all, like `file:/path`.
fn file_authority(uri: &str) -> Option<&str> {
    let (_, rest) = uri.split_once(':')?;
    rest.strip_prefix("//")?.split(['/', '?', '#']).next()
}

/// What a cell adds to a logical line, one `char` per cell so an index maps
/// back to its cell: its character, or a space, which ends a link, for a
/// cell that holds none, a whole cluster, or text a program hid.
fn link_char(frame: &Frame, row: usize, col: usize) -> char {
    let hidden = frame
        .cell(row, col)
        .is_some_and(|cell| cell.flags & ffi::CELL_INVISIBLE != 0);
    match frame.grapheme(row, col) {
        &[codepoint] if !hidden => char::from_u32(codepoint).unwrap_or(' '),
        _ => ' ',
    }
}

/// The links in a logical line, as ranges of it. Prose must never open
/// anything, so text is a link only if all of this holds:
///
/// - It starts with one of [`LINK_SCHEMES`], in any case, and no scheme
///   character comes right before that, so `git+https://` is not one.
/// - It runs over ASCII URL characters. Anything else ends it, and so does
///   a closing bracket that closes nothing in the link: `(https://a.b/c)`
///   stops before the `)`, `https://a.b/c_(d)` keeps it.
/// - [`LINK_TRAILING`] punctuation at its end is left out.
/// - What remains has a target: a host for `http` and `https`, an address
///   for `mailto`.
///
/// `cut_off` says the line goes on past `line`'s end, below the viewport:
/// a link reaching that far is left out, its tail unknown.
fn scan_links(line: &[char], cut_off: bool) -> Vec<Range<usize>> {
    let mut links = Vec::new();
    let mut start = 0;
    while start < line.len() {
        let Some(scheme) = scheme_at(line, start) else {
            start += 1;
            continue;
        };
        let target = start + scheme.len();
        let run = link_run(line, target);
        let mut end = run;
        while end > target && LINK_TRAILING.contains(line[end - 1]) {
            end -= 1;
        }
        let text: String = line[target..end].iter().collect();
        if !(cut_off && run == line.len()) && link_target_valid(scheme, &text) {
            links.push(start..end);
        }
        start = run;
    }
    links
}

/// The scheme of [`LINK_SCHEMES`] starting at `at`, on a word's start.
fn scheme_at(line: &[char], at: usize) -> Option<&'static str> {
    let in_word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.');
    if at > 0 && in_word(line[at - 1]) {
        return None;
    }
    LINK_SCHEMES.into_iter().find(|scheme| {
        line.get(at..at + scheme.len()).is_some_and(|text| {
            text.iter()
                .zip(scheme.chars())
                .all(|(c, s)| c.eq_ignore_ascii_case(&s))
        })
    })
}

/// Where the URL characters starting at `from` end.
fn link_run(line: &[char], from: usize) -> usize {
    let mut open = Vec::new();
    for (at, &c) in line.iter().enumerate().skip(from) {
        match c {
            '(' | '[' => open.push(c),
            ')' if open.pop() != Some('(') => return at,
            ']' if open.pop() != Some('[') => return at,
            ')' | ']' => {}
            c if c.is_ascii_alphanumeric() || LINK_PUNCTUATION.contains(c) => {}
            _ => return at,
        }
    }
    line.len()
}

/// Whether `text`, what follows `scheme`, goes anywhere.
fn link_target_valid(scheme: &str, text: &str) -> bool {
    if scheme == MAILTO {
        let address = text.split('?').next().unwrap_or(text);
        address
            .split_once('@')
            .is_some_and(|(user, domain)| !user.is_empty() && host_valid(domain))
    } else {
        let authority = text.split(['/', '?', '#']).next().unwrap_or(text);
        authority_valid(authority)
    }
}

/// `[user@]host[:port]`, the host a name or a bracketed IPv6 address.
fn authority_valid(authority: &str) -> bool {
    let address = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) if !address.ends_with(']') => (host, Some(port)),
        _ => (address, None),
    };
    let ipv6 = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .is_some_and(|host| host.parse::<std::net::Ipv6Addr>().is_ok());
    let port_valid =
        port.is_none_or(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
    port_valid && (ipv6 || host_valid(host))
}

/// Dot-separated labels of letters, digits and inner hyphens.
fn host_valid(host: &str) -> bool {
    host.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Hand `uri` to the desktop's URI handler and go on: the handler runs
/// outside Wayterm, which stays open. The failure is logged without the
/// error's text, which may quote the URI, and a URI can carry a token.
pub(super) fn launch_uri(view: &GhosttyView, uri: &str) {
    let parent = view.root().and_downcast::<gtk4::Window>();
    gtk4::UriLauncher::new(uri).launch(parent.as_ref(), gio::Cancellable::NONE, |result| {
        if result.is_err() {
            eprintln!("Wayterm: could not open a link");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::paint::painted_row;
    use super::*;
    use crate::terminal::ghostty::engine::{Engine, Scroll};
    use crate::terminal::options::GhosttyOptions;

    /// A 20-column screen after `output`.
    fn screen(output: &str) -> Frame {
        let engine = Engine::new(20, 3).expect("engine");
        engine.write(output.as_bytes());
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        frame
    }

    #[test]
    fn a_hyperlink_covers_its_cells_and_no_others() {
        let frame = screen("a \x1b]8;;https://a.example/\x1b\\link\x1b]8;;\x1b\\ b");
        let link = link_at(&frame, 0, 3).expect("a link");
        assert_eq!(link.uri, "https://a.example/");
        assert_eq!(link.cells, [2, 3, 4, 5]);
        assert_eq!(link_at(&frame, 0, 1), None);
        assert_eq!(link_at(&frame, 0, 6), None);
    }

    #[test]
    fn a_hyperlink_broken_over_rows_is_one_link() {
        let open = "\x1b]8;;https://a.example/\x1b\\";
        let frame = screen(&format!("{open}ab\x1b]8;;\x1b\\\r\n{open}cd\x1b]8;;\x1b\\"));
        let link = link_at(&frame, 1, 0).expect("a link");
        assert_eq!(link.cells, [0, 1, 20, 21]);
    }

    #[test]
    fn text_that_reads_as_a_url_is_not_an_osc_8_hyperlink() {
        // The frame carries no OSC 8 link: [`scan_links`] may still find a
        // URL in the plain text at [`link_at`]'s fallback.
        let frame = screen("https://a.example/");
        assert_eq!(frame.cell(0, 3).map(|cell| cell.link), Some(0));
        assert_eq!(frame.link(0, 3), None);
    }

    #[test]
    fn cells_past_the_frame_hold_no_link() {
        let frame = screen("\x1b]8;;https://a.example/\x1b\\x\x1b]8;;\x1b\\");
        assert_eq!(link_at(&frame, 3, 0), None);
        assert_eq!(link_at(&frame, 0, 20), None);
    }

    #[test]
    fn only_a_file_link_naming_this_host_may_open() {
        let may = |uri| may_open(uri, "here");
        assert!(may("file://here/etc/hosts"));
        assert!(may("FILE://here/etc/hosts"));
        assert!(may("file://here"));
        for uri in [
            "file://there/etc/hosts",
            "FILE://there/etc/hosts",
            "file://HERE/etc/hosts",
            "file://here.example/etc/hosts",
            "file://user@here/etc/hosts",
            "file://here:80/etc/hosts",
            "file://here\\@there/etc/hosts",
            "file://localhost/etc/hosts",
            "file:///etc/hosts",
            "file:/etc/hosts",
            "file:etc/hosts",
        ] {
            assert!(!may(uri), "{uri}");
        }
    }

    #[test]
    fn a_host_with_no_name_opens_no_file_link() {
        assert!(!may_open("file:///etc/hosts", ""));
    }

    #[test]
    fn other_schemes_may_open_and_text_without_one_may_not() {
        let may = |uri| may_open(uri, "here");
        assert!(may("https://example.com/"));
        assert!(may("mailto:a@example.com"));
        assert!(!may(" file://there/etc/hosts"));
        assert!(!may("/etc/hosts"));
        assert!(!may(""));
    }

    /// The links `scan_links` finds in `text`.
    fn links(text: &str) -> Vec<String> {
        let line: Vec<char> = text.chars().collect();
        scan_links(&line, false)
            .into_iter()
            .map(|range| line[range].iter().collect())
            .collect()
    }

    /// `text` written to a `cols` × `rows` terminal, as a frame.
    fn written(cols: u16, rows: u16, text: &str) -> Frame {
        let engine = Engine::new(cols, rows).expect("engine");
        engine.write(text.as_bytes());
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        frame
    }

    fn uri_at(frame: &Frame, row: usize, col: usize) -> Option<String> {
        link_at(frame, row, col).map(|link| link.uri)
    }

    #[test]
    fn the_known_schemes_make_links() {
        for url in [
            "https://example.com",
            "http://example.com/a/b?c=d&e=%20#f",
            "http://localhost:8080/",
            "http://192.168.0.1/x",
            "http://[::1]:8080/x",
            "https://user@example.com/",
            "HTTPS://EXAMPLE.COM/",
            "mailto:someone@example.com",
            "mailto:someone@example.com?subject=hi",
        ] {
            assert_eq!(links(url), [url]);
            assert_eq!(links(&format!("see {url} now")), [url]);
        }
        assert_eq!(
            links("http://a.example mailto:b@c.example https://d.example"),
            [
                "http://a.example",
                "mailto:b@c.example",
                "https://d.example"
            ]
        );
    }

    #[test]
    fn anything_else_is_no_link() {
        for text in [
            "",
            "   ",
            "example.com",
            "www.example.com",
            "ftp://example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ssh://example.com",
            // The scheme must start the word.
            "xhttps://example.com",
            "git+https://example.com",
            "no-mailto:a@b.example",
            // Nothing to go to.
            "https://",
            "https:///path",
            "https://.",
            "https://...",
            "https://-",
            "https://?q",
            "https://:80",
            "https://example.com:port/",
            "http://[nope]/",
            "mailto:",
            "mailto:nobody",
            "mailto:@example.com",
            "mailto:someone@",
            "mailto:a@b@c.example",
        ] {
            assert_eq!(links(text), [] as [&str; 0], "{text:?}");
        }
    }

    #[test]
    fn trailing_punctuation_stays_out_of_a_link() {
        let url = "https://example.com/a";
        for text in [
            "https://example.com/a.",
            "https://example.com/a,",
            "https://example.com/a;",
            "https://example.com/a:",
            "https://example.com/a!",
            "https://example.com/a?",
            "https://example.com/a...",
            "https://example.com/a).",
            "(https://example.com/a)",
            "(https://example.com/a).",
            "(see https://example.com/a), then",
            "(https://example.com/a),more",
            "[https://example.com/a]",
            "<https://example.com/a>",
            "\"https://example.com/a\"",
            "'https://example.com/a'",
            "`https://example.com/a`",
            "*https://example.com/a*",
            "https://example.com/a(",
        ] {
            assert_eq!(links(text), [url], "{text:?}");
        }
    }

    #[test]
    fn closed_brackets_and_inner_punctuation_stay_in_a_link() {
        for url in [
            "https://en.example.org/wiki/Rust_(language)",
            "https://example.com/a[1]/b",
            "https://example.com/a.b,c;d:e!f",
            "https://example.com/~me/?q=(a)(b)",
        ] {
            assert_eq!(links(url), [url]);
            assert_eq!(links(&format!("({url})")), [url]);
        }
    }

    #[test]
    fn a_link_stops_at_what_a_url_cannot_hold() {
        for (text, url) in [
            ("https://example.com/a b", "https://example.com/a"),
            ("https://example.com/a\"b", "https://example.com/a"),
            ("https://example.com/a<b", "https://example.com/a"),
            ("https://example.com/a|b", "https://example.com/a"),
            ("https://example.com/a\\b", "https://example.com/a"),
            ("https://example.com/\u{e9}", "https://example.com/"),
            (
                "https://example.com/\u{65e5}\u{672c}",
                "https://example.com/",
            ),
        ] {
            assert_eq!(links(text), [url], "{text:?}");
        }
        // Non-ASCII before the scheme is no scheme character.
        assert_eq!(
            links("\u{65e5}\u{672c}https://example.com/"),
            ["https://example.com/"]
        );
    }

    #[test]
    fn a_link_running_off_the_line_end_is_dropped_when_cut_off() {
        let scan = |text: &str, cut_off| {
            let line: Vec<char> = text.chars().collect();
            scan_links(&line, cut_off).len()
        };
        assert_eq!(scan("https://example.com/a", false), 1);
        assert_eq!(scan("https://example.com/a", true), 0);
        // Trailing punctuation does not hide that it runs on.
        assert_eq!(scan("https://example.com/a.", true), 0);
        // One that ended before the cut stands.
        assert_eq!(scan("https://example.com/a b", true), 1);
        assert_eq!(scan("(https://example.com/a)", true), 1);
    }

    #[test]
    fn the_snapshot_marks_soft_wrapped_rows_on_their_last_cell() {
        let frame = written(10, 4, "0123456789abc\r\n0123456789\r\nx");
        let wraps = |row, col| frame.cell(row, col).unwrap().wraps != 0;
        assert!(wraps(0, 9));
        assert!(!wraps(0, 8));
        assert!(!wraps(1, 9), "a soft wrap's last row");
        assert!(!wraps(2, 9), "full, but ended by a line feed");
    }

    #[test]
    fn a_soft_wrapped_link_is_one_link_from_any_of_its_cells() {
        let url = "https://example.com/some/long/path";
        let frame = written(16, 4, &format!("go {url} ok"));
        for (row, col) in [(0, 3), (0, 15), (1, 0), (1, 15), (2, 0), (2, 4)] {
            assert_eq!(
                uri_at(&frame, row, col).as_deref(),
                Some(url),
                "{row},{col}"
            );
        }
        assert_eq!(uri_at(&frame, 0, 2), None);
        assert_eq!(uri_at(&frame, 2, 6), None, "the word after it");
        assert_eq!(uri_at(&frame, 3, 0), None);
        let link = link_at(&frame, 1, 0).unwrap();
        assert_eq!(link.cells, (3..3 + url.len()).collect::<Vec<usize>>());
    }

    #[test]
    fn a_hard_line_break_ends_a_link_at_the_last_column() {
        let url = "https://example.com/";
        let frame = written(20, 3, &format!("{url}\r\nmore"));
        assert_eq!(uri_at(&frame, 0, 0).as_deref(), Some(url));
        assert_eq!(uri_at(&frame, 1, 0), None);
    }

    #[test]
    fn a_link_cut_by_the_bottom_of_the_viewport_is_not_opened() {
        let engine = Engine::new(16, 2).expect("engine");
        engine.write(b"https://example.com/some/long/path");
        let mut frame = Frame::default();
        // The bottom of the screen: the scheme is gone above it.
        assert!(engine.snapshot(&mut frame));
        assert_eq!(uri_at(&frame, 0, 0), None);
        // Scrolled back: the tail is gone below it.
        engine.scroll(Scroll::By(-1));
        assert!(engine.snapshot(&mut frame));
        assert!(frame.row_text(0).starts_with("https://"));
        assert_eq!(uri_at(&frame, 0, 0), None);
    }

    #[test]
    fn cells_that_hold_no_single_character_end_a_link() {
        // Nothing, or off the grid.
        let frame = written(20, 2, "");
        assert_eq!(uri_at(&frame, 0, 0), None);
        assert_eq!(uri_at(&frame, 5, 0), None);
        assert_eq!(uri_at(&frame, 0, 50), None);
        assert_eq!(link_at(&Frame::default(), 0, 0), None);

        // Wide characters before and after, and their tail cells.
        let frame = written(40, 2, "\u{65e5}\u{672c}https://example.com/a\u{65e5}");
        assert_eq!(
            uri_at(&frame, 0, 4).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(uri_at(&frame, 0, 1), None);
        assert_eq!(uri_at(&frame, 0, 25), None);

        // A cluster is not the character it starts with.
        let frame = written(40, 2, "https://example.com/e\u{301}x");
        assert_eq!(
            uri_at(&frame, 0, 0).as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(uri_at(&frame, 0, 20), None);

        // Text a program hid is not there to click.
        let frame = written(40, 2, "\x1b[8mhttps://example.com/\x1b[0m");
        assert_eq!(uri_at(&frame, 0, 0), None);
    }

    #[test]
    fn a_link_underlines_its_cells_on_each_of_its_rows() {
        let frame = written(8, 3, "ab http://c.example d");
        let link = link_at(&frame, 0, 3).unwrap();
        let underlined = |row| -> Vec<bool> {
            let mut cells = painted_row(&frame, row, &[], false, &GhosttyOptions::default());
            link.underline(row, &mut cells);
            cells
                .iter()
                .map(|cell| cell.underline != ffi::UNDERLINE_NONE)
                .collect()
        };
        let cells = |n: usize, start: usize, end: usize| -> Vec<bool> {
            (0..n).map(|col| (start..end).contains(&col)).collect()
        };
        assert_eq!(link.uri, "http://c.example");
        assert_eq!(underlined(0), cells(8, 3, 8));
        assert_eq!(underlined(1), cells(8, 0, 8));
        assert_eq!(underlined(2), cells(8, 0, 3));
    }
}
