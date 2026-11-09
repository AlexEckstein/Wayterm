use super::*;
use crate::terminal::color_scheme::{DARK, LIGHT};

#[test]
fn text_lands_in_the_grid_and_cursor_moves() {
    let engine = engine();
    engine.write(b"hello\r\nworld");
    let frame = frame(&engine);
    assert_eq!((frame.info.cols, frame.info.rows), (20, 5));
    assert_eq!(frame.row_text(0).trim_end(), "hello");
    assert_eq!(frame.row_text(1).trim_end(), "world");
    assert_eq!((frame.info.cursor_x, frame.info.cursor_y), (5, 1));
}

/// DECSCUSR picks the shape and the blink, mode 12 the blink alone;
/// DECSCUSR 0 and a reset go back to a steady block.
#[test]
fn cursor_style_and_blink_follow_the_terminal() {
    let engine = engine();
    let cursor = || {
        let info = frame(&engine).info;
        (info.cursor_style, info.cursor_blinking != 0)
    };
    assert_eq!(cursor(), (ffi::CURSOR_BLOCK, false));
    for (decscusr, style, blinking) in [
        (1, ffi::CURSOR_BLOCK, true),
        (2, ffi::CURSOR_BLOCK, false),
        (3, ffi::CURSOR_UNDERLINE, true),
        (4, ffi::CURSOR_UNDERLINE, false),
        (5, ffi::CURSOR_BAR, true),
        (6, ffi::CURSOR_BAR, false),
    ] {
        engine.write(format!("\x1b[{decscusr} q").as_bytes());
        assert_eq!(cursor(), (style, blinking), "DECSCUSR {decscusr}");
    }
    engine.write(b"\x1b[?12h");
    assert_eq!(cursor(), (ffi::CURSOR_BAR, true));
    engine.write(b"\x1b[?12l");
    assert_eq!(cursor(), (ffi::CURSOR_BAR, false));

    engine.write(b"\x1b[5 q\x1b[0 q");
    assert_eq!(cursor(), (ffi::CURSOR_BLOCK, false));
    engine.write(b"\x1b[3 q");
    engine.reset();
    assert_eq!(cursor(), (ffi::CURSOR_BLOCK, false));
}

/// A cell's cluster as a string; empty for an empty cell.
fn cluster(frame: &Frame, row: usize, col: usize) -> String {
    frame
        .grapheme(row, col)
        .iter()
        .filter_map(|&c| char::from_u32(c))
        .collect()
}

#[test]
fn marks_and_selectors_stay_in_their_base_cell() {
    let engine = engine();
    // A decomposed é, ❤ with VS16, Thai ก with SARA I and Devanagari क
    // with vowel sign U, each followed by a narrow cell; then a wide
    // CJK character.
    let clusters = [
        "e\u{301}",
        "\u{2764}\u{fe0f}",
        "\u{e01}\u{e34}",
        "\u{915}\u{941}",
    ];
    let line = format!("{}\u{4e2d}|", clusters.map(|c| format!("{c}|")).concat());
    engine.write(line.as_bytes());
    let frame = frame(&engine);
    for (i, expected) in clusters.into_iter().enumerate() {
        let col = 2 * i;
        assert_eq!(cluster(&frame, 0, col), expected);
        let cell = frame.cell(0, col).unwrap();
        assert_eq!(usize::from(cell.grapheme_len), expected.chars().count());
        assert_eq!(Some(cell.codepoint), expected.chars().next().map(u32::from));
        assert_eq!(cluster(&frame, 0, col + 1), "|", "after {expected:?}");
    }
    // The wide character fills its cell; its tail holds nothing.
    assert_eq!(frame.grapheme(0, 8), [0x4e2d]);
    assert!(frame.grapheme(0, 9).is_empty());
    assert_eq!(frame.cell(0, 9).unwrap().grapheme_len, 0);
    assert_eq!(cluster(&frame, 0, 10), "|");
    assert_eq!(
        frame.row_text(0).trim_end(),
        line.replace('\u{4e2d}', "\u{4e2d} ")
    );
}

