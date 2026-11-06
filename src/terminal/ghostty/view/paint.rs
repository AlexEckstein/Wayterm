//! Each frame snapshots the engine's viewport and paints it with
//! `GtkSnapshot`: one background rectangle per run of equal colors, and
//! for each run of equal style, glyphs that Pango picks and shapes but
//! does not place: each goes at its own cell, since the font's advances
//! are rarely the whole-pixel cell width and would drift off the grid.
//! A cell's whole grapheme cluster is shaped together and sits at that
//! cell. Strikethrough is a rectangle; underlines run on across changes of
//! text color, in the SGR style and underline color of their cells. Search
//! matches are lit in Ghostty's colors, the current one apart, under the
//! selection and the cursor. The cursor takes the shape the terminal asks
//! for and blinks on a timer that runs only while a blink can show; without
//! focus it is a steady hollow block. Blinking text goes out and back on a
//! timer that runs only while a mapped view shows some. Kitty images come
//! from the same snapshot, each in its layer: over the default background,
//! over the cell backgrounds, or over the text.
//! The link under the pointer is underlined. Text an input method is
//! composing goes over everything, at the cursor.

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, pango};

use super::cursor::{cursor_color, cursor_rects, cursor_shape, is_cursor, CursorShape};
use super::{rgba, GhosttyView, Metrics};
use crate::terminal::ghostty::engine::{push_cluster, Cell as GridCell, Frame, FrameInfo, Span};
use crate::terminal::ghostty::ffi;
use crate::terminal::options::SearchColors;

/// The character of a Kitty placeholder cell; the image takes its place.
const KITTY_PLACEHOLDER: u32 = 0x10_eeee;

impl GhosttyView {
    pub(super) fn paint(&self, snapshot: &gtk4::Snapshot) {
        let Some(shared) = self.shared() else { return };
        let now_ms = glib::monotonic_time().unsigned_abs() / 1000;
        let next_ms = shared.engine.tick_animations(now_ms);
        let mut frame = self.imp().frame.borrow_mut();
        if !shared.engine.snapshot(&mut frame) {
            return;
        }
        self.follow_blink(frame.blinks());
        let off = self.imp().text_blink_off.get();
        if let Some(next_ms) = next_ms {
            self.redraw_after(next_ms.saturating_sub(now_ms));
        }
        self.update_textures(&shared, &frame);
        // Output and scrolling move text under a pointer that stays put.
        self.show_link(self.link_under(&shared, &frame));
        let link = self.imp().link.borrow();
        let info = frame.info;
        self.sync_blink(&info);
        let imp = self.imp();
        let cursor = cursor_shape(&info, imp.focused.get(), imp.blink_off.get());
        let metrics = self.metrics();
        let (cw, ch) = (metrics.width as f32, metrics.height as f32);
        snapshot.append_color(
            &rgba(info.bg, self.imp().opacity.get()),
            &graphene::Rect::new(0.0, 0.0, self.width() as f32, self.height() as f32),
        );

        let font = self.font();
        let (cols, rows) = (usize::from(info.cols), usize::from(info.rows));
        let marks = search_marks(&shared.engine.search_spans(), cols, rows);
        let search = self.imp().options.borrow().search;
        let painted: Vec<Vec<Painted>> = (0..rows)
            .map(|row| {
                let block = cursor == Some(CursorShape::Block);
                let mut cells = painted_row(&frame, row, &marks, block, &search);
                if let Some(link) = link.as_ref() {
                    link.underline(row, &mut cells);
                }
                cells
            })
            .collect();

        self.paint_images(snapshot, &frame, ffi::LAYER_BELOW_BG);
        for (row, cells) in painted.iter().enumerate() {
            paint_backgrounds(snapshot, cells, row as f32 * ch, (cw, ch));
        }
        self.paint_images(snapshot, &frame, ffi::LAYER_BELOW_TEXT);
        for (row, cells) in painted.iter().enumerate() {
            self.paint_text(snapshot, cells, row as f32 * ch, &metrics, &font);
            let y = row as f32 * ch;
            // Underlines: one per run of equal style and color, so text
            // changing color under a line does not break it. Blinking text
            // in its off phase hides its line with it.
            let mut col = 0;
            while col < cells.len() {
                let start = col;
                let underline = cells[start].underline();
                let shown = !cells[start].hidden(off);
                while col < cells.len()
                    && cells[col].underline() == underline
                    && !cells[col].hidden(off) == shown
                {
                    col += 1;
                }
                if let Some((style, color)) = underline.filter(|_| shown) {
                    self.draw_underline(snapshot, style, &color, start..col, y);
                }
            }

            // The cursor's outline, over the row's text.
            if row == usize::from(info.cursor_y) {
                let col = usize::from(info.cursor_x);
                if let (Some(shape), Some(cell)) = (cursor, cells.get(col)) {
                    let color = rgba(cursor_color(&info, cell), 1.0);
                    let origin = (col as f32 * cw, y);
                    for rect in cursor_rects(shape, origin, (cw, ch), metrics.underline.1) {
                        snapshot.append_color(&color, &rect);
                    }
                }
            }
        }
        self.paint_images(snapshot, &frame, ffi::LAYER_ABOVE_TEXT);
        self.paint_preedit(snapshot, &info, &metrics);
        self.follow_im_cursor(&info, &metrics);
    }

