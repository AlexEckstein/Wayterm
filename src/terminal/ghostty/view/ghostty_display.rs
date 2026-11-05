use std::ops::Range;
use std::time::Duration;

use gtk4::graphene;
use tokio::sync::mpsc;

use super::links::LINK_CURSOR;
use super::paint::{painted_row, run_text};
use super::*;
use crate::macros::recorder::is_pointer_or_focus_report;
use crate::terminal::defaults::{GhosttyOptions, SearchColors};
use crate::terminal::ghostty::engine::Scroll;
use crate::terminal::ghostty::ffi;
use crate::terminal::{
    create_terminal, install_backend_preference, BackendKind, GhosttyBackend, TerminalBackend,
    TerminalDefaults, TRANSLUCENT_CLASS,
};
use crate::ui::appearance::install_terminal_defaults;
use crate::ui::translucency::gtk_tests::alpha_at;

/// The view paints a frame, reports its grid once allocated, and turns
/// a key press into `DataOut`.
#[test]
#[ignore = "needs a display; run under xvfb-run"]
fn view_draws_sizes_and_types() {
    gtk4::init().expect("gtk init");
    let (tx, mut rx) = mpsc::channel(16);
    let backend = GhosttyBackend::new(tx).expect("engine");
    let window = gtk4::Window::new();
    window.set_default_size(400, 200);
    window.set_child(Some(&backend.widget()));
    window.present();
    backend.write(b"\x1b[1mhello\x1b[0m");
    let context = glib::MainContext::default();
    for _ in 0..50 {
        context.iteration(false);
    }

    let view = backend.widget().downcast::<GhosttyView>().unwrap();
    assert!(view.width() > 0 && view.height() > 0);
    let events: Vec<TerminalEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        events.iter().any(
            |e| matches!(e, TerminalEvent::SizeChange { rows, cols } if *rows > 0 && *cols > 0)
        ),
        "{events:?}"
    );
    assert_eq!(view.imp().frame.borrow().row_text(0).trim_end(), "hello");

    // Opacity fades the default background only.
    backend.write(b"\r\n\x1b[41m  \x1b[0m");
    backend.set_opacity(0.5);
    assert!(view.has_css_class(TRANSLUCENT_CLASS));
    // The paintable reads the last frame drawn: let the clock tick.
    for _ in 0..50 {
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (cell_w, cell_h) = view.cell_size();
    let faded = alpha_at(&view, view.width() - 2, view.height() - 2);
    assert!((120..=135).contains(&faded), "default background {faded}");
    assert_eq!(alpha_at(&view, cell_w / 2, cell_h + cell_h / 2), 255);
    backend.set_opacity(1.0);
    assert!(!view.has_css_class(TRANSLUCENT_CLASS));

    // A new cell size reaches the engine with the grid unchanged.
    backend.write(b"\x1b[?2048h");
    while rx.try_recv().is_ok() {}
    let (cols, rows) = view.shared().unwrap().engine.size();
    backend.set_font(&pango::FontDescription::from_string("Monospace 20"));
    let (cell_w, cell_h) = view.cell_size();
    let (width, height) = (i32::from(cols) * cell_w, i32::from(rows) * cell_h);
    view.fit_grid(width, height);
    backend.write(b"");
    let report = format!("\x1b[48;{rows};{cols};{height};{width}t").into_bytes();
    assert_eq!(rx.try_recv(), Ok(TerminalEvent::DataOut(report)));
    backend.write(b"\x1b[?2048l");
    backend.set_font(&pango::FontDescription::from_string(DEFAULT_FONT));
    view.fit_grid(view.width(), view.height());
    while rx.try_recv().is_ok() {}

    assert!(view.key(gdk::Key::a, gdk::ModifierType::empty()));
    assert_eq!(
        rx.try_recv().unwrap(),
        TerminalEvent::DataOut(b"a".to_vec())
    );
    assert!(!view.key(gdk::Key::Shift_L, gdk::ModifierType::empty()));

    // Shift+PageUp scrolls the viewport and sends nothing.
    for line in 0..200 {
        backend.write(format!("line {line}\r\n").as_bytes());
    }
    let top_row = || {
        let mut frame = Frame::default();
        assert!(view.shared().unwrap().engine.snapshot(&mut frame));
        frame.row_text(0)
    };
    let bottom = top_row();
    assert!(view.key(gdk::Key::Page_Up, gdk::ModifierType::SHIFT_MASK));
    assert_ne!(top_row(), bottom);
    assert!(view.key(gdk::Key::End, gdk::ModifierType::SHIFT_MASK));
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|e| matches!(e, TerminalEvent::DataOut(_))),
        "scroll keys stay local"
    );
    assert_eq!(top_row(), bottom);

    backend.send_input(b"ls\r");
    assert_eq!(
        rx.try_recv().unwrap(),
        TerminalEvent::DataOut(b"ls\r".to_vec())
    );
    mouse_routing(&backend, &view, &mut rx);
    other_buttons(&backend, &view, &mut rx);
    sideways_wheel(&backend, &view, &mut rx);
    glyphs_on_the_grid(&backend, &view);
    cursor_shapes(&backend, &view);
    underline_styles(&backend, &view);
    clusters_shape_whole(&backend, &view);
    shell_watchers(&backend, &view);
    program_clipboard(&backend, &view, &mut rx);
    search_highlight(&backend, &view);
    blinking_text(&backend, &view);
    installed_options(&backend, &view);
    primary_selection(&backend, &view, &mut rx);
    kitty_images(&backend, &view);
    hyperlinks_hover_and_open(&backend, &view, &mut rx);
    links_hover_and_open(&backend, &view, &mut rx);
    window.close();

    // The factory follows the installed preference; VTE is the default.
    install_backend_preference(BackendKind::Ghostty);
    assert_eq!(create_terminal().backend.name(), "ghostty");
    install_backend_preference(BackendKind::Vte);
    assert_eq!(create_terminal().backend.name(), "vte");
}

/// Buttons, the wheel and focus changes go local or to the program as
/// its modes say; called from the test above, which owns GTK.
fn mouse_routing(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let mut chunks = || -> Vec<Vec<u8>> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                TerminalEvent::DataOut(bytes) => Some(bytes),
                _ => None,
            })
            .collect()
    };
    let none = gdk::ModifierType::empty();
    let shift = gdk::ModifierType::SHIFT_MASK;
    let (cell_w, cell_h) = view.cell_size();
    let at = |col: i32, row: i32| {
        (
            f64::from(col * cell_w + cell_w / 2),
            f64::from(row * cell_h + cell_h / 2),
        )
    };
    let engine = || view.shared().unwrap().engine.selection_text();
    let top_row = || {
        let mut frame = Frame::default();
        assert!(view.shared().unwrap().engine.snapshot(&mut frame));
        frame.row_text(0)
    };
    backend.reset();
    for line in 0..200 {
        backend.write(format!("line {line}\r\n").as_bytes());
    }
    chunks();

    // No tracking: a drag selects and the wheel scrolls, sending nothing.
    view.press(none, 1000, at(0, 0));
    view.drag_to(none, at(2, 0));
    view.release(none, at(2, 0));
    assert!(engine().is_some_and(|text| !text.is_empty()));
    let bottom = top_row();
    view.wheel(-1.0, gdk::ScrollUnit::Wheel, none);
    assert_ne!(top_row(), bottom);
    assert_eq!(chunks().concat(), b"");
    backend.scroll_to_bottom();

    // Tracking with SGR: press, drag and release are reported, nothing
    // is selected, and the wheel is three wheel-up reports.
    backend.write(b"\x1b[?1002h\x1b[?1006h");
    view.press(none, 2000, at(1, 1));
    view.drag_to(none, at(1, 1));
    view.drag_to(none, at(3, 1));
    view.release(none, at(3, 1));
    assert_eq!(engine(), None);
    let reports = chunks();
    assert_eq!(reports.concat(), b"\x1b[<0;2;2M\x1b[<32;4;2M\x1b[<0;4;2m");
    // Each report is a chunk of its own, which macro recording skips.
    assert_eq!(reports.len(), 3);
    assert!(reports.iter().all(|r| is_pointer_or_focus_report(r)));
    view.hover(none, at(0, 0));
    assert_eq!(chunks().concat(), b"", "1002 reports no bare motion");
    view.wheel(-1.0, gdk::ScrollUnit::Wheel, none);
    assert_eq!(
        chunks(),
        vec![b"\x1b[<64;1;1M".to_vec(); 3],
        "a report per chunk"
    );
    assert_eq!(top_row(), bottom, "the viewport stays put");

    // Shift keeps the drag and the wheel local.
    view.press(shift, 3000, at(0, 0));
    view.drag_to(shift, at(2, 0));
    view.release(shift, at(2, 0));
    assert!(engine().is_some_and(|text| !text.is_empty()));
    view.wheel(-1.0, gdk::ScrollUnit::Wheel, shift);
    assert_ne!(top_row(), bottom);
    assert_eq!(chunks().concat(), b"");
    backend.scroll_to_bottom();

    // Alternate screen, no tracking: the wheel sends arrow keys.
    backend.write(b"\x1b[?1002l\x1b[?1006l\x1b[?1049h");
    view.wheel(1.0, gdk::ScrollUnit::Wheel, none);
    assert_eq!(chunks().concat(), b"\x1b[B".repeat(3));
    backend.write(b"\x1b[?1007l");
    view.wheel(1.0, gdk::ScrollUnit::Wheel, none);
    assert_eq!(chunks().concat(), b"", "no alternate scroll");
    backend.write(b"\x1b[?1049l");

    // Focus changes only with mode 1004.
    view.focus_changed(true);
    assert_eq!(chunks().concat(), b"");
    backend.write(b"\x1b[?1004h");
    view.focus_changed(false);
    view.focus_changed(true);
    let reports = chunks();
    assert_eq!(reports.concat(), b"\x1b[O\x1b[I");
    assert!(reports.iter().all(|r| is_pointer_or_focus_report(r)));
}

