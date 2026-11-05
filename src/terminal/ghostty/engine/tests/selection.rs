use super::*;

const CELL_W: u32 = 10;
const CELL_H: u32 = 20;
const REPEAT_MS: u32 = 400;

fn pixel((col, row): (u16, u16)) -> (f64, f64) {
    let w = f64::from(CELL_W);
    let h = f64::from(CELL_H);
    (f64::from(col) * w + 1.0, f64::from(row) * h + 1.0)
}

fn click(engine: &Engine, cell: (u16, u16), time_ms: u32) {
    let repeat_px = f64::from(CELL_W);
    assert!(engine.selection_press(cell, pixel(cell), time_ms, REPEAT_MS, repeat_px));
    engine.selection_release(cell);
}

fn drag_to(engine: &Engine, cell: (u16, u16)) {
    // The far side of the cell, so the drag takes it in.
    let (x, y) = pixel(cell);
    let at = (x + f64::from(CELL_W) - 2.0, y);
    assert!(engine.selection_drag(cell, at, CELL_W, 24 * CELL_H));
}

/// Select cells `from` through `to`, each `(col, row)`, by mouse; a
/// new gesture each time.
pub(super) fn drag_select(engine: &Engine, from: (u16, u16), to: (u16, u16)) {
    engine.selection_cancel();
    let repeat_px = f64::from(CELL_W);
    assert!(engine.selection_press(from, pixel(from), 0, REPEAT_MS, repeat_px));
    drag_to(engine, to);
    engine.selection_release(to);
}

#[test]
fn clicks_select_nothing_then_a_word_then_a_line() {
    let engine = engine();
    engine.write(b"alpha beta gamma");
    click(&engine, (7, 0), 1000);
    assert_eq!(engine.selection_text(), None);
    click(&engine, (7, 0), 1100);
    assert_eq!(engine.selection_text().as_deref(), Some("beta"));
    click(&engine, (7, 0), 1200);
    assert_eq!(engine.selection_text().as_deref(), Some("alpha beta gamma"));
    // Three is the most: a fourth click stays on the line.
    click(&engine, (7, 0), 1300);
    assert_eq!(engine.selection_text().as_deref(), Some("alpha beta gamma"));
}

#[test]
fn clicks_too_slow_or_too_far_apart_start_over() {
    let engine = engine();
    engine.write(b"alpha beta gamma");
    click(&engine, (7, 0), 1000);
    click(&engine, (7, 0), 1000 + REPEAT_MS + 1);
    assert_eq!(engine.selection_text(), None, "too slow");
    click(&engine, (13, 0), 1500);
    assert_eq!(engine.selection_text(), None, "too far");
    click(&engine, (13, 0), 1600);
    assert_eq!(engine.selection_text().as_deref(), Some("gamma"));
}

#[test]
fn a_word_keeps_a_path_whole() {
    let engine = engine();
    engine.write(b"see /usr/local/bin");
    click(&engine, (8, 0), 1000);
    click(&engine, (8, 0), 1100);
    assert_eq!(engine.selection_text().as_deref(), Some("/usr/local/bin"));
}

#[test]
fn a_triple_click_takes_a_wrapped_line_whole() {
    let engine = Engine::new(10, 4).unwrap();
    engine.write(b"0123456789abcde\r\nnext");
    for time in [1000, 1100, 1200] {
        click(&engine, (2, 1), time);
    }
    assert_eq!(engine.selection_text().as_deref(), Some("0123456789abcde"));
}

#[test]
fn a_drag_extends_by_what_the_press_selected() {
    let engine = Engine::new(80, 5).unwrap();
    engine.write(b"alpha beta gamma delta");
    click(&engine, (7, 0), 1000);
    let repeat_px = f64::from(CELL_W);
    assert!(engine.selection_press((7, 0), pixel((7, 0)), 1100, REPEAT_MS, repeat_px));
    drag_to(&engine, (12, 0));
    engine.selection_release((12, 0));
    assert_eq!(engine.selection_text().as_deref(), Some("beta gamma"));
}

#[test]
fn a_drag_back_to_the_pressed_cell_selects_nothing() {
    let engine = engine();
    engine.write(b"abc def");
    drag_select(&engine, (0, 0), (2, 0));
    assert!(engine.selection_text().is_some());
    let repeat_px = f64::from(CELL_W);
    engine.selection_cancel();
    assert!(engine.selection_press((1, 0), pixel((1, 0)), 0, REPEAT_MS, repeat_px));
    drag_to(&engine, (3, 0));
    assert!(engine.selection_text().is_some());
    assert!(engine.selection_drag((1, 0), pixel((1, 0)), CELL_W, 24 * CELL_H));
    assert_eq!(engine.selection_text(), None);
}

#[test]
fn cancelling_makes_the_next_press_a_first_press() {
    let engine = engine();
    engine.write(b"alpha beta");
    click(&engine, (7, 0), 1000);
    engine.selection_cancel();
    click(&engine, (7, 0), 1100);
    assert_eq!(engine.selection_text(), None);
}

#[test]
fn a_gesture_survives_output_a_reset_and_a_screen_switch() {
    let engine = engine();
    engine.write(b"alpha beta");
    click(&engine, (7, 0), 1000);
    for _ in 0..100 {
        engine.write(b"more\r\n");
    }
    engine.selection_drag((1, 1), pixel((1, 1)), CELL_W, 24 * CELL_H);
    engine.write(b"\x1b[?1049h");
    engine.selection_drag((1, 1), pixel((1, 1)), CELL_W, 24 * CELL_H);
    assert_eq!(
        engine.selection_text(),
        None,
        "the press is on another screen"
    );
    engine.write(b"\x1b[?1049l");
    click(&engine, (2, 2), 2000);
    engine.reset();
    engine.selection_drag((1, 1), pixel((1, 1)), CELL_W, 24 * CELL_H);
    assert_eq!(engine.selection_text(), None);
    engine.write(b"alpha beta");
    click(&engine, (7, 0), 3000);
    click(&engine, (7, 0), 3100);
    assert_eq!(engine.selection_text().as_deref(), Some("beta"));
}

#[test]
fn selection_select_all_and_clear() {
    let engine = engine();
    engine.write(b"abc def");
    assert_eq!(engine.selection_text(), None);
    drag_select(&engine, (0, 0), (2, 0));
    assert_eq!(engine.selection_text().as_deref(), Some("abc"));
    let frame = frame(&engine);
    assert_ne!(frame.cell(0, 0).unwrap().flags & ffi::CELL_SELECTED, 0);
    assert_eq!(frame.cell(0, 4).unwrap().flags & ffi::CELL_SELECTED, 0);
    assert!(engine.select_all());
    assert!(engine.selection_text().unwrap().contains("abc def"));
    engine.clear_selection();
    assert_eq!(engine.selection_text(), None);
}
