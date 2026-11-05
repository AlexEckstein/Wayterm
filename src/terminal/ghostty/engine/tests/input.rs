use super::*;

#[test]
fn keys_follow_terminal_modes() {
    let engine = engine();
    assert_eq!(
        engine.encode_key(u32::from('a'), u32::from('a'), 0, "a"),
        b"a"
    );
    assert_eq!(engine.encode_key(0xff0d, 0, 0, "\r"), b"\r");
    assert_eq!(
        engine.encode_key(u32::from('c'), u32::from('c'), ffi::MOD_CTRL, "c"),
        b"\x03"
    );
    assert_eq!(engine.encode_key(0xff52, 0, 0, ""), b"\x1b[A");
    // DECCKM (application cursor keys) switches arrows to SS3.
    engine.write(b"\x1b[?1h");
    assert_eq!(engine.encode_key(0xff52, 0, 0, ""), b"\x1bOA");
}

#[test]
fn paste_is_bracketed_only_when_the_program_asks() {
    let engine = engine();
    assert_eq!(engine.encode_paste("a\nb"), b"a\rb");
    engine.write(b"\x1b[?2004h");
    assert_eq!(engine.encode_paste("a\nb"), b"\x1b[200~a\nb\x1b[201~");
}

/// A mouse event over a 20×5 grid of 10×20-pixel cells, at the centre
/// of cell `(col, row)`.
fn mouse(action: u32, button: u32, mods: u32, (col, row): (u16, u16)) -> MouseEvent {
    MouseEvent {
        action,
        button,
        mods,
        x: f32::from(col) * 10.0 + 5.0,
        y: f32::from(row) * 20.0 + 10.0,
        width: 200,
        height: 100,
        cell_w: 10,
        cell_h: 20,
        any_pressed: u32::from(
            action != ffi::MOUSE_RELEASE
                && matches!(
                    button,
                    ffi::BUTTON_LEFT | ffi::BUTTON_MIDDLE | ffi::BUTTON_RIGHT
                ),
        ),
    }
}

#[test]
fn mouse_modes_report_tracking_alt_scroll_and_the_alt_screen() {
    let engine = engine();
    // libghostty-vt starts with alternate scroll on.
    let start = MouseModes {
        tracking: None,
        alt_scroll: true,
        alt_screen: false,
    };
    assert_eq!(engine.mouse_modes(), start);
    engine.write(b"\x1b[?1007l");
    assert!(!engine.mouse_modes().alt_scroll);
    engine.write(b"\x1b[?9h");
    assert_eq!(engine.mouse_modes().tracking, Some(Tracking::X10));
    engine.write(b"\x1b[?1000h");
    assert_eq!(engine.mouse_modes().tracking, Some(Tracking::Normal));
    engine.write(b"\x1b[?1002h");
    assert_eq!(engine.mouse_modes().tracking, Some(Tracking::Button));
    engine.write(b"\x1b[?1003h");
    assert_eq!(engine.mouse_modes().tracking, Some(Tracking::Any));
    engine.write(b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l\x1b[?1007h");
    let modes = engine.mouse_modes();
    assert_eq!(modes.tracking, None);
    assert!(modes.alt_scroll && !modes.alt_screen);
    engine.write(b"\x1b[?1049h");
    assert!(engine.mouse_modes().alt_screen);
    engine.write(b"\x1b[?1049l\x1b[?1047h");
    assert!(engine.mouse_modes().alt_screen);
    engine.write(b"\x1b[?1047l");
    assert!(!engine.mouse_modes().alt_screen);
}

#[test]
fn sgr_mouse_reports_press_release_wheel_and_modifiers() {
    let engine = engine();
    engine.write(b"\x1b[?1000h\x1b[?1006h");
    let press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, 0, (3, 2));
    // SGR: CSI < button ; column ; row, 1-based; M presses, m releases.
    assert_eq!(engine.encode_mouse(&press), b"\x1b[<0;4;3M");
    let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, 0, (3, 2));
    assert_eq!(engine.encode_mouse(&release), b"\x1b[<0;4;3m");
    // Normal tracking (1000) reports no motion.
    let motion = mouse(ffi::MOUSE_MOTION, ffi::BUTTON_LEFT, 0, (4, 2));
    assert_eq!(engine.encode_mouse(&motion), b"");
    // The wheel is buttons 4 and 5, codes 64 and 65.
    let up = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_UP, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&up), b"\x1b[<64;1;1M");
    let down = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_DOWN, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&down), b"\x1b[<65;1;1M");
    // Shift adds 4, Ctrl 16.
    let mods = mouse(
        ffi::MOUSE_PRESS,
        ffi::BUTTON_LEFT,
        ffi::MOD_SHIFT | ffi::MOD_CTRL,
        (0, 0),
    );
    assert_eq!(engine.encode_mouse(&mods), b"\x1b[<20;1;1M");
}

#[test]
fn button_tracking_reports_drags_and_any_tracking_bare_motion() {
    let engine = engine();
    engine.write(b"\x1b[?1002h\x1b[?1006h");
    // Motion adds 32 to the button code.
    let drag = mouse(ffi::MOUSE_MOTION, ffi::BUTTON_LEFT, 0, (4, 2));
    assert_eq!(engine.encode_mouse(&drag), b"\x1b[<32;5;3M");
    let hover = mouse(ffi::MOUSE_MOTION, ffi::BUTTON_NONE, 0, (4, 2));
    assert_eq!(engine.encode_mouse(&hover), b"", "1002 needs a button");
    engine.write(b"\x1b[?1003h");
    // No button is code 3, plus 32 for motion.
    assert_eq!(engine.encode_mouse(&hover), b"\x1b[<35;5;3M");
}