    /// One row's text at `y`: one run per style, each cluster at its own
    /// cell.
    fn paint_text(
        &self,
        snapshot: &gtk4::Snapshot,
        cells: &[Painted],
        y: f32,
        metrics: &Metrics,
        font: &pango::FontDescription,
    ) {
        let cw = metrics.width as f32;
        let mut col = 0;
        while col < cells.len() {
            let start = col;
            while col < cells.len() && cells[col].same_style(&cells[start]) {
                col += 1;
            }
            let (text, columns) = run_text(&cells[start..col]);
            let style = &cells[start];
            let off = self.imp().text_blink_off.get();
            if style.hidden(off) || text.trim().is_empty() {
                continue;
            }
            let color = rgba(style.fg, style.alpha());
            let x = start as f32 * cw;
            let baseline = graphene::Point::new(x, y + metrics.baseline);
            self.draw_glyphs(
                snapshot,
                &text,
                &columns,
                &style.font(font),
                &color,
                &baseline,
            );
            if style.flags & ffi::CELL_STRIKE != 0 {
                let (top, thickness) = metrics.strike;
                let width = (col - start) as f32 * cw;
                let line = graphene::Rect::new(x, y + top, width, thickness);
                snapshot.append_color(&color, &line);
            }
        }
    }
}

/// One row's backgrounds at `y`: one rectangle per run of equal,
/// non-default color.
fn paint_backgrounds(snapshot: &gtk4::Snapshot, cells: &[Painted], y: f32, (cw, ch): (f32, f32)) {
    let mut col = 0;
    while col < cells.len() {
        let start = col;
        while col < cells.len() && cells[col].bg == cells[start].bg {
            col += 1;
        }
        if let Some(bg) = cells[start].bg {
            snapshot.append_color(
                &rgba(bg, 1.0),
                &graphene::Rect::new(start as f32 * cw, y, (col - start) as f32 * cw, ch),
            );
        }
    }
}

/// A run's text and, for each of its bytes, the column of the cell it
/// comes from, counted from the run's first cell. A cell adds its whole
/// cluster, an empty cell or the tail of a wide one a space, so every cell
/// keeps its column: what follows a wide cluster starts two columns on.
pub(super) fn run_text(cells: &[Painted]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    for (col, cell) in cells.iter().enumerate() {
        push_cluster(&mut text, cell.cluster);
        columns.resize(text.len(), col);
    }
    (text, columns)
}

/// One viewport row's cells as painted, search marks included: per cell,
/// `Some(current)` lights it for the search, the current match apart, and
/// the selection and the cursor still draw on top. `block` says this frame
/// has a solid block cursor, the one shape that recolors its cell; the
/// others go over the cell as [`cursor_rects`].
pub(super) fn painted_row<'a>(
    frame: &'a Frame,
    row: usize,
    marks: &[Option<bool>],
    block: bool,
    search: &SearchColors,
) -> Vec<Painted<'a>> {
    let info = &frame.info;
    let cols = usize::from(info.cols);
    (0..cols)
        .filter_map(|col| frame.cell(row, col).map(|cell| (col, cell)))
        .map(|(col, cell)| {
            let cursor = block && is_cursor(info, row, col);
            let cluster = match frame.grapheme(row, col) {
                [KITTY_PLACEHOLDER, ..] => &[],
                cluster => cluster,
            };
            let painted = Painted::new(info, cell, cluster, cursor);
            match marks.get(row * cols + col) {
                Some(&Some(current)) if !cursor && cell.flags & ffi::CELL_SELECTED == 0 => {
                    painted.searched(current, search)
                }
                _ => painted,
            }
        })
        .collect()
}