#[test]
fn with_grapheme_clustering_a_wide_cluster_takes_a_cell_and_its_tail() {
    let engine = engine();
    // Mode 2027: the program asks for Unicode grapheme clustering, so
    // a ZWJ sequence, a flag and a VS16 emoji each make one wide cell.
    engine.write(b"\x1b[?2027h");
    let clusters = [
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
        "\u{1f1e9}\u{1f1ea}",
        "\u{2764}\u{fe0f}",
        "\u{4e2d}",
    ];
    engine.write(clusters.map(|c| format!("{c}|")).concat().as_bytes());
    let frame = frame(&engine);
    for (i, expected) in clusters.into_iter().enumerate() {
        let col = 3 * i;
        assert_eq!(cluster(&frame, 0, col), expected);
        let cell = frame.cell(0, col).unwrap();
        assert_eq!(usize::from(cell.grapheme_len), expected.chars().count());
        assert!(
            frame.grapheme(0, col + 1).is_empty(),
            "the tail of {expected:?}"
        );
        assert_eq!(cluster(&frame, 0, col + 2), "|", "after {expected:?}");
    }
}

#[test]
fn the_cluster_buffer_grows_to_a_full_screen_and_is_reused() {
    let engine = engine();
    let mut frame = Frame::default();
    assert!(engine.snapshot(&mut frame));
    // Each of the 20×5 cells holds two codepoints.
    engine.write("e\u{301}".repeat(100).as_bytes());
    assert!(engine.snapshot(&mut frame));
    for row in 0..5 {
        for col in 0..20 {
            assert_eq!(frame.grapheme(row, col), [0x65, 0x301], "cell {col},{row}");
        }
    }
    // The next frame reuses the buffer; plain cells do not point into it.
    engine.reset();
    engine.write("x\u{301}y".as_bytes());
    assert!(engine.snapshot(&mut frame));
    assert_eq!(frame.grapheme(0, 0), [u32::from('x'), 0x301]);
    assert_eq!(frame.grapheme(0, 1), [u32::from('y')]);
    assert_eq!(frame.row_text(1).trim_end(), "");
}

#[test]
fn sgr_styles_and_colors_are_resolved() {
    let engine = engine();
    engine.write(b"\x1b[1;38;2;255;128;0;48;5;4mX\x1b[0mY");
    let frame = frame(&engine);
    let x = frame.cell(0, 0).unwrap();
    assert_eq!(x.codepoint, u32::from('X'));
    assert_ne!(x.flags & ffi::CELL_BOLD, 0);
    assert_eq!(x.fg, [255, 128, 0]);
    assert_eq!(x.has_bg, 1);
    let y = frame.cell(0, 1).unwrap();
    assert_eq!(y.flags, 0);
    assert_eq!(y.has_bg, 0);
}

#[test]
fn sgr_5_and_6_set_blink_and_sgr_25_clears_it() {
    let engine = engine();
    engine.write(b"\x1b[5ma\x1b[25mb\x1b[6mc\x1b[25md\x1b[5;6me\x1b[0mf");
    let frame = frame(&engine);
    let blink = |col| frame.cell(0, col).unwrap().flags & ffi::CELL_BLINK != 0;
    assert_eq!(
        [0, 1, 2, 3, 4, 5].map(blink),
        [true, false, true, false, true, false]
    );
}

#[test]
fn a_frame_blinks_only_while_blinking_text_shows() {
    let engine = engine();
    engine.write(b"plain");
    assert!(!frame(&engine).blinks());
    engine.write(b"\x1b[5m  \x1b[8mhidden\x1b[0m");
    assert!(!frame(&engine).blinks(), "nothing there changes");
    engine.write(b"\x1b[6mx\x1b[0m");
    assert!(frame(&engine).blinks());

    // Scrolled out of the viewport, and back.
    engine.write(&b"\r\n".repeat(10));
    assert!(!frame(&engine).blinks());
    engine.scroll(Scroll::Top);
    assert!(frame(&engine).blinks());
}

#[test]
fn cells_left_past_a_shrunk_grid_do_not_blink() {
    let engine = engine();
    engine.write(b"\x1b[5;1H\x1b[5mx\x1b[0m");
    let mut frame = frame(&engine);
    assert!(frame.blinks());
    engine.reset();
    assert!(engine.resize(20, 2, 10, 20));
    assert!(engine.snapshot(&mut frame));
    assert!(!frame.blinks());
}

