//! Glyphs and lines: what Pango shapes, pinned to the grid.

use std::ops::Range;

use gtk4::prelude::*;
use gtk4::{gdk, graphene, gsk, pango};

use super::{pixels, GhosttyView};
use crate::terminal::ghostty::ffi;

/// Lit and dark lengths of the patterned underlines, in line thicknesses.
const DOTTED: (i32, i32) = (1, 2);
const DASHED: (i32, i32) = (4, 2);

impl GhosttyView {
    /// Draw an underline of `ffi::UNDERLINE_*` style under the cells
    /// `columns` of the row at `y`.
    pub(super) fn draw_underline(
        &self,
        snapshot: &gtk4::Snapshot,
        style: u8,
        color: &gdk::RGBA,
        columns: Range<usize>,
        y: f32,
    ) {
        let cell = self.metrics().width;
        let span = columns.start as i32 * cell..columns.end as i32 * cell;
        match style {
            ffi::UNDERLINE_DOUBLE => {
                self.draw_pango_line(snapshot, pango::Underline::Double, color, span, y);
            }
            ffi::UNDERLINE_CURLY => self.draw_wave(snapshot, color, span, y),
            ffi::UNDERLINE_DOTTED => self.draw_pattern(snapshot, DOTTED, color, span, y),
            ffi::UNDERLINE_DASHED => self.draw_pattern(snapshot, DASHED, color, span, y),
            _ => self.draw_pango_line(snapshot, pango::Underline::Single, color, span, y),
        }
    }

    /// Draw a Pango underline across the pixels `span` of the row at `y`:
    /// the line of a blank shaped to the span's width. The font's line is
    /// rarely whole pixels, so it is moved and stretched onto the view's.
    fn draw_pango_line(
        &self,
        snapshot: &gtk4::Snapshot,
        underline: pango::Underline,
        color: &gdk::RGBA,
        span: Range<i32>,
        y: f32,
    ) {
        let blank = pango::Rectangle::new(
            0,
            -pango::SCALE,
            (span.end - span.start) * pango::SCALE,
            pango::SCALE,
        );
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrFontDesc::new(&self.font()));
        attrs.insert(pango::AttrShape::new(&blank, &blank));
        attrs.insert(pango::AttrInt::new_underline(underline));
        let layout = self.create_pango_layout(Some(" "));
        layout.set_attributes(Some(&attrs));

        let metrics = self.metrics();
        let (top, thickness) = metrics.underline;
        let (position, font_thickness) = metrics.font_underline;
        snapshot.save();
        snapshot.translate(&graphene::Point::new(span.start as f32, y + top));
        snapshot.scale(1.0, thickness / font_thickness.max(f32::EPSILON));
        snapshot.translate(&graphene::Point::new(
            0.0,
            position - pixels(layout.baseline()),
        ));
        snapshot.append_layout(&layout, color);
        snapshot.restore();
    }

    /// Draw a wave across the pixels `span` of the row at `y`, a crest and
    /// a trough per cell so that neighboring runs meet, down to the cell's
    /// bottom. Not Pango's error underline: GTK draws that as round dots.
    fn draw_wave(&self, snapshot: &gtk4::Snapshot, color: &gdk::RGBA, span: Range<i32>, y: f32) {
        let metrics = self.metrics();
        let (top, thickness) = metrics.underline;
        let height = (metrics.height as f32 - top).max(3.0 * thickness);
        let middle = y + top + height / 2.0;
        // A curve goes half way to its control point.
        let reach = height - thickness;
        let cell = metrics.width as f32;
        let path = gsk::PathBuilder::new();
        path.move_to(span.start as f32, middle);
        for x in span.step_by(metrics.width as usize) {
            let x = x as f32;
            path.quad_to(x + cell / 4.0, middle - reach, x + cell / 2.0, middle);
            path.quad_to(x + cell * 0.75, middle + reach, x + cell, middle);
        }
        snapshot.append_stroke(&path.to_path(), &gsk::Stroke::new(thickness), color);
    }

    /// Draw a dotted or dashed line across the pixels `span` of the row at
    /// `y`, Pango having neither: `on` lit and `off` dark, in thicknesses.
    fn draw_pattern(
        &self,
        snapshot: &gtk4::Snapshot,
        (on, off): (i32, i32),
        color: &gdk::RGBA,
        span: Range<i32>,
        y: f32,
    ) {
        let (top, thickness) = self.metrics().underline;
        let scale = thickness as i32;
        for (x, width) in line_pieces(span.start, span.end, on * scale, off * scale) {
            let line = graphene::Rect::new(x as f32, y + top, width as f32, thickness);
            snapshot.append_color(color, &line);
        }
    }

    /// Draw `text` from a point on the baseline, each byte in the cell
    /// `columns` gives it (see [`run_text`]).
    pub(super) fn draw_glyphs(
        &self,
        snapshot: &gtk4::Snapshot,
        text: &str,
        columns: &[usize],
        font: &pango::FontDescription,
        color: &gdk::RGBA,
        baseline: &graphene::Point,
    ) {
        self.shape(text, columns, font, |font, glyphs, _| {
            if let Some(node) = gsk::TextNode::new(font, glyphs, color, baseline) {
                snapshot.append_node(node);
            }
        });
    }

    /// Shape `text` for `each` Pango item: its font, its glyphs and each
    /// glyph's column. Pango picks and shapes the glyphs, a cluster's
    /// together; [`pin_glyphs`] places them at the cell their cluster
    /// comes from.
    pub(super) fn shape(
        &self,
        text: &str,
        columns: &[usize],
        font: &pango::FontDescription,
        mut each: impl FnMut(&pango::Font, &pango::GlyphString, &[usize]),
    ) {
        let cell = self.metrics().width * pango::SCALE;
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrFontDesc::new(font));
        let length = i32::try_from(text.len()).unwrap_or(i32::MAX);
        for item in pango::itemize(&self.pango_context(), text, 0, length, &attrs, None) {
            let offset = item.offset() as usize;
            let item_text = &text[offset..offset + item.length() as usize];
            let mut glyphs = pango::GlyphString::new();
            pango::shape_full(item_text, Some(text), item.analysis(), &mut glyphs);
            let columns: Vec<usize> = glyphs
                .log_clusters()
                .iter()
                .map(|&byte| columns[offset + byte as usize])
                .collect();
            let advances: Vec<i32> = glyphs
                .glyph_info()
                .iter()
                .map(|glyph| glyph.geometry().width())
                .collect();
            let offsets = pin_glyphs(&columns, &advances, cell);
            for (glyph, x) in glyphs.glyph_info_mut().iter_mut().zip(offsets) {
                let geometry = glyph.geometry_mut();
                geometry.set_x_offset(x + geometry.x_offset());
                geometry.set_width(0);
            }
            each(&item.analysis().font(), &glyphs, &columns);
        }
    }
}