/// One cell as painted: colors after inverse/selection/cursor swaps.
pub(super) struct Painted<'a> {
    /// The cell's grapheme cluster; empty for an empty cell and for the
    /// tail of a wide one.
    cluster: &'a [u32],
    pub(super) fg: [u8; 3],
    /// `None` = the frame background already painted underneath.
    pub(super) bg: Option<[u8; 3]>,
    flags: u16,
    /// `ffi::UNDERLINE_*`.
    pub(super) underline: u8,
    /// `None` = the line takes the text color.
    pub(super) underline_rgb: Option<[u8; 3]>,
}

/// Flags that change how text is drawn (not just its colors).
const TEXT_FLAGS: u16 = ffi::CELL_BOLD
    | ffi::CELL_ITALIC
    | ffi::CELL_STRIKE
    | ffi::CELL_FAINT
    | ffi::CELL_INVISIBLE
    | ffi::CELL_BLINK;

impl<'a> Painted<'a> {
    fn new(info: &FrameInfo, cell: &GridCell, cluster: &'a [u32], cursor: bool) -> Self {
        let swapped =
            (cell.flags & ffi::CELL_INVERSE != 0) ^ (cell.flags & ffi::CELL_SELECTED != 0) ^ cursor;
        let (fg, bg) = if swapped {
            let fg = if cursor && info.cursor_has_rgb != 0 {
                info.cursor_rgb
            } else {
                cell.fg
            };
            (cell.bg, Some(fg))
        } else {
            (cell.fg, (cell.has_bg != 0).then_some(cell.bg))
        };
        Self {
            cluster,
            fg,
            bg,
            flags: cell.flags & TEXT_FLAGS,
            underline: cell.underline,
            underline_rgb: (cell.underline_has_rgb != 0).then_some(cell.underline_rgb),
        }
    }

    /// Search highlight colors; `current` for the current match.
    fn searched(self, current: bool, colors: &SearchColors) -> Self {
        let bg = if current {
            colors.current_bg
        } else {
            colors.bg
        };
        Self {
            fg: colors.fg,
            bg: Some(bg),
            ..self
        }
    }

    /// Whether the text is out of sight: invisible, or blinking in its off
    /// phase. Only the text goes; the cell keeps its place and background.
    fn hidden(&self, blink_off: bool) -> bool {
        self.flags & ffi::CELL_INVISIBLE != 0 || (blink_off && self.flags & ffi::CELL_BLINK != 0)
    }

    fn same_style(&self, other: &Self) -> bool {
        self.fg == other.fg && self.flags == other.flags
    }

    /// Faint text and its lines are see-through.
    fn alpha(&self) -> f32 {
        if self.flags & ffi::CELL_FAINT != 0 {
            0.6
        } else {
            1.0
        }
    }

    /// The line to draw under this cell: its `ffi::UNDERLINE_*` style and
    /// its color.
    fn underline(&self) -> Option<(u8, gdk::RGBA)> {
        let hidden = self.flags & ffi::CELL_INVISIBLE != 0;
        (self.underline != ffi::UNDERLINE_NONE && !hidden).then(|| {
            let color = self.underline_rgb.unwrap_or(self.fg);
            (self.underline, rgba(color, self.alpha()))
        })
    }

    fn font(&self, base: &pango::FontDescription) -> pango::FontDescription {
        let mut font = base.clone();
        if self.flags & ffi::CELL_BOLD != 0 {
            font.set_weight(pango::Weight::Bold);
        }
        if self.flags & ffi::CELL_ITALIC != 0 {
            font.set_style(pango::Style::Italic);
        }
        font
    }
}