/// Each SGR underline form reaches the cell as its own style, and the
/// forms that turn it off export none.
#[test]
fn sgr_underline_styles_are_exported() {
    let engine = engine();
    engine.write(
        b"\x1b[4ma\x1b[4:1mb\x1b[4:2mc\x1b[21md\x1b[4:3me\x1b[4:4mf\x1b[4:5mg\
          \x1b[4:0mh\x1b[4:3mi\x1b[24mj\x1b[4:3mk\x1b[0ml",
    );
    let frame = frame(&engine);
    let styles: Vec<u8> = (0..12)
        .map(|col| frame.cell(0, col).unwrap().underline)
        .collect();
    assert_eq!(
        styles,
        [
            ffi::UNDERLINE_SINGLE,
            ffi::UNDERLINE_SINGLE,
            ffi::UNDERLINE_DOUBLE,
            ffi::UNDERLINE_DOUBLE,
            ffi::UNDERLINE_CURLY,
            ffi::UNDERLINE_DOTTED,
            ffi::UNDERLINE_DASHED,
            ffi::UNDERLINE_NONE,
            ffi::UNDERLINE_CURLY,
            ffi::UNDERLINE_NONE,
            ffi::UNDERLINE_CURLY,
            ffi::UNDERLINE_NONE,
        ]
    );
}

/// SGR 58 sets the underline color in each of its forms, a palette
/// index through the palette in effect, and SGR 59 and 0 drop it.
#[test]
fn sgr_underline_color_is_resolved_and_reset() {
    let engine = engine();
    engine.write(b"\x1b]4;1;rgb:12/34/56\x07");
    engine.write(
        b"\x1b[4ma\x1b[58;2;255;128;0mb\x1b[58:2::1:2:3mc\x1b[58;5;1md\x1b[58:5:1me\
          \x1b[59mf\x1b[58;2;9;9;9mg\x1b[0mh",
    );
    let frame = frame(&engine);
    let color = |col| {
        let cell = frame.cell(0, col).unwrap();
        (cell.underline_has_rgb != 0).then_some(cell.underline_rgb)
    };
    assert_eq!(color(0), None);
    assert_eq!(color(1), Some([255, 128, 0]));
    assert_eq!(color(2), Some([1, 2, 3]));
    assert_eq!(color(3), Some([0x12, 0x34, 0x56]));
    assert_eq!(color(4), Some([0x12, 0x34, 0x56]));
    assert_eq!(color(5), None, "SGR 59");
    assert_eq!(frame.cell(0, 5).unwrap().underline, ffi::UNDERLINE_SINGLE);
    assert_eq!(color(6), Some([9, 9, 9]));
    assert_eq!(color(7), None, "SGR 0");
}

fn rgb([r, g, b]: [u8; 3]) -> Rgb {
    Rgb(r, g, b)
}

/// Light and Dark replace libghostty-vt's built-in colors, recolor
/// cells already drawn, and leave the 256-color cube and ramp xterm's.
#[test]
fn a_color_scheme_becomes_the_default_colors() {
    let engine = engine();
    // Default text; ANSI red on bright blue; 256-color 196 on 244.
    engine.write(b"d\x1b[31;104mr\x1b[0m\x1b[38;5;196;48;5;244mx");
    let built_in = frame(&engine);
    assert_eq!(built_in.info.cursor_has_rgb, 0);
    for scheme in [&LIGHT, &DARK] {
        assert_ne!(rgb(built_in.info.fg), scheme.foreground);
        assert_ne!(rgb(built_in.info.bg), scheme.background);
        assert_ne!(rgb(built_in.cell(0, 1).unwrap().fg), scheme.palette[1]);
        assert_ne!(rgb(built_in.cell(0, 1).unwrap().bg), scheme.palette[12]);
    }

    for scheme in [&LIGHT, &DARK] {
        assert!(engine.set_color_scheme(scheme));
        let frame = frame(&engine);
        let name = scheme.name;
        assert_eq!(rgb(frame.info.fg), scheme.foreground, "{name}");
        assert_eq!(rgb(frame.info.bg), scheme.background, "{name}");
        assert_eq!(frame.info.cursor_has_rgb, 1, "{name}");
        assert_eq!(rgb(frame.info.cursor_rgb), scheme.cursor, "{name}");
        let text = frame.cell(0, 0).unwrap();
        assert_eq!(rgb(text.fg), scheme.foreground, "{name}");
        assert_eq!(text.has_bg, 0, "{name}");
        assert_eq!(rgb(text.bg), scheme.background, "{name}");
        let ansi = frame.cell(0, 1).unwrap();
        assert_eq!(rgb(ansi.fg), scheme.palette[1], "{name}");
        assert_eq!(rgb(ansi.bg), scheme.palette[12], "{name}");
        let extended = frame.cell(0, 2).unwrap();
        assert_eq!(extended.fg, [0xff, 0x00, 0x00], "{name}: xterm's 196");
        assert_eq!(extended.bg, [0x80, 0x80, 0x80], "{name}: xterm's 244");
    }
}