/// Each glyph's x, in Pango units from its run's start, that puts it at
/// its cell: `columns[i]` is the column of glyph `i`'s cluster and
/// `advances[i]` its natural width. Glyphs of one cluster (a base and its
/// marks) keep their spacing from its first glyph; no advance carries over
/// to the next cell.
fn pin_glyphs(columns: &[usize], advances: &[i32], cell: i32) -> Vec<i32> {
    let mut cluster = None;
    let mut pen = 0;
    columns
        .iter()
        .zip(advances)
        .map(|(&col, &advance)| {
            if cluster.replace(col) != Some(col) {
                pen = 0;
            }
            let x = col as i32 * cell + pen;
            pen += advance;
            x
        })
        .collect()
}

/// The lit `(x, width)` pieces of a patterned line over the pixels
/// `start..end`: `on` lit and `off` dark, counted from the grid's left
/// edge so a line split into runs stays in step.
fn line_pieces(start: i32, end: i32, on: i32, off: i32) -> Vec<(i32, i32)> {
    let period = on + off;
    (start / period * period..end)
        .step_by(period as usize)
        .map(|x| (x.max(start), (x + on).min(end)))
        .filter(|(from, to)| from < to)
        .map(|(from, to)| (from, to - from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_pieces_follow_the_grid_not_the_run() {
        assert_eq!(line_pieces(0, 7, 1, 2), [(0, 1), (3, 1), (6, 1)]);
        // A run starting mid-pattern picks it up where it stands.
        assert_eq!(line_pieces(4, 13, 4, 2), [(6, 4), (12, 1)]);
        assert_eq!(line_pieces(2, 5, 4, 2), [(2, 2)]);
        assert_eq!(line_pieces(4, 6, 4, 2), []);
        // Split anywhere, a line comes out the same.
        let whole = line_pieces(0, 30, 4, 2);
        let mut halves = line_pieces(0, 12, 4, 2);
        halves.extend(line_pieces(12, 30, 4, 2));
        assert_eq!(whole, halves);
    }

    #[test]
    fn glyphs_sit_at_their_cells_whatever_the_font_advances() {
        // 9.6-pixel glyphs on a 10-pixel grid.
        let advance = 96 * pango::SCALE / 10;
        let cell = 10 * pango::SCALE;
        assert_eq!(
            pin_glyphs(&[0, 1, 2, 70], &[advance; 4], cell),
            vec![0, cell, 2 * cell, 70 * cell]
        );
        // Right-to-left items list their glyphs in visual order.
        assert_eq!(
            pin_glyphs(&[2, 1, 0], &[advance; 3], cell),
            vec![2 * cell, cell, 0]
        );
        // A ligature covers two cells; the next glyph starts after them.
        assert_eq!(
            pin_glyphs(&[0, 2], &[2 * advance, advance], cell),
            vec![0, 2 * cell]
        );
        // A base and its mark share a cluster: the mark keeps its place.
        assert_eq!(
            pin_glyphs(&[0, 0, 1], &[advance, 0, advance], cell),
            vec![0, advance, cell]
        );
    }
}