/// Per viewport cell, row-major: `Some(current)` where a search match is
/// lit. Empty without spans.
fn search_marks(spans: &[Span], cols: usize, rows: usize) -> Vec<Option<bool>> {
    if spans.is_empty() {
        return Vec::new();
    }
    let mut marks = vec![None; cols * rows];
    for span in spans.iter().filter(|span| usize::from(span.row) < rows) {
        let line = usize::from(span.row) * cols;
        let end = usize::from(span.end).min(cols);
        let start = usize::from(span.start).min(end);
        for mark in &mut marks[line + start..line + end] {
            *mark = Some(span.current != 0 || mark.unwrap_or(false));
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ghostty::engine::Engine;

    #[test]
    fn search_spans_mark_their_cells() {
        let span = |row, start, end, current| Span {
            row,
            start,
            end,
            current,
        };
        assert_eq!(search_marks(&[], 4, 2), []);
        let spans = [
            span(0, 1, 3, 0),
            span(1, 0, 2, 1),
            // Clipped to the grid; off it, or empty, nothing.
            span(1, 3, 9, 0),
            span(5, 0, 4, 1),
            span(0, 3, 2, 1),
        ];
        let (lit, current) = (Some(false), Some(true));
        assert_eq!(
            search_marks(&spans, 4, 2),
            [None, lit, lit, None, current, current, None, lit]
        );
        // The current match wins where matches overlap, in either order.
        for spans in [
            [span(0, 0, 2, 0), span(0, 1, 2, 1)],
            [span(0, 1, 2, 1), span(0, 0, 2, 0)],
        ] {
            assert_eq!(search_marks(&spans, 2, 1), [lit, current]);
        }
    }

    #[test]
    fn a_run_carries_each_cells_whole_cluster_at_its_column() {
        let engine = Engine::new(10, 1).expect("engine");
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        // Mode 2027 makes the family one wide cell.
        engine.write(format!("\x1b[?2027hx{family}e\u{301}y").as_bytes());
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let (text, columns) =
            run_text(&painted_row(&frame, 0, &[], true, &SearchColors::default())[..5]);
        // The family's tail is a space, so é and y keep their columns.
        let cells = [("x", 0), (family, 1), (" ", 2), ("e\u{301}", 3), ("y", 4)];
        assert_eq!(text, cells.map(|(cluster, _)| cluster).concat());
        let expected: Vec<usize> = cells
            .iter()
            .flat_map(|&(cluster, col)| std::iter::repeat_n(col, cluster.len()))
            .collect();
        assert_eq!(columns, expected);
    }

    /// The line a painted cell draws: its own color, else the text's,
    /// which search changes; nothing under hidden text.
    #[test]
    fn a_cell_underline_takes_its_color_and_style_from_the_cell() {
        let engine = Engine::new(10, 2).expect("engine");
        engine.write(
            b"\x1b[4:3;38;2;1;2;3ma\x1b[58;2;9;8;7mb\x1b[59;2mc\x1b[0;8;4md\x1b[0;58;2;9;8;7me",
        );
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let cells = painted_row(&frame, 0, &[], false, &SearchColors::default());
        assert_eq!(
            cells[0].underline(),
            Some((ffi::UNDERLINE_CURLY, rgba([1, 2, 3], 1.0)))
        );
        assert_eq!(
            cells[1].underline(),
            Some((ffi::UNDERLINE_CURLY, rgba([9, 8, 7], 1.0)))
        );
        assert_eq!(
            cells[2].underline(),
            Some((ffi::UNDERLINE_CURLY, rgba([1, 2, 3], 0.6))),
            "a faint line"
        );
        assert_eq!(cells[3].underline(), None, "hidden text");
        assert_eq!(cells[4].underline(), None, "a color but no style");

        let marks = [Some(true), Some(true)];
        let cells = painted_row(&frame, 0, &marks, false, &SearchColors::default());
        assert_eq!(
            cells[0].underline(),
            Some((ffi::UNDERLINE_CURLY, rgba(SearchColors::default().fg, 1.0)))
        );
        assert_eq!(
            cells[1].underline(),
            Some((ffi::UNDERLINE_CURLY, rgba([9, 8, 7], 1.0)))
        );
    }

    #[test]
    fn placeholder_cells_draw_no_text() {
        let engine = Engine::new(4, 1).expect("engine");
        engine.write("a\u{10eeee}\u{305}\u{305}b".as_bytes());
        let mut frame = Frame::default();
        assert!(engine.snapshot(&mut frame));
        let (text, _) =
            run_text(&painted_row(&frame, 0, &[], false, &SearchColors::default())[..3]);
        assert_eq!(text, "a b");
    }
}