/// Each of the 16 colors lands at its own index, by SGR 30–37 and
/// 90–97 as by `38;5;n`.
#[test]
fn every_ansi_index_resolves_through_the_scheme() {
    let palette: [Rgb; 16] = std::array::from_fn(|i| Rgb(i as u8 * 16, 0x40, 0xff - i as u8));
    let scheme = ColorScheme {
        name: "Test",
        palette,
        ..LIGHT
    };
    let engine = Engine::new(40, 5).expect("engine");
    assert!(engine.set_color_scheme(&scheme));
    for index in 0..16u8 {
        let sgr = if index < 8 { 30 + index } else { 82 + index };
        engine.write(format!("\x1b[{sgr}mA\x1b[38;5;{index}mB\x1b[0m").as_bytes());
    }
    let frame = frame(&engine);
    for (index, color) in palette.into_iter().enumerate() {
        let fg = |col| rgb(frame.cell(0, col).unwrap().fg);
        assert_eq!(fg(2 * index), color, "SGR for {index}");
        assert_eq!(fg(2 * index + 1), color, "38;5;{index}");
    }
}

/// A reset keeps the scheme; colors a program set (a palette entry,
/// the background) survive a scheme change.
#[test]
fn the_scheme_outlives_a_reset_and_yields_to_program_colors() {
    let engine = engine();
    assert!(engine.set_color_scheme(&DARK));
    engine.reset();
    engine.write(b"\x1b[31mr");
    let after_reset = frame(&engine);
    assert_eq!(rgb(after_reset.info.bg), DARK.background);
    assert_eq!(rgb(after_reset.cell(0, 0).unwrap().fg), DARK.palette[1]);

    engine.write(b"\x1b]4;1;rgb:12/34/56\x07");
    assert!(engine.set_color_scheme(&LIGHT));
    let switched = frame(&engine);
    assert_eq!(rgb(switched.info.bg), LIGHT.background);
    assert_eq!(switched.cell(0, 0).unwrap().fg, [0x12, 0x34, 0x56]);

    engine.write(b"\x1b]11;rgb:65/43/21\x07");
    assert!(engine.set_color_scheme(&DARK));
    assert_eq!(frame(&engine).info.bg, [0x65, 0x43, 0x21]);
}

#[test]
fn bell_and_device_attributes_raise_effects() {
    let engine = engine();
    let effects = engine.write(b"\x07\x1b[c");
    assert!(effects.contains(&Effect::Bell));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Pty(reply) if reply.starts_with(b"\x1b[?"))),
        "a primary DA reply: {effects:?}"
    );
    assert!(engine.write(b"plain").is_empty());
}

#[test]
fn an_osc_7_report_raises_the_raw_uri() {
    let engine = engine();
    assert_eq!(
        engine.write(b"\x1b]7;file://box/srv/my%20site\x07"),
        vec![Effect::Pwd("file://box/srv/my%20site".into())]
    );
}

#[test]
fn resize_reflows_and_reports_the_new_grid() {
    let engine = engine();
    assert!(engine.resize(40, 10, 8, 16));
    assert_eq!(engine.size(), (40, 10));
    assert_eq!(frame(&engine).cells.len(), 400);
}

/// `count` lines of 120 columns into a 120-column terminal limited to
/// `limit`: how many the terminal keeps.
fn kept_lines(limit: usize, count: usize) -> usize {
    let engine = Engine::new(120, 24).expect("engine");
    engine.set_scrollback_lines(limit);
    for line in 0..count {
        engine.write(format!("{line:0120}\r\n").as_bytes());
    }
    let text = engine.screen_text().expect("text");
    text.lines().filter(|line| !line.is_empty()).count()
}