/// The middle and right buttons reach a tracking program as the
/// primary does, and do nothing new otherwise.
fn other_buttons(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let mut chunks = || -> Vec<Vec<u8>> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                TerminalEvent::DataOut(bytes) => Some(bytes),
                _ => None,
            })
            .collect()
    };
    let none = gdk::ModifierType::empty();
    let shift = gdk::ModifierType::SHIFT_MASK;
    let (cell_w, cell_h) = view.cell_size();
    let at = |col: i32, row: i32| {
        (
            f64::from(col * cell_w + cell_w / 2),
            f64::from(row * cell_h + cell_h / 2),
        )
    };
    let selected = || view.shared().unwrap().engine.selection_text();
    let buttons = [(ffi::BUTTON_MIDDLE, 1), (ffi::BUTTON_RIGHT, 2)];
    backend.reset();
    backend.write(b"alpha beta gamma");
    chunks();

    // A gesture per button, and a wheel on both axes.
    let controllers = view.observe_controllers();
    let controllers = || (0..controllers.n_items()).filter_map(|i| controllers.item(i));
    let drags: Vec<u32> = controllers()
        .filter_map(|c| c.downcast::<gtk4::GestureDrag>().ok())
        .map(|drag| drag.button())
        .collect();
    for button in [
        gdk::BUTTON_PRIMARY,
        gdk::BUTTON_MIDDLE,
        gdk::BUTTON_SECONDARY,
    ] {
        assert!(drags.contains(&button), "{button} in {drags:?}");
    }
    assert!(controllers()
        .filter_map(|c| c.downcast::<gtk4::EventControllerScroll>().ok())
        .any(|wheel| wheel
            .flags()
            .contains(gtk4::EventControllerScrollFlags::BOTH_AXES)));

    // No tracking: nothing is sent, and the selection stays.
    view.press(none, 40_000, at(0, 0));
    view.drag_to(none, at(2, 0));
    view.release(none, at(2, 0));
    let selection = selected();
    assert!(selection.is_some());
    chunks();
    for (button, _) in buttons {
        view.button_press(button, none, at(1, 1));
        view.button_drag(button, none, at(3, 1));
        view.button_release(button, none, at(3, 1));
    }
    assert_eq!(chunks().concat(), b"");
    assert_eq!(selected(), selection);

    backend.write(b"\x1b[?1002h\x1b[?1006h");
    for (button, code) in buttons {
        // Press, drag and release, each report a chunk of its own; a
        // reported press clears the selection, as the primary's does.
        view.button_press(button, none, at(1, 1));
        view.button_drag(button, none, at(1, 1));
        view.button_drag(button, none, at(3, 1));
        view.button_release(button, none, at(3, 1));
        let drag = code + 32;
        let expected = format!("\x1b[<{code};2;2M\x1b[<{drag};4;2M\x1b[<{code};4;2m");
        let reports = chunks();
        assert_eq!(reports.concat(), expected.as_bytes());
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(|r| is_pointer_or_focus_report(r)));
        assert_eq!(selected(), None);

        // A drag out of the widget stays reported.
        view.button_press(button, none, at(1, 1));
        chunks();
        view.button_drag(button, none, (-5.0, -5.0));
        let expected = format!("\x1b[<{drag};1;1M");
        assert_eq!(chunks().concat(), expected.as_bytes());
        view.button_release(button, none, at(0, 0));
        chunks();

        // A held button is not hovering: the motion controller's view
        // of a drag sends nothing, and leaves the drag its cell.
        backend.write(b"\x1b[?1003h");
        view.button_press(button, none, at(1, 1));
        chunks();
        view.hover(none, at(3, 1));
        assert_eq!(chunks().concat(), b"", "hover during a drag");
        view.button_drag(button, none, at(3, 1));
        let expected = format!("\x1b[<{drag};4;2M");
        assert_eq!(chunks().concat(), expected.as_bytes());
        view.button_release(button, none, at(3, 1));
        chunks();
        view.hover(none, at(5, 1));
        assert_eq!(chunks().concat(), b"\x1b[<35;6;2M", "hovering again");
        backend.write(b"\x1b[?1003l\x1b[?1002h");

        // The whole drag goes the way its press did. A reported press
        // gets its release, Shift or not; Shift adds 4.
        view.button_press(button, none, at(1, 1));
        view.button_release(button, shift, at(1, 1));
        let shifted = code + 4;
        let expected = format!("\x1b[<{code};2;2M\x1b[<{shifted};2;2m");
        assert_eq!(chunks().concat(), expected.as_bytes());

        // Shift keeps it local to the end, Shift let go or not.
        backend.write(b"\x1b[?1003h");
        view.button_press(button, shift, at(1, 1));
        view.hover(none, at(2, 1));
        view.button_drag(button, none, at(3, 1));
        view.button_release(button, none, at(3, 1));
        assert_eq!(chunks().concat(), b"", "Shift keeps it local");
        backend.write(b"\x1b[?1003l\x1b[?1002h");
    }

    // A second button resets the first's gesture, which ends with no
    // start point: the press still gets its release, at the pointer,
    // and nothing stays held. Emitted by hand, as an idle gesture has
    // no start point either.
    let drag_end = |button| {
        let drag = controllers()
            .filter_map(|c| c.downcast::<gtk4::GestureDrag>().ok())
            .find(|drag| drag.button() == button)
            .unwrap();
        assert_eq!(drag.start_point(), None);
        drag.emit_by_name::<()>("drag-end", &[&0.0f64, &0.0f64]);
    };
    view.hover(none, at(3, 1));
    view.press(none, 35_000, at(3, 1));
    drag_end(gdk::BUTTON_PRIMARY);
    assert_eq!(chunks().concat(), b"\x1b[<0;4;2M\x1b[<0;4;2m");
    view.button_press(ffi::BUTTON_RIGHT, none, at(3, 1));
    drag_end(gdk::BUTTON_SECONDARY);
    assert_eq!(chunks().concat(), b"\x1b[<2;4;2M\x1b[<2;4;2m");
    view.button_press(ffi::BUTTON_MIDDLE, none, at(3, 1));
    drag_end(gdk::BUTTON_MIDDLE);
    assert_eq!(chunks().concat(), b"\x1b[<1;4;2M\x1b[<1;4;2m");
    assert!(!view.button_held());

    // A press with no tracking stays local after the program asks.
    backend.write(b"\x1b[?1002l\x1b[?1006l");
    view.button_press(ffi::BUTTON_RIGHT, none, at(1, 1));
    backend.write(b"\x1b[?1002h\x1b[?1006h");
    view.button_release(ffi::BUTTON_RIGHT, none, at(1, 1));
    assert_eq!(chunks().concat(), b"", "no release without its press");
    backend.write(b"\x1b[?1002l\x1b[?1006l");

    // X10 mode takes the presses alone.
    backend.write(b"\x1b[?9h");
    view.button_press(ffi::BUTTON_MIDDLE, none, at(0, 0));
    view.button_drag(ffi::BUTTON_MIDDLE, none, at(2, 0));
    view.button_release(ffi::BUTTON_MIDDLE, none, at(2, 0));
    assert_eq!(chunks().concat(), b"\x1b[M!!!");
    backend.write(b"\x1b[?9l");
}

