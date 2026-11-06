//! How a frame's cursor is drawn.

use gtk4::graphene;

use super::paint::Painted;
use crate::terminal::ghostty::engine::FrameInfo;
use crate::terminal::ghostty::ffi;

/// How the cursor is drawn in one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CursorShape {
    Block,
    Underline,
    Bar,
    Hollow,
}

pub(super) fn is_cursor(info: &FrameInfo, row: usize, col: usize) -> bool {
    info.cursor_visible != 0
        && usize::from(info.cursor_y) == row
        && usize::from(info.cursor_x) == col
}

/// How the frame's cursor is drawn, if at all: hidden by the terminal or
/// in the blink's off phase it is not, and without focus it stays up as a
/// hollow block, as in Ghostty.
pub(super) fn cursor_shape(
    info: &FrameInfo,
    focused: bool,
    blink_off: bool,
) -> Option<CursorShape> {
    if info.cursor_visible == 0 {
        return None;
    }
    if !focused {
        return Some(CursorShape::Hollow);
    }
    if info.cursor_blinking != 0 && blink_off {
        return None;
    }
    Some(match info.cursor_style {
        ffi::CURSOR_UNDERLINE => CursorShape::Underline,
        ffi::CURSOR_BAR => CursorShape::Bar,
        ffi::CURSOR_BLOCK_HOLLOW => CursorShape::Hollow,
        _ => CursorShape::Block,
    })
}

/// Whether a blink can show: anywhere else the timer would only wake the
/// main loop.
pub(super) fn needs_blink_timer(info: &FrameInfo, focused: bool, mapped: bool) -> bool {
    info.cursor_visible != 0 && info.cursor_blinking != 0 && focused && mapped
}

/// The color of a cursor drawn over `cell`: the terminal's cursor color,
/// else the cell's text as painted, which shows on its background.
pub(super) fn cursor_color(info: &FrameInfo, cell: &Painted) -> [u8; 3] {
    if info.cursor_has_rgb != 0 {
        info.cursor_rgb
    } else {
        cell.fg
    }
}

/// The rectangles of a cursor `line` thick in the cell at `origin`; none
/// for the block, which recolors the cell instead (see [`painted_row`]).
pub(super) fn cursor_rects(
    shape: CursorShape,
    (x, y): (f32, f32),
    (width, height): (f32, f32),
    line: f32,
) -> Vec<graphene::Rect> {
    let top = graphene::Rect::new(x, y, width, line);
    let bottom = graphene::Rect::new(x, y + height - line, width, line);
    let left = graphene::Rect::new(x, y, line, height);
    let right = graphene::Rect::new(x + width - line, y, line, height);
    match shape {
        CursorShape::Block => Vec::new(),
        CursorShape::Underline => vec![bottom],
        CursorShape::Bar => vec![left],
        CursorShape::Hollow => vec![top, bottom, left, right],
    }
}

#[cfg(test)]
mod tests {
    use super::super::paint::painted_row;
    use super::*;
    use crate::terminal::ghostty::engine::{Engine, Frame};
    use crate::terminal::options::{GhosttyOptions, SearchColors};

    /// A shown cursor's frame state, as the shim fills it.
    fn cursor_info(style: u8, blinking: bool) -> FrameInfo {
        FrameInfo {
            cursor_visible: 1,
            cursor_style: style,
            cursor_blinking: u8::from(blinking),
            ..FrameInfo::default()
        }
    }

    #[test]
    fn the_cursor_shape_follows_the_terminal_the_focus_and_the_blink() {
        use CursorShape::{Bar, Block, Hollow, Underline};
        for (style, shape) in [
            (ffi::CURSOR_BLOCK, Block),
            (ffi::CURSOR_UNDERLINE, Underline),
            (ffi::CURSOR_BAR, Bar),
            (ffi::CURSOR_BLOCK_HOLLOW, Hollow),
        ] {
            let (steady, blinking) = (cursor_info(style, false), cursor_info(style, true));
            assert_eq!(cursor_shape(&steady, true, false), Some(shape));
            assert_eq!(cursor_shape(&steady, true, true), Some(shape));
            assert_eq!(cursor_shape(&blinking, true, false), Some(shape));
            assert_eq!(cursor_shape(&blinking, true, true), None);
            // Without focus it stays up as a hollow block, whatever the phase.
            assert_eq!(cursor_shape(&steady, false, false), Some(Hollow));
            assert_eq!(cursor_shape(&blinking, false, true), Some(Hollow));
        }
        let hidden = FrameInfo {
            cursor_visible: 0,
            ..cursor_info(ffi::CURSOR_BAR, false)
        };
        assert_eq!(cursor_shape(&hidden, true, false), None);
        assert_eq!(cursor_shape(&hidden, false, false), None);
    }