#[test]
fn the_line_limit_alone_bounds_the_scrollback() {
    assert_eq!(kept_lines(10_000, 5_000), 5_000);
    // Pruning goes by page: 387 rows at this width.
    let kept = kept_lines(1_000, 5_000);
    assert!(kept.abs_diff(1_000) < 400, "{kept}");
}

#[test]
fn a_limit_of_zero_keeps_no_scrollback() {
    assert!(kept_lines(0, 5_000) <= 24, "{}", kept_lines(0, 5_000));
}

#[test]
fn screen_text_includes_scrollback() {
    let engine = engine();
    for line in 0..20 {
        engine.write(format!("line {line}\r\n").as_bytes());
    }
    let text = engine.screen_text().unwrap();
    assert!(text.contains("line 0\n"), "{text:?}");
    assert!(text.contains("line 19"), "{text:?}");
}

#[test]
fn reset_clears_the_screen() {
    let engine = engine();
    engine.write(b"junk");
    engine.reset();
    assert_eq!(frame(&engine).row_text(0).trim(), "");
}

use super::selection::drag_select;

/// A prompt as a shell marks it, `$ ` then the input.
fn shell_prompt(engine: &Engine) {
    engine.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
}

/// A finished command as a shell marks it: prompt, input, output, end.
fn shell_command(engine: &Engine, command: &str, output: &str) {
    shell_prompt(engine);
    let text = format!("{command}\r\n\x1b]133;C\x07{output}\x1b]133;D;0\x07");
    engine.write(text.as_bytes());
}

/// Three commands of four lines each, idle at a fourth prompt. Five
/// rows on screen: `three`'s prompt is the newest in the scrollback.
fn shell_session() -> Engine {
    let engine = engine();
    for name in ["one", "two", "three"] {
        let output: String = (1..=4).map(|n| format!("{name} {n}\r\n")).collect();
        shell_command(&engine, name, &output);
    }
    shell_prompt(&engine);
    engine
}