/// The sideways wheel goes to a tracking program as a report per
/// column, and is left alone otherwise: no scrolling, no arrow keys.
fn sideways_wheel(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let mut chunks = || -> Vec<Vec<u8>> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                TerminalEvent::DataOut(bytes) => Some(bytes),
                _ => None,
            })
            .collect()
    };
    let none = gdk::ModifierType::empty();
    let shift = gdk::ModifierType::SHIFT_MASK;
    let notch = gdk::ScrollUnit::Wheel;
    let (cell_w, _) = view.cell_size();
    backend.reset();
    chunks();
    view.hover(none, (0.0, 0.0));

    // Not the program's: unclaimed, for whatever sits above the view.
    assert!(!view.wheel_sideways(-1.0, notch, none));
    backend.write(b"\x1b[?1049h");
    assert!(!view.wheel_sideways(1.0, notch, none), "no arrow keys");
    backend.write(b"\x1b[?1049l");
    assert_eq!(chunks().concat(), b"");

    backend.write(b"\x1b[?1000h\x1b[?1006h");
    assert!(view.wheel_sideways(-1.0, notch, none));
    assert_eq!(chunks(), vec![b"\x1b[<66;1;1M".to_vec(); 3]);
    assert!(view.wheel_sideways(1.0, notch, none));
    let reports = chunks();
    assert_eq!(reports, vec![b"\x1b[<67;1;1M".to_vec(); 3]);
    assert!(reports.iter().all(|r| is_pointer_or_focus_report(r)));

    // Steps add up on their own carry, pixels by cell width.
    for _ in 0..8 {
        assert!(view.wheel_sideways(0.125, notch, none));
    }
    assert_eq!(chunks().len(), 3);
    view.wheel(0.125, notch, none);
    assert!(view.wheel_sideways(-0.125, notch, none));
    assert_eq!(chunks().concat(), b"", "the axes share no carry");
    view.wheel(-0.125, notch, none);
    let surface = gdk::ScrollUnit::Surface;
    assert!(view.wheel_sideways(f64::from(cell_w), surface, none));
    assert_eq!(chunks(), vec![b"\x1b[<67;1;1M".to_vec()]);

    // Shift keeps it from the program.
    assert!(!view.wheel_sideways(1.0, notch, shift));
    assert_eq!(chunks().concat(), b"");
    backend.write(b"\x1b[?1000l\x1b[?1006l");
}

/// The hooks the tab follows the shell by (doc 07 §Following the
/// shell) see input, a reported directory, and then the output.
fn shell_watchers(backend: &GhosttyBackend, view: &GhosttyView) {
    let seen: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = Rc::clone(&seen);
    view.connect_input(move |data| {
        log.borrow_mut()
            .push(format!("input {}", String::from_utf8_lossy(data)))
    });
    let log = Rc::clone(&seen);
    view.connect_output(move || log.borrow_mut().push("output".into()));
    let log = Rc::clone(&seen);
    view.connect_directory(move |uri| log.borrow_mut().push(format!("directory {uri}")));

    backend.send_input(b"cd /tmp\r");
    assert!(view.key(gdk::Key::a, gdk::ModifierType::empty()));
    backend.write(b"\x1b]7;file://box/tmp\x07");
    assert_eq!(
        *seen.borrow(),
        [
            "input cd /tmp\r",
            "input a",
            "directory file://box/tmp",
            "output"
        ]
    );
}