    #[test]
    fn the_blink_timer_runs_only_while_a_blink_can_show() {
        let blinking = cursor_info(ffi::CURSOR_BLOCK, true);
        assert!(needs_blink_timer(&blinking, true, true));
        assert!(!needs_blink_timer(&blinking, false, true), "unfocused");
        assert!(!needs_blink_timer(&blinking, true, false), "unmapped");
        let steady = cursor_info(ffi::CURSOR_BLOCK, false);
        assert!(!needs_blink_timer(&steady, true, true));
        let hidden = FrameInfo {
            cursor_visible: 0,
            ..blinking
        };
        assert!(!needs_blink_timer(&hidden, true, true));
    }

    #[test]
    fn cursor_shapes_sit_on_the_edges_of_their_cell() {
        let rects = |shape| {
            cursor_rects(shape, (20.0, 40.0), (10.0, 18.0), 2.0)
                .iter()
                .map(|r| (r.x(), r.y(), r.width(), r.height()))
                .collect::<Vec<_>>()
        };
        assert_eq!(rects(CursorShape::Block), [], "it recolors the cell");
        assert_eq!(rects(CursorShape::Underline), [(20.0, 56.0, 10.0, 2.0)]);
        assert_eq!(rects(CursorShape::Bar), [(20.0, 40.0, 2.0, 18.0)]);
        assert_eq!(
            rects(CursorShape::Hollow),
            [
                (20.0, 40.0, 10.0, 2.0),
                (20.0, 56.0, 10.0, 2.0),
                (20.0, 40.0, 2.0, 18.0),
                (28.0, 40.0, 2.0, 18.0),
            ]
        );
    }

    /// Only a solid block takes over its cell: any other frame paints the
    /// cell as usual, search mark included, for the cursor to go on top.
    #[test]
    fn only_a_solid_block_recolors_the_cell_under_the_cursor() {
        let engine = Engine::new(4, 1).expect("engine");
        engine.write(b"ab\x1b[D");
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let cell = *frame.cell(0, 1).unwrap();
        let marks = [None, Some(true), None, None];

        let block = &painted_row(&frame, 0, &marks, true, &GhosttyOptions::default())[1];
        assert_eq!((block.fg, block.bg), (cell.bg, Some(cell.fg)));
        let plain = &painted_row(&frame, 0, &[], false, &GhosttyOptions::default())[1];
        assert_eq!((plain.fg, plain.bg), (cell.fg, None));
        let marked = &painted_row(&frame, 0, &marks, false, &GhosttyOptions::default())[1];
        let search = SearchColors::default();
        assert_eq!((marked.fg, marked.bg), (search.fg, Some(search.current_bg)));
    }

    /// An outline takes the cursor color, else the text color its cell is
    /// painted in, so it shows on an inverse cell too.
    #[test]
    fn an_outline_cursor_shows_on_any_cell() {
        let engine = Engine::new(4, 1).expect("engine");
        engine.write(b"\x1b[7;31;42ma\x1b[0m\x1b[D");
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let cells = painted_row(&frame, 0, &[], false, &GhosttyOptions::default());
        let cell = frame.cell(0, 0).unwrap();
        assert_eq!(cells[0].bg, Some(cell.fg));
        assert_eq!(cursor_color(&frame.info, &cells[0]), cell.bg);

        let info = FrameInfo {
            cursor_rgb: [1, 2, 3],
            cursor_has_rgb: 1,
            ..frame.info
        };
        assert_eq!(cursor_color(&info, &cells[0]), [1, 2, 3]);
    }
}