fn output_of(name: &str) -> String {
    (1..=4)
        .map(|n| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn top_row(engine: &Engine) -> String {
    frame(engine).row_text(0).trim_end().to_string()
}

#[test]
fn prompt_scrolling_steps_between_prompts_and_clamps() {
    let engine = shell_session();
    let bottom = top_row(&engine);
    for expected in ["$ three", "$ two", "$ one", "$ one"] {
        engine.scroll(Scroll::Prompts(-1));
        assert_eq!(top_row(&engine), expected);
    }
    for expected in ["$ two", "$ three"] {
        engine.scroll(Scroll::Prompts(1));
        assert_eq!(top_row(&engine), expected);
    }
    // The idle prompt is on screen: the viewport goes back to the bottom.
    engine.scroll(Scroll::Prompts(1));
    assert_eq!(top_row(&engine), bottom);
    engine.scroll(Scroll::Prompts(1));
    assert_eq!(top_row(&engine), bottom);

    engine.scroll(Scroll::Prompts(-2));
    assert_eq!(top_row(&engine), "$ two");
    engine.scroll(Scroll::Prompts(0));
    assert_eq!(top_row(&engine), "$ two");
}

#[test]
fn prompt_scrolling_stays_put_without_marks() {
    let engine = engine();
    for line in 0..20 {
        engine.write(format!("line {line}\r\n").as_bytes());
    }
    let bottom = top_row(&engine);
    engine.scroll(Scroll::Prompts(-1));
    assert_eq!(top_row(&engine), bottom);
    engine.scroll(Scroll::By(-3));
    let scrolled = top_row(&engine);
    assert_ne!(scrolled, bottom);
    engine.scroll(Scroll::Prompts(-1));
    assert_eq!(top_row(&engine), scrolled);
    engine.scroll(Scroll::Prompts(1));
    assert_eq!(top_row(&engine), scrolled);
}

#[test]
fn at_the_bottom_the_last_output_is_selected() {
    let engine = shell_session();
    assert!(engine.select_output());
    assert_eq!(engine.selection_text(), Some(output_of("three")));
    // The prompts of commands that printed nothing are passed over.
    shell_command(&engine, "true", "");
    shell_prompt(&engine);
    assert!(engine.select_output());
    assert_eq!(engine.selection_text(), Some(output_of("three")));
}

#[test]
fn after_a_jump_that_prompts_output_is_selected() {
    let engine = shell_session();
    engine.scroll(Scroll::Prompts(-2));
    assert_eq!(top_row(&engine), "$ two");
    assert!(engine.select_output());
    assert_eq!(engine.selection_text(), Some(output_of("two")));
}

#[test]
fn scrolled_into_output_that_output_is_selected() {
    let engine = shell_session();
    engine.scroll(Scroll::Top);
    engine.scroll(Scroll::By(2));
    assert_eq!(top_row(&engine), "one 2");
    assert!(engine.select_output());
    assert_eq!(engine.selection_text(), Some(output_of("one")));
}

#[test]
fn the_next_prompt_ends_output_without_a_command_end() {
    let engine = engine();
    shell_prompt(&engine);
    engine.write(b"old\r\n\x1b]133;C\x07no end\r\n");
    shell_prompt(&engine);
    assert!(engine.select_output());
    assert_eq!(engine.selection_text().as_deref(), Some("no end"));
}

#[test]
fn a_running_commands_output_so_far_is_selected() {
    let engine = engine();
    shell_prompt(&engine);
    engine.write(b"run\r\n\x1b]133;C\x07so far\r\nstill");
    assert!(engine.select_output());
    assert_eq!(engine.selection_text().as_deref(), Some("so far\nstill"));
}

#[test]
fn wrapped_output_is_selected_whole() {
    let engine = engine();
    let long = "x".repeat(30);
    shell_command(&engine, "long", &format!("{long}\r\n"));
    shell_prompt(&engine);
    assert!(engine.select_output());
    assert_eq!(engine.selection_text(), Some(long));
}

#[test]
fn nothing_is_selected_without_marks_or_output() {
    let engine = engine();
    engine.write(b"plain\r\nbash");
    assert!(!engine.select_output());
    assert_eq!(engine.selection_text(), None);

    // An existing selection stays.
    drag_select(&engine, (0, 0), (4, 0));
    assert!(!engine.select_output());
    assert_eq!(engine.selection_text().as_deref(), Some("plain"));

    // Marks, but nothing printed yet.
    let engine = self::engine();
    shell_prompt(&engine);
    assert!(!engine.select_output());
    assert_eq!(engine.selection_text(), None);

    // The alternate screen has no marks of its own.
    let engine = shell_session();
    engine.write(b"\x1b[?1049hfull screen");
    assert!(!engine.select_output());
    assert_eq!(engine.selection_text(), None);
}

#[test]
fn scrolling_moves_the_viewport() {
    let engine = engine();
    for line in 0..20 {
        engine.write(format!("line {line}\r\n").as_bytes());
    }
    engine.scroll(Scroll::Top);
    assert_eq!(frame(&engine).row_text(0).trim_end(), "line 0");
    engine.scroll(Scroll::Bottom);
    assert_ne!(frame(&engine).row_text(0).trim_end(), "line 0");
}

fn cursor(engine: &Engine) -> (u8, u8) {
    let mut frame = Frame::default();
    assert!(engine.snapshot(&mut frame));
    (frame.info.cursor_style, frame.info.cursor_blinking)
}

/// A configured default holds at once, through a program's own shape going
/// back with CSI 0 q, and through a full reset.
#[test]
fn the_default_cursor_takes_effect_and_outlives_a_reset() {
    let engine = Engine::new(4, 2).expect("engine");
    assert_eq!(cursor(&engine), (ffi::CURSOR_BLOCK, 0));

    assert!(engine.set_cursor_default(ffi::CURSOR_BAR, true));
    assert_eq!(cursor(&engine), (ffi::CURSOR_BAR, 1));

    engine.write(b"\x1b[4 q");
    assert_eq!(cursor(&engine), (ffi::CURSOR_UNDERLINE, 0));
    engine.write(b"\x1b[0 q");
    assert_eq!(cursor(&engine), (ffi::CURSOR_BAR, 1));

    engine.write(b"\x1b[4 q");
    engine.reset();
    assert_eq!(cursor(&engine), (ffi::CURSOR_BAR, 1));
}