#[test]
fn middle_and_right_buttons_report_press_drag_and_release() {
    let engine = engine();
    engine.write(b"\x1b[?1002h\x1b[?1006h");
    // Button codes 1 and 2; motion adds 32.
    for (button, press, drag, release) in [
        (
            ffi::BUTTON_MIDDLE,
            &b"\x1b[<1;4;3M"[..],
            &b"\x1b[<33;5;3M"[..],
            &b"\x1b[<1;5;3m"[..],
        ),
        (
            ffi::BUTTON_RIGHT,
            b"\x1b[<2;4;3M",
            b"\x1b[<34;5;3M",
            b"\x1b[<2;5;3m",
        ),
    ] {
        let event = mouse(ffi::MOUSE_PRESS, button, 0, (3, 2));
        assert_eq!(engine.encode_mouse(&event), press);
        let event = mouse(ffi::MOUSE_MOTION, button, 0, (4, 2));
        assert_eq!(engine.encode_mouse(&event), drag);
        let event = mouse(ffi::MOUSE_RELEASE, button, 0, (4, 2));
        assert_eq!(engine.encode_mouse(&event), release);
    }
}

#[test]
fn a_held_button_reports_motion_past_the_widget() {
    let engine = engine();
    engine.write(b"\x1b[?1002h\x1b[?1006h");
    for (button, drag) in [
        (ffi::BUTTON_MIDDLE, &b"\x1b[<33;1;1M"[..]),
        (ffi::BUTTON_RIGHT, b"\x1b[<34;1;1M"),
    ] {
        let mut event = mouse(ffi::MOUSE_MOTION, button, 0, (0, 0));
        (event.x, event.y) = (-5.0, -5.0);
        assert_eq!(engine.encode_mouse(&event), drag);
        event.any_pressed = 0;
        assert_eq!(engine.encode_mouse(&event), b"", "no button held");
    }
}

#[test]
fn the_horizontal_wheel_is_buttons_six_and_seven() {
    let engine = engine();
    engine.write(b"\x1b[?1000h\x1b[?1006h");
    let left = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_LEFT, 0, (0, 0));
    let right = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_RIGHT, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&left), b"\x1b[<66;1;1M");
    assert_eq!(engine.encode_mouse(&right), b"\x1b[<67;1;1M");
    // 32 + 66 and 32 + 67 in the byte format.
    engine.write(b"\x1b[?1006l");
    assert_eq!(engine.encode_mouse(&left), b"\x1b[Mb!!");
    assert_eq!(engine.encode_mouse(&right), b"\x1b[Mc!!");
}

#[test]
fn without_sgr_the_mouse_uses_the_x10_byte_format() {
    let engine = engine();
    engine.write(b"\x1b[?1000h");
    // CSI M, then button, column and row as 32 + value (1-based).
    let press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, 0, (3, 2));
    assert_eq!(engine.encode_mouse(&press), b"\x1b[M $#");
    // Legacy releases do not say which button: code 3.
    let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, 0, (3, 2));
    assert_eq!(engine.encode_mouse(&release), b"\x1b[M#$#");
    let middle = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_MIDDLE, 0, (3, 2));
    assert_eq!(engine.encode_mouse(&middle), b"\x1b[M!$#");
    let right = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_RIGHT, 0, (3, 2));
    assert_eq!(engine.encode_mouse(&right), b"\x1b[M\"$#");
    let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_RIGHT, 0, (3, 2));
    assert_eq!(engine.encode_mouse(&release), b"\x1b[M#$#");
}

#[test]
fn x10_mode_reports_presses_only_and_no_tracking_reports_nothing() {
    let engine = engine();
    let press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, ffi::MOD_SHIFT, (0, 0));
    assert_eq!(engine.encode_mouse(&press), b"");
    engine.write(b"\x1b[?9h");
    // X10 carries no modifiers.
    assert_eq!(engine.encode_mouse(&press), b"\x1b[M !!");
    let release = mouse(ffi::MOUSE_RELEASE, ffi::BUTTON_LEFT, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&release), b"");
    let wheel = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_UP, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&wheel), b"");
    let wheel = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_WHEEL_LEFT, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&wheel), b"");
    let middle = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_MIDDLE, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&middle), b"\x1b[M!!!");
    let right = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_RIGHT, 0, (0, 0));
    assert_eq!(engine.encode_mouse(&right), b"\x1b[M\"!!");
}

#[test]
fn pixels_past_the_grid_count_as_its_last_cell() {
    let engine = engine();
    engine.write(b"\x1b[?1000h\x1b[?1006h");
    // The widget is 7 pixels wider and taller than the 20×5 grid.
    let mut press = mouse(ffi::MOUSE_PRESS, ffi::BUTTON_LEFT, 0, (0, 0));
    (press.width, press.height, press.x, press.y) = (207, 107, 204.0, 105.0);
    assert_eq!(engine.encode_mouse(&press), b"\x1b[<0;20;5M");
}

#[test]
fn focus_events_only_when_the_program_asks() {
    let engine = engine();
    assert_eq!(engine.encode_focus(true), b"");
    assert_eq!(engine.encode_focus(false), b"");
    engine.write(b"\x1b[?1004h");
    assert_eq!(engine.encode_focus(true), b"\x1b[I");
    assert_eq!(engine.encode_focus(false), b"\x1b[O");
    engine.write(b"\x1b[?1004l");
    assert_eq!(engine.encode_focus(true), b"");
}