/// OSC 52 sets the clipboard or PRIMARY, a malformed write leaves them
/// alone, and a query gets no answer.
fn program_clipboard(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let context = glib::MainContext::default();
    let read = |clipboard: gdk::Clipboard| {
        let text = context.block_on(clipboard.read_text_future());
        text.unwrap().map(String::from)
    };
    view.clipboard().set_text("system before");
    view.primary_clipboard().set_text("primary before");

    backend.write(b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(read(view.clipboard()).as_deref(), Some("hello"));
    assert_eq!(
        read(view.primary_clipboard()).as_deref(),
        Some("primary before")
    );

    backend.write(b"\x1b]52;p;d29ybGQ=\x07");
    assert_eq!(read(view.clipboard()).as_deref(), Some("hello"));
    assert_eq!(read(view.primary_clipboard()).as_deref(), Some("world"));
    backend.write(b"\x1b]52;s;Ym90aA==\x07");
    assert_eq!(read(view.primary_clipboard()).as_deref(), Some("both"));

    backend.write(b"\x1b]52;c;SGVs!!!bG8=\x07");
    backend.write(b"\x1b]52;c;YQBi\x07");
    backend.write(b"\x1b]52;c;\x07");
    assert_eq!(read(view.clipboard()).as_deref(), Some("hello"));

    while rx.try_recv().is_ok() {}
    backend.write(b"\x1b]52;c;?\x07");
    backend.write(b"\x1b]52;p;?\x07");
    while context.iteration(false) {}
    assert!(rx.try_recv().is_err(), "a query must go unanswered");
}

/// Glyphs, the cursor and text lines sit on the cell grid, however far
/// the font's advances are from the whole-pixel cell width.
fn glyphs_on_the_grid(backend: &GhosttyBackend, view: &GhosttyView) {
    let metrics = view.metrics();
    let (cw, ch) = (metrics.width as usize, metrics.height as usize);
    backend.reset();
    let (cols, _) = view.shared().unwrap().engine.size();
    let n = usize::from(cols) - 2;
    backend.write("M".repeat(n).as_bytes());
    let inked = render(view);
    for col in 0..n {
        assert!(
            inked(col * cw..(col + 1) * cw, 0..ch),
            "M {col} left its cell"
        );
    }
    // The cursor block comes right after the last M, then nothing.
    assert!(inked(n * cw..(n + 1) * cw, 0..ch));
    assert!(!inked((n + 1) * cw..(n + 2) * cw, 0..ch));

    // Lines cross the blank cells of their runs: cells 1-2 underlined,
    // 6-7 struck through.
    backend.write(b"\r\n\x1b[4ma  b\x1b[24m \x1b[9m_  _\x1b[0m");
    let inked = render(view);
    for (top, cells) in [(metrics.underline.0, [1, 2]), (metrics.strike.0, [6, 7])] {
        let y = ch + top as usize;
        for col in cells {
            let x = col * cw + cw / 2;
            assert!(inked(x..x + 1, y..y + 1), "no line in cell {col}");
        }
    }
}

/// The cursor's shapes on an empty cell, the hollow block of an unfocused
/// view, and the blink: painted whole, on a timer that runs only while
/// the cursor blinks in a focused, mapped view.
fn cursor_shapes(backend: &GhosttyBackend, view: &GhosttyView) {
    let metrics = view.metrics();
    let (cw, ch) = (metrics.width as usize, metrics.height as usize);
    let line = metrics.underline.1 as usize;
    let (x, y) = (cw / 2, ch / 2);
    // Which of the cursor cell's middle and its four edges have ink.
    let shape = || {
        let inked = render(view);
        [
            inked(x..x + 1, y..y + 1),
            inked(x..x + 1, 0..line),
            inked(x..x + 1, ch - line..ch),
            inked(0..line, y..y + 1),
            inked(cw - line..cw, y..y + 1),
        ]
    };
    const BLOCK: [bool; 5] = [true; 5];
    const NOTHING: [bool; 5] = [false; 5];
    const UNDERLINE: [bool; 5] = [false, false, true, false, false];
    const BAR: [bool; 5] = [false, false, false, true, false];
    const HOLLOW: [bool; 5] = [false, true, true, true, true];
    let timer = || view.imp().blink.borrow().is_some();

    backend.reset();
    view.focus_changed(true);
    assert_eq!(shape(), BLOCK, "the default");
    for (decscusr, expected) in [(2, BLOCK), (4, UNDERLINE), (6, BAR)] {
        backend.write(format!("\x1b[{decscusr} q").as_bytes());
        assert_eq!(shape(), expected, "DECSCUSR {decscusr}");
        assert!(!timer(), "a steady cursor needs no timer");
        view.focus_changed(false);
        assert_eq!(shape(), HOLLOW, "unfocused after DECSCUSR {decscusr}");
        view.focus_changed(true);
    }

    // Each phase is one whole frame; DECSCUSR's blink variants and mode
    // 12 both start it.
    for (start, on) in [("\x1b[1 q", BLOCK), ("\x1b[3 q", UNDERLINE)] {
        backend.write(start.as_bytes());
        assert_eq!(shape(), on);
        assert!(timer());
        view.blink_tick();
        assert_eq!(shape(), NOTHING);
        view.blink_tick();
        assert_eq!(shape(), on);
    }
    backend.write(b"\x1b[6 q\x1b[?12h");
    assert_eq!(shape(), BAR);
    assert!(timer());

    // The real timer flips the phase and redraws.
    let context = glib::MainContext::default();
    let start = std::time::Instant::now();
    while !view.imp().blink_off.get()
        && start.elapsed() < 3 * GhosttyOptions::default().cursor_blink
    {
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(view.imp().blink_off.get(), "the timer never ticked");
    assert_eq!(shape(), NOTHING);

    // Focus lost in the off phase: hollow at once, no timer; focus back
    // starts over with the cursor on.
    view.focus_changed(false);
    assert_eq!(shape(), HOLLOW);
    assert!(!timer());
    view.focus_changed(true);
    assert_eq!(shape(), BAR);
    assert!(timer());

    // The blink's end stops the timer, in the off phase too.
    view.blink_tick();
    backend.write(b"\x1b[?12l");
    assert_eq!(shape(), BAR);
    assert!(!timer());
    assert!(!view.imp().blink_off.get());

    // So does hiding the cursor, until it is back.
    backend.write(b"\x1b[1 q");
    assert_eq!(shape(), BLOCK);
    assert!(timer());
    backend.write(b"\x1b[?25l");
    assert_eq!(shape(), NOTHING);
    assert!(!timer());
    backend.write(b"\x1b[?25h");
    assert_eq!(shape(), BLOCK);
    assert!(timer());

    // Unmapped, nothing ticks; the paint after the map starts it again.
    view.blink_tick();
    view.set_visible(false);
    assert!(!view.is_mapped());
    assert!(!timer());
    assert!(!view.imp().blink_off.get());
    view.paint(&gtk4::Snapshot::new());
    assert!(!timer());
    view.set_visible(true);
    for _ in 0..50 {
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(view.is_mapped());
    assert!(timer());

    backend.reset();
    assert_eq!(shape(), BLOCK);
    assert!(!timer());
}

/// Each SGR underline style has its own shape, sharp and in its full
/// color: the cell's underline color when it has one, the text's
/// otherwise. The rows hold only blanks, so all that is lit is the line.
fn underline_styles(backend: &GhosttyBackend, view: &GhosttyView) {
    const CELLS: usize = 8;
    const RED: [u8; 4] = [0, 0, 255, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];
    let metrics = view.metrics();
    let (cw, ch) = (metrics.width as usize, metrics.height as usize);
    let (top, thickness) = (metrics.underline.0 as usize, metrics.underline.1 as usize);
    let blank = " ".repeat(CELLS);
    let red = "58;2;255;0;0";
    let green = "38;2;0;255;0";
    // The pixels of a row's line, from its top on, as rows of pixels.
    let line_of = |sgr: &str| -> Vec<Vec<[u8; 4]>> {
        backend.reset();
        backend.write(format!("\x1b[{sgr}m{blank}\x1b[0m\r\n").as_bytes());
        let (width, height) = (view.width() as usize, view.height() as usize);
        let at = pixels(view);
        let background = at(width - 1, height - 1);
        (top..ch)
            .map(|y| {
                (0..CELLS * cw)
                    .map(|x| at(x, y))
                    .map(|p| if p == background { [0; 4] } else { p })
                    .collect()
            })
            .collect()
    };
    let solid = |row: &[[u8; 4]], color| row.iter().all(|&p| p == color);
    let blank_row = |row: &[[u8; 4]]| row.iter().all(|&p| p == [0; 4]);
    // The lengths of a row's lit stretches.
    let stretches = |row: &[[u8; 4]]| -> Vec<usize> {
        row.split(|&p| p == [0; 4])
            .map(<[_]>::len)
            .filter(|&len| len > 0)
            .collect()
    };

    let single = line_of(&format!("4:1;{red}"));
    assert!(solid(&single[0], RED), "{:?}", &single[0][..4]);
    assert!(single[thickness..].iter().all(|row| blank_row(row)));

    let double = line_of(&format!("4:2;{red}"));
    assert!(solid(&double[0], RED));
    assert!(blank_row(&double[thickness]), "the lines stay apart");
    assert!(solid(&double[2 * thickness], RED));

    // A wave: its crest moves between rows along the line, over three
    // rows or more, and it is strong in the line's color, not a stipple.
    let curly = line_of(&format!("4:3;{red}"));
    let strength = |[_, g, r, _]: [u8; 4]| i32::from(r) - i32::from(g);
    let crest = |x: usize| (0..curly.len()).max_by_key(|&y| strength(curly[y][x]));
    let crests: std::collections::HashSet<_> = (0..CELLS * cw).filter_map(crest).collect();
    assert!(crests.len() >= 2, "{crests:?}");
    assert!(curly.iter().filter(|row| !blank_row(row)).count() >= 3);
    let peak = curly.iter().flatten().map(|&p| strength(p)).max();
    assert!(peak >= Some(160), "{peak:?}");
    assert!(curly.iter().all(|row| !solid(row, RED)));
    for &[b, g, r, _] in curly.iter().flatten().filter(|&&p| p != [0; 4]) {
        assert!(r > g && r > b, "{:?}", [r, g, b]);
    }

    let dotted = line_of(&format!("4:4;{red}"));
    assert!(stretches(&dotted[0]).iter().all(|&len| len == thickness));
    assert!(stretches(&dotted[0]).len() > CELLS);
    assert!(dotted[0].iter().all(|&p| p == RED || p == [0; 4]));
    assert!(dotted[thickness..].iter().all(|row| blank_row(row)));

    let dashed = line_of(&format!("4:5;{red}"));
    let dashes = stretches(&dashed[0]);
    assert!(dashes.len() > 1);
    assert!(dashes[..dashes.len() - 1]
        .iter()
        .all(|&len| len > 2 * thickness));
    assert!(dashed[0].iter().all(|&p| p == RED || p == [0; 4]));

    // With no underline color each style is in the text color, and
    // SGR 59 goes back to it.
    assert!(solid(&line_of(&format!("4:1;{green}"))[0], GREEN));
    assert!(solid(&line_of(&format!("4:2;{green}"))[0], GREEN));
    let reset = format!("4:1;{green};{red};59");
    assert!(solid(&line_of(&reset)[0], GREEN), "SGR 59");
    for sgr in ["4:3", "4:4", "4:5"] {
        let line = line_of(&format!("{sgr};{green}"));
        for &[b, g, r, _] in line.iter().flatten().filter(|&&p| p != [0; 4]) {
            assert!(g > r && g > b, "{sgr}: {:?}", [r, g, b]);
        }
    }

    // Off is off, and a color alone draws nothing.
    for sgr in ["4:3;4:0", "4:3;24", red] {
        let line = line_of(sgr);
        assert!(line.iter().all(|row| blank_row(row)), "{sgr}");
    }

    // Neighbors of one style split on color, and join across a change
    // of text color: a wave runs on unbroken under a line of code.
    backend.reset();
    let half = " ".repeat(CELLS / 2);
    backend.write(format!("\x1b[4:1;{red}m{half}\x1b[58;2;0;0;255m{half}\x1b[0m").as_bytes());
    let at = pixels(view);
    assert_eq!(at(cw, top), RED);
    assert_eq!(at((CELLS - 1) * cw, top), BLUE);
    let whole = line_of(&format!("4:3;{red};38;2;4;5;6m{half}\x1b[38;2;4;5;6"));
    let split = line_of(&format!("4:3;{red};38;2;1;2;3m{half}\x1b[38;2;4;5;6"));
    assert_eq!(whole, split);

    // The line crosses text as it crosses blanks.
    backend.reset();
    backend.write(format!("\x1b[4:1;{red}mMMM\x1b[0m").as_bytes());
    assert_eq!(pixels(view)(cw + cw / 2, top), RED);
}

/// Double and triple clicks select a word and a line, a finished
/// selection reaches PRIMARY, and a middle click pastes it unless the
/// program tracks the mouse.
fn primary_selection(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let none = gdk::ModifierType::empty();
    let shift = gdk::ModifierType::SHIFT_MASK;
    let (cell_w, cell_h) = view.cell_size();
    let at = |col: i32, row: i32| {
        (
            f64::from(col * cell_w + cell_w / 2),
            f64::from(row * cell_h + cell_h / 2),
        )
    };
    let click = |state, time, col| {
        view.press(state, time, at(col, 0));
        view.release(state, at(col, 0));
    };
    while rx.try_recv().is_ok() {}
    let context = glib::MainContext::default();
    let primary = || {
        let read = view.primary_clipboard().read_text_future();
        context.block_on(read).unwrap().map(String::from)
    };
    let mut pasted = |state| {
        view.paste_primary(state);
        let mut sent = Vec::new();
        for _ in 0..100 {
            while context.iteration(false) {}
            while let Ok(event) = rx.try_recv() {
                if let TerminalEvent::DataOut(bytes) = event {
                    sent.extend(bytes);
                }
            }
            if !sent.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        sent
    };

    backend.reset();
    backend.write(b"alpha beta gamma");
    view.primary_clipboard().set_text("before");
    click(none, 10_000, 7);
    assert_eq!(
        primary().as_deref(),
        Some("before"),
        "a click selects nothing"
    );
    click(none, 10_100, 7);
    assert_eq!(backend.selected_text().as_deref(), Some("beta"));
    assert_eq!(primary().as_deref(), Some("beta"));
    click(none, 10_200, 7);
    assert_eq!(primary().as_deref(), Some("alpha beta gamma"));

    // A drag reaches PRIMARY on release, not along the way.
    view.primary_clipboard().set_text("before");
    view.press(none, 20_000, at(0, 0));
    view.drag_to(none, at(4, 0));
    assert_eq!(primary().as_deref(), Some("before"));
    view.release(none, at(4, 0));
    assert!(primary().is_some_and(|text| text.starts_with("alph")));

    backend.select_all();
    assert!(primary().is_some_and(|text| text.contains("alpha beta gamma")));

    view.primary_clipboard().set_text("a\nb");
    assert_eq!(pasted(none), b"a\rb");
    backend.write(b"\x1b[?2004h");
    assert_eq!(pasted(none), b"\x1b[200~a\nb\x1b[201~");
    backend.write(b"\x1b[?2004l\x1b[?1000h");
    assert_eq!(pasted(none), b"", "the program owns the mouse");
    assert_eq!(pasted(shift), b"a\rb", "Shift keeps it local");

    // A reported press ends a click sequence, and Shift still selects.
    click(shift, 30_000, 7);
    click(none, 30_100, 7);
    click(shift, 30_200, 7);
    assert_eq!(backend.selected_text(), None);
    click(shift, 30_300, 7);
    assert_eq!(backend.selected_text().as_deref(), Some("beta"));
    backend.write(b"\x1b[?1000l");
    while rx.try_recv().is_ok() {}
}

/// Hovering a link underlines it and sets the cursor, Ctrl+click opens
/// it, and a program tracking the mouse keeps both until Shift. The
/// desktop's handler is stubbed out: nothing here leaves the process.
fn links_hover_and_open(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = Rc::clone(&opened);
    let stub: Launcher = Rc::new(move |_, uri| log.borrow_mut().push(uri.to_owned()));
    let desktop = view.imp().launcher.replace(Some(stub));
    assert!(desktop.is_some(), "the desktop's handler is the default");
    assert!(backend.features().url_detection);

    let none = gdk::ModifierType::empty();
    let shift = gdk::ModifierType::SHIFT_MASK;
    let ctrl = gdk::ModifierType::CONTROL_MASK;
    let metrics = view.metrics();
    let (cell_w, cell_h) = (metrics.width, metrics.height);
    let at = |col: i32| (f64::from(col * cell_w + cell_w / 2), f64::from(cell_h / 2));
    let click = |state, time, col| {
        view.press(state, time, at(col));
        view.release(state, at(col));
    };
    let cursor = || view.cursor().and_then(|cursor| cursor.name());
    let pointer = || cursor().as_deref() == Some(LINK_CURSOR);
    let underlined = |col: i32| {
        let (x, y) = (
            (col * cell_w + cell_w / 2) as usize,
            metrics.underline.0 as usize,
        );
        render(view)(x..x + 1, y..y + 1)
    };
    let took = || std::mem::take(&mut *opened.borrow_mut());
    let url = "https://example.com/x";

    view.pointer_left();
    backend.reset();
    backend.write(format!("see {url} now").as_bytes());
    assert!(!underlined(4), "no hover, no underline");
    assert_eq!(cursor(), None);

    // On and off the link.
    view.hover(none, at(4));
    assert!(pointer());
    for col in [4, 14, 24] {
        assert!(underlined(col), "column {col}");
    }
    assert!(!underlined(1) && !underlined(25));
    view.hover(none, at(1));
    assert_eq!(cursor(), None);
    assert!(!underlined(4));

    // Output moves the text under a pointer that stays put.
    view.hover(none, at(4));
    backend.reset();
    backend.write(b"nothing to see");
    assert!(!underlined(4));
    assert_eq!(cursor(), None);
    backend.reset();
    backend.write(format!("see {url} now").as_bytes());
    assert!(underlined(4));
    assert!(pointer());

    // Leaving the widget restores everything.
    view.pointer_left();
    assert_eq!(cursor(), None);
    assert!(!underlined(4));

    // Only a Ctrl+click on the link opens it.
    click(none, 40_000, 5);
    click(ctrl, 41_000, 1);
    assert_eq!(took(), [] as [&str; 0]);
    click(ctrl, 42_000, 5);
    assert_eq!(took(), [url]);

    // A drag selects instead, and a release off the link opens nothing.
    view.press(ctrl, 43_000, at(5));
    view.drag_to(ctrl, at(9));
    view.release(ctrl, at(9));
    assert!(backend.selected_text().is_some_and(|text| !text.is_empty()));
    assert_eq!(took(), [] as [&str; 0]);
    view.press(ctrl, 44_000, at(5));
    view.release(ctrl, at(1));
    assert_eq!(took(), [] as [&str; 0]);

    // A program tracking the mouse gets the click, until Shift.
    backend.write(b"\x1b[?1000h\x1b[?1006h");
    view.hover(none, at(4));
    assert_eq!(cursor(), None);
    while rx.try_recv().is_ok() {}
    click(ctrl, 45_000, 5);
    assert_eq!(took(), [] as [&str; 0]);
    let report = std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
        matches!(event, TerminalEvent::DataOut(bytes) if is_pointer_or_focus_report(&bytes))
    });
    assert!(report, "the press went to the program");
    view.hover(shift, at(4));
    assert!(pointer());
    click(ctrl | shift, 46_000, 5);
    assert_eq!(took(), [url]);
    backend.write(b"\x1b[?1000l\x1b[?1006l");

    view.pointer_left();
    view.imp().launcher.replace(desktop);
    while rx.try_recv().is_ok() {}
}

/// Search lights its matches and the current one apart, under the
/// selection; an empty query or a press puts them out. The popover the
/// search actions open sits on this widget as it does on VTE's.
fn search_highlight(backend: &GhosttyBackend, view: &GhosttyView) {
    let features = backend.features();
    assert!(features.search && features.scrollback_search);
    let (cw, _) = view.cell_size();
    let cell = |col: usize| col * cw as usize + 1;
    let background = || {
        let at = pixels(view);
        at(view.width() as usize - 1, view.height() as usize - 1)
    };
    // The cell's top left: its background, never a glyph.
    let color = |col| pixels(view)(cell(col), 1);
    let opaque = |[r, g, b]: [u8; 3]| [b, g, r, 255];
    let search = SearchColors::default();

    backend.reset();
    backend.write(b"one two one");
    assert_eq!(color(0), background(), "no search, no highlight");
    backend.search("one", false);
    assert_eq!(color(0), opaque(search.current_bg));
    assert_eq!(color(4), background());
    assert_eq!(color(8), opaque(search.bg));
    // Forward again: the current match moves, the first stays lit.
    backend.search("one", false);
    assert_eq!(color(0), opaque(search.bg));
    assert_eq!(color(8), opaque(search.current_bg));

    let engine = || view.shared().unwrap().engine.selection_text();
    // On the engine: a press through the view would end the search.
    let shared = view.shared().unwrap();
    shared.engine.selection_cancel();
    assert!(shared
        .engine
        .selection_press((0, 0), (1.0, 1.0), 0, 400, 1.0));
    let end = (f64::from(3 * cw - 1), 1.0);
    assert!(shared
        .engine
        .selection_drag((2, 0), end, cw as u32, view.height() as u32));
    assert_ne!(color(0), opaque(search.bg), "the selection is on top");
    assert_eq!(engine().as_deref(), Some("one"));
    view.shared().unwrap().engine.clear_selection();
    assert_eq!(color(0), opaque(search.bg));

    backend.search("", false);
    assert_eq!(color(0), background());
    assert_eq!(color(8), background());

    backend.search("one", false);
    assert_eq!(color(0), opaque(search.current_bg));
    view.press(gdk::ModifierType::empty(), 5000, (1.0, 1.0));
    view.release(gdk::ModifierType::empty(), (1.0, 1.0));
    assert_eq!(color(0), background());

    backend.search("one", false);
    assert!(view.key(gdk::Key::a, gdk::ModifierType::empty()));
    assert_eq!(color(0), background());

    let popover = gtk4::Popover::new();
    popover.set_child(Some(&gtk4::Entry::new()));
    popover.set_parent(view);
    popover.popup();
    let context = glib::MainContext::default();
    for _ in 0..50 {
        context.iteration(false);
    }
    assert!(popover.is_visible());
    popover.popdown();
    popover.unparent();
}

/// Blinking text goes out and comes back with its cells in place: their
/// backgrounds stay, selection and search still reach them, and the
/// cells after them do not move. The timer runs only while text blinks
/// on a mapped view; a frame without blinking text never toggles.
fn blinking_text(backend: &GhosttyBackend, view: &GhosttyView) {
    let (cw, ch) = view.cell_size();
    let (cw, ch) = (cw as usize, ch as usize);
    // Every pixel of a first-row cell, painted now.
    let cell = |col: usize| -> Vec<[u8; 4]> {
        let at = pixels(view);
        (0..ch)
            .flat_map(|y| (0..cw).map(move |x| (x, y)))
            .map(|(x, y)| at(col * cw + x, y))
            .collect()
    };
    let solid = |pixels: &[[u8; 4]]| pixels.iter().all(|pixel| *pixel == pixels[0]);
    let timer = || view.imp().text_blink_timer.borrow().is_some();
    let off = || view.imp().text_blink_off.get();
    let phase = GhosttyOptions::default().text_blink;
    let context = glib::MainContext::default();
    // Run the main loop until `done`, or for `limit`.
    let wait = |limit: Duration, done: &dyn Fn() -> bool| {
        let start = std::time::Instant::now();
        while !done() && start.elapsed() < limit {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    };

    // Cells 0 and 1 blink on red, cell 3 does not.
    backend.reset();
    backend.write(b"\x1b[5;41mMM\x1b[0m M");
    let shown = cell(0);
    let plain = cell(3);
    assert!(!solid(&shown), "blinking text starts on");
    assert!(timer(), "blinking text starts the timer");

    view.text_blink_tick();
    let hidden = cell(0);
    assert!(solid(&hidden), "no glyph in the off phase");
    assert_eq!(hidden[0], shown[0], "the background stays");
    assert!(solid(&cell(1)));
    assert_eq!(cell(3), plain, "the next cells stay put");

    backend.select_all();
    assert!(backend.selected_text().unwrap().contains("MM"));
    assert_ne!(cell(0)[0], hidden[0], "the selection lights the cell");
    view.shared().unwrap().engine.clear_selection();
    backend.search("MM", false);
    let [r, g, b] = SearchColors::default().current_bg;
    assert_eq!(cell(0)[0], [b, g, r, 255], "search lights the cell");
    backend.search("", false);
    assert_eq!(cell(0), hidden);

    view.text_blink_tick();
    assert_eq!(cell(0), shown, "the glyph comes back");

    // The timer itself flips the phase.
    wait(phase * 4, &|| off());
    assert!(off(), "the timer never ticked");

    // Unmapped, nothing ticks; mapped again, the next frame restarts it.
    view.set_visible(false);
    wait(phase / 5, &|| false);
    assert!(!timer() && !off(), "a hidden view keeps no timer");
    view.set_visible(true);
    wait(phase * 4, &timer);
    assert!(timer(), "the view came back and did not blink");

    // Text gone in the off phase: the timer stops with the text on.
    wait(phase * 4, &|| off());
    backend.reset();
    backend.write(b"plain");
    let still = cell(0);
    assert!(!timer() && !off(), "nothing blinks");
    view.text_blink_tick();
    assert_eq!(cell(0), still, "a late tick changes nothing");
    assert!(!timer() && !off());
    wait(phase * 2, &|| off());
    assert!(!timer() && !off(), "a blink-free frame toggled");
}

/// Options installed as Settings does reach an open view at once and a
/// view built later: colors repaint, the wheel follows, and running blink
/// timers restart at the new period.
fn installed_options(backend: &GhosttyBackend, view: &GhosttyView) {
    let install = |ghostty: GhosttyOptions| {
        install_terminal_defaults(TerminalDefaults {
            ghostty,
            ..Default::default()
        });
    };
    let changed = GhosttyOptions {
        wheel_rows: 1.0,
        cursor_blink: Duration::from_millis(20),
        text_blink: Duration::from_millis(20),
        search: SearchColors {
            current_bg: [1, 2, 3],
            ..Default::default()
        },
    };
    let lit = || pixels(view)(1, 1);
    let opaque = |[r, g, b]: [u8; 3]| [b, g, r, 255];
    let cursor_timer = || view.imp().blink.borrow().is_some();
    let text_timer = || view.imp().text_blink_timer.borrow().is_some();

    backend.reset();
    backend.write(b"\x1b[5mfind\x1b[0m\x1b[1 q");
    backend.search("find", false);
    assert_eq!(lit(), opaque(SearchColors::default().current_bg));
    view.focus_changed(true);
    lit();
    assert!(cursor_timer() && text_timer());

    install(changed.clone());
    assert!(!cursor_timer() && !text_timer(), "the old periods are gone");
    assert_eq!(lit(), opaque([1, 2, 3]));
    assert!(
        cursor_timer() && text_timer(),
        "the next paint restarts them"
    );

    // Well short of the old period.
    let context = glib::MainContext::default();
    let start = std::time::Instant::now();
    while !view.imp().text_blink_off.get() && start.elapsed() < Duration::from_millis(400) {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(view.imp().text_blink_off.get(), "still on the old period");

    view.set_options(changed.clone());
    assert!(
        cursor_timer() && text_timer(),
        "unchanged periods keep running"
    );

    backend.reset();
    for line in 0..100 {
        backend.write(format!("line {line}\r\n").as_bytes());
    }
    backend.scroll_to_top();
    view.wheel(1.0, gdk::ScrollUnit::Wheel, gdk::ModifierType::empty());
    let mut frame = Frame::default();
    assert!(view.shared().unwrap().engine.snapshot(&mut frame));
    assert_eq!(frame.row_text(0).trim_end(), "line 1");

    let (tx, _rx) = mpsc::channel(1);
    let later = GhosttyBackend::new(tx).expect("engine");
    let later = later.widget().downcast::<GhosttyView>().unwrap();
    assert_eq!(*later.imp().options.borrow(), changed);

    install(GhosttyOptions::default());
    assert_eq!(*view.imp().options.borrow(), GhosttyOptions::default());
    assert_eq!(*later.imp().options.borrow(), GhosttyOptions::default());
    backend.reset();
}

/// A cell's whole cluster reaches Pango: a ZWJ family shapes to other
/// glyphs than its first emoji alone (the family glyph with an emoji
/// font, a glyph per codepoint without one), all at the family's cell,
/// and the cells after it stay on the grid. Likewise a decomposed é.
fn clusters_shape_whole(backend: &GhosttyBackend, view: &GhosttyView) {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    backend.reset();
    // Mode 2027 makes the family one wide cell.
    backend.write(b"\x1b[?2027h");
    for row in [family, "\u{1f468}", "e\u{301}", "e"] {
        backend.write(format!("x{row}y\r\n").as_bytes());
    }
    let mut frame = Frame::default();
    assert!(view.shared().unwrap().engine.snapshot(&mut frame));
    // Each glyph of a row's first four cells: (glyph, column, x).
    let shaped = |row: usize| {
        let (text, columns) =
            run_text(&painted_row(&frame, row, &[], true, &SearchColors::default())[..4]);
        let mut glyphs = Vec::new();
        view.shape(&text, &columns, &view.font(), |_, string, columns| {
            for (glyph, &col) in string.glyph_info().iter().zip(columns) {
                glyphs.push((glyph.glyph(), col, glyph.geometry().x_offset()));
            }
        });
        glyphs
    };
    let at = |glyphs: &[(u32, usize, i32)], col: usize| -> Vec<u32> {
        glyphs
            .iter()
            .filter(|&&(_, c, _)| c == col)
            .map(|&(glyph, ..)| glyph)
            .collect()
    };

    let (families, man) = (shaped(0), shaped(1));
    let cluster = at(&families, 1);
    assert_ne!(cluster, at(&man, 1), "only the first emoji was shaped");
    assert!(
        cluster.len() == 1 || cluster.len() == family.chars().count(),
        "{cluster:?}"
    );
    // x at 0, the family at 1, its tail at 2, y at 3, each on its cell.
    let cell = view.metrics().width * pango::SCALE;
    let mut columns: Vec<usize> = families.iter().map(|&(_, col, _)| col).collect();
    columns.dedup();
    assert_eq!(columns, [0, 1, 2, 3]);
    for &(_, col, x) in families.iter().filter(|&&(_, col, _)| col != 1) {
        assert_eq!(x, col as i32 * cell, "column {col}");
    }

    let (decomposed, plain) = (shaped(2), shaped(3));
    assert_ne!(at(&decomposed, 1), at(&plain, 1), "the accent was dropped");
    assert_eq!(at(&decomposed, 2).len(), 1, "y follows in the next cell");
}

/// Images land at their cells in their layers, scroll with the text,
/// go when deleted, and animate; called from the test above, which
/// owns GTK.
fn kitty_images(backend: &GhosttyBackend, view: &GhosttyView) {
    const RED: [u8; 4] = [0, 0, 255, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    assert!(backend.features().kitty_graphics);
    let (cell_w, cell_h) = view.cell_size();
    let (w, h) = (cell_w as usize, cell_h as usize);
    // An image filling `cols` × `rows` cells, in one color.
    let show = |id: u32, [b, g, r, a]: [u8; 4], (cols, rows): (usize, usize), rest: &str| {
        let (width, height) = (cols * w, rows * h);
        let data = [r, g, b, a].repeat(width * height);
        let control = format!("a=T,f=32,i={id},s={width},v={height},q=2{rest}");
        let data = glib::base64_encode(&data);
        backend.write(format!("\x1b_G{control};{data}\x1b\\").as_bytes());
    };
    // The middle of a cell.
    let mid = |at: &dyn Fn(usize, usize) -> [u8; 4], col: usize, row: usize| {
        at(col * w + w / 2, row * h + h / 2)
    };
    // With the cursor hidden: it would sit on the cell after an image.
    let fresh = || {
        backend.reset();
        backend.write(b"\x1b[?25l");
    };
    fresh();
    let background = pixels(view)(view.width() as usize - 1, view.height() as usize - 1);

    // At its cell, and not beside it.
    backend.write(b"\x1b[2;3H");
    show(1, RED, (2, 1), "");
    let at = pixels(view);
    assert_eq!(mid(&at, 2, 1), RED);
    assert_eq!(mid(&at, 3, 1), RED);
    assert_eq!(mid(&at, 1, 1), background);
    assert_eq!(mid(&at, 4, 1), background);
    assert_eq!(mid(&at, 2, 0), background);
    assert_eq!(mid(&at, 2, 2), background);

    // Scrolls with the text, is clipped at the top, then goes.
    backend.write(b"\x1b[999;1H\r\n");
    assert_eq!(mid(&pixels(view), 2, 0), RED, "moved up a row");
    backend.write(b"\r\n");
    assert_eq!(mid(&pixels(view), 2, 0), background, "scrolled off");
    assert!(view.imp().textures.borrow().is_empty(), "texture kept");
    view.shared().unwrap().engine.scroll(Scroll::By(-1));
    assert_eq!(mid(&pixels(view), 2, 0), RED, "back in the scrollback");
    view.shared().unwrap().engine.scroll(Scroll::Bottom);

    // Partly above the viewport, what is left shows.
    fresh();
    backend.write(b"\x1b[999;1H");
    show(10, RED, (1, 2), ",C=1");
    let rows = usize::from(view.shared().unwrap().engine.size().1);
    backend.write(b"\x1b[999;1H");
    backend.write("\r\n".repeat(rows).as_bytes());
    let at = pixels(view);
    assert_eq!(view.imp().frame.borrow().placements[0].row, -1);
    assert_eq!(mid(&at, 0, 0), RED);
    assert_eq!(mid(&at, 0, 1), background);

    // Past the grid's edge it is clipped, even with room to draw.
    fresh();
    let (cols, rows) = view.shared().unwrap().engine.size();
    view.request_grid(rows, cols - 1);
    let last = usize::from(cols) - 2;
    backend.write(format!("\x1b[1;{}H", last + 1).as_bytes());
    show(11, RED, (3, 1), ",C=1");
    let at = pixels(view);
    assert_eq!(mid(&at, last, 0), RED);
    assert_eq!(mid(&at, last + 1, 0), background, "past the last column");
    view.request_grid(rows, cols);

    // Deleted, it goes, along with its texture.
    fresh();
    show(2, RED, (1, 1), "");
    assert_eq!(mid(&pixels(view), 0, 0), RED);
    backend.write(b"\x1b_Ga=d,d=I,i=2,q=2\x1b\\");
    assert_eq!(mid(&pixels(view), 0, 0), background);
    assert!(view.imp().textures.borrow().is_empty(), "texture kept");

    // Below text: over a colored cell, under its text. A block fills
    // the middle of its cell, so text shows through at that pixel.
    fresh();
    backend.write("\x1b[1;1H\x1b[44m \u{2588}\x1b[0m\x1b[1;1H".as_bytes());
    show(3, RED, (2, 1), ",z=-1,C=1");
    let at = pixels(view);
    assert_eq!(mid(&at, 0, 0), RED, "over the cell's background");
    assert_ne!(mid(&at, 1, 0), RED, "under the text");

    // Below backgrounds: over the default one, under a colored cell.
    fresh();
    backend.write(b"\x1b[1;1H\x1b[44m \x1b[0m\x1b[1;1H");
    show(4, RED, (2, 1), ",z=-1073741825,C=1");
    let at = pixels(view);
    assert_ne!(mid(&at, 0, 0), RED, "under the colored cell");
    assert_eq!(mid(&at, 1, 0), RED, "over the default background");

    // Above text, and higher z over lower.
    fresh();
    backend.write("\x1b[1;1H\u{2588}\x1b[1;1H".as_bytes());
    show(5, GREEN, (1, 1), ",z=2,C=1");
    show(6, RED, (1, 1), ",z=1,C=1");
    assert_eq!(mid(&pixels(view), 0, 0), GREEN);

    // The source rectangle crops: the right half of red | green.
    fresh();
    let row: Vec<u8> = [[255, 0, 0, 255].repeat(w), [0, 255, 0, 255].repeat(w)].concat();
    let data = glib::base64_encode(&row.repeat(h));
    let (width, half) = (2 * w, w);
    let control = format!("a=T,f=32,i=7,s={width},v={h},x={half},w={half},q=2");
    backend.write(format!("\x1b_G{control};{data}\x1b\\").as_bytes());
    let at = pixels(view);
    assert_eq!(mid(&at, 0, 0), GREEN);
    assert_eq!(mid(&at, 1, 0), background);

    // A placeholder cell shows the image, with no glyph over it.
    fresh();
    let (width, height) = (w, h);
    let data = glib::base64_encode(&[255, 0, 0, 255].repeat(width * height));
    let control = format!("a=T,U=1,f=32,i=8,s={width},v={height},c=1,r=1,q=2");
    backend.write(format!("\x1b_G{control};{data}\x1b\\").as_bytes());
    backend.write("\x1b[38;5;8m\u{10eeee}\u{305}\u{305}\x1b[39m".as_bytes());
    let at = pixels(view);
    for (x, y) in [(1, 1), (w / 2, h / 2), (w - 2, h - 2)] {
        assert_eq!(at(x, y), RED, "({x}, {y})");
    }

    // An animation runs by itself: the view asks for its own frames.
    fresh();
    let frame = |control: &str, pixel: [u8; 3]| {
        let [r, g, b] = pixel;
        let data = glib::base64_encode(&[r, g, b, 255].repeat(w * h));
        let size = format!("f=32,s={w},v={h},q=2");
        backend.write(format!("\x1b_G{control},{size};{data}\x1b\\").as_bytes());
    };
    frame("a=T,i=9", [255, 0, 0]);
    frame("a=f,i=9,z=40", [0, 255, 0]);
    backend.write(b"\x1b_Ga=a,i=9,r=1,z=40,q=2\x1b\\");
    backend.write(b"\x1b_Ga=a,i=9,s=3,q=2\x1b\\");
    // Only the main loop runs here, so each frame is one GTK drew.
    let context = glib::MainContext::default();
    let mut frames = std::collections::HashSet::new();
    for _ in 0..500 {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(2));
        let drawn = view.imp().frame.borrow();
        frames.extend(drawn.placements.iter().map(|p| p.generation));
        if frames.len() >= 3 {
            break;
        }
    }
    assert!(frames.len() >= 3, "{frames:?}");
    backend.reset();
}

/// Hovering a hyperlink shows its URI, underlines it and sets the
/// cursor, Ctrl+click opens it, a `file://` link for another host stays
/// shut, and a program tracking the mouse keeps all of it until Shift.
/// The desktop's handler is stubbed out: nothing here leaves the process.
fn hyperlinks_hover_and_open(
    backend: &GhosttyBackend,
    view: &GhosttyView,
    rx: &mut mpsc::Receiver<TerminalEvent>,
) {
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = Rc::clone(&opened);
    let stub: Launcher = Rc::new(move |_, uri| log.borrow_mut().push(uri.to_owned()));
    let desktop = view.imp().launcher.replace(Some(stub));
    assert!(desktop.is_some(), "the desktop's handler is the default");

    let none = gdk::ModifierType::empty();
    let shift = gdk::ModifierType::SHIFT_MASK;
    let ctrl = gdk::ModifierType::CONTROL_MASK;
    let metrics = view.metrics();
    let (cell_w, cell_h) = (metrics.width, metrics.height);
    let at = |col: i32| (f64::from(col * cell_w + cell_w / 2), f64::from(cell_h / 2));
    let click = |state, time, col| {
        view.press(state, time, at(col));
        view.release(state, at(col));
    };
    let tooltip = || view.tooltip_text().map(|text| text.to_string());
    let pointer = || view.cursor().and_then(|c| c.name()).as_deref() == Some(LINK_CURSOR);
    let underlined = |col: i32| {
        let (x, y) = (
            (col * cell_w + cell_w / 2) as usize,
            metrics.underline.0 as usize,
        );
        render(view)(x..x + 1, y..y + 1)
    };
    let took = || std::mem::take(&mut *opened.borrow_mut());
    let linked = |uri: &str| format!("see \x1b]8;;{uri}\x1b\\the docs\x1b]8;;\x1b\\ now");
    let uri = "https://example.com/x";

    view.pointer_left();
    backend.reset();
    backend.write(linked(uri).as_bytes());
    assert!(!underlined(5), "no hover, no underline");
    assert_eq!(tooltip(), None);

    // On and off the link.
    view.hover(none, at(5));
    assert_eq!(tooltip().as_deref(), Some(uri));
    assert!(pointer());
    for col in [4, 8, 11] {
        assert!(underlined(col), "column {col}");
    }
    assert!(!underlined(1) && !underlined(13));
    view.hover(none, at(1));
    assert_eq!(tooltip(), None);
    assert!(!pointer());
    assert!(!underlined(5));

    // Output moves the text under a pointer that stays put.
    view.hover(none, at(5));
    backend.reset();
    backend.write(b"nothing to see");
    assert!(!underlined(5));
    assert_eq!(tooltip(), None);
    backend.reset();
    backend.write(linked(uri).as_bytes());
    assert!(underlined(5));
    assert_eq!(tooltip().as_deref(), Some(uri));

    // Leaving the widget clears everything.
    view.pointer_left();
    assert_eq!(tooltip(), None);
    assert!(!pointer());
    assert!(!underlined(5));

    // Only a Ctrl+click on the link opens it.
    click(none, 50_000, 5);
    click(ctrl, 51_000, 1);
    assert_eq!(took(), [] as [&str; 0]);
    click(ctrl, 52_000, 5);
    assert_eq!(took(), [uri]);

    // A drag selects instead, and a release off the link opens nothing.
    view.press(ctrl, 53_000, at(5));
    view.drag_to(ctrl, at(9));
    view.release(ctrl, at(9));
    assert!(backend.selected_text().is_some_and(|text| !text.is_empty()));
    assert_eq!(took(), [] as [&str; 0]);
    view.press(ctrl, 54_000, at(5));
    view.release(ctrl, at(1));
    assert_eq!(took(), [] as [&str; 0]);

    // A file link opens for this host alone, and still shows either way.
    let host = glib::host_name();
    let here = format!("file://{host}/etc/hosts");
    let there = format!("file://not-{host}/etc/hosts");
    backend.reset();
    backend.write(linked(&here).as_bytes());
    assert!(!underlined(5), "painted, and no hover");
    click(ctrl, 55_000, 5);
    assert_eq!(took(), [here.as_str()]);
    backend.reset();
    backend.write(linked(&there).as_bytes());
    view.hover(none, at(5));
    assert!(underlined(5));
    assert_eq!(tooltip().as_deref(), Some(there.as_str()));
    click(ctrl, 56_000, 5);
    assert_eq!(took(), [] as [&str; 0]);
    view.pointer_left();

    // A program tracking the mouse gets the click, until Shift.
    backend.reset();
    backend.write(linked(uri).as_bytes());
    backend.write(b"\x1b[?1000h\x1b[?1006h");
    assert!(!underlined(5), "painted, and no hover");
    view.hover(none, at(5));
    assert_eq!(tooltip(), None);
    while rx.try_recv().is_ok() {}
    click(ctrl, 57_000, 5);
    assert_eq!(took(), [] as [&str; 0]);
    let report = std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
        matches!(event, TerminalEvent::DataOut(bytes) if is_pointer_or_focus_report(&bytes))
    });
    assert!(report, "the press went to the program");
    view.hover(shift, at(5));
    assert_eq!(tooltip().as_deref(), Some(uri));
    click(ctrl | shift, 58_000, 5);
    assert_eq!(took(), [uri]);
    backend.write(b"\x1b[?1000l\x1b[?1006l");
    view.pointer_left();
    while rx.try_recv().is_ok() {}
}

/// Paint the view now; the result tells whether any pixel in a box
/// differs from the background.
fn render(view: &GhosttyView) -> impl Fn(Range<usize>, Range<usize>) -> bool {
    let (width, height) = (view.width() as usize, view.height() as usize);
    let at = pixels(view);
    let background = at(width - 1, height - 1);
    move |xs, ys| {
        xs.flat_map(|x| ys.clone().map(move |y| (x, y)))
            .any(|(x, y)| at(x, y) != background)
    }
}

/// Paint the view now; the result reads one pixel, as GDK stores it:
/// blue, green, red, alpha.
fn pixels(view: &GhosttyView) -> impl Fn(usize, usize) -> [u8; 4] {
    let (width, height) = (view.width(), view.height());
    let snapshot = gtk4::Snapshot::new();
    view.paint(&snapshot);
    let renderer = view.native().and_then(|n| n.renderer()).unwrap();
    let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(snapshot.to_node().unwrap(), Some(&bounds));
    let stride = width as usize * 4;
    let mut pixels = vec![0; stride * height as usize];
    texture.download(&mut pixels, stride);
    move |x, y| {
        let i = y * stride + x * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }
}
