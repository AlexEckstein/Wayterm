//! Kitty images: their textures and where they go.

use std::time::Duration;

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene};

use super::GhosttyView;
use crate::terminal::ghostty::engine::{Frame, Pixels, Placement};
use crate::terminal::ghostty::{ffi, Shared};

/// The shortest wait for an animation's next frame, in milliseconds.
const MIN_ANIMATION_DELAY: u64 = 8;

impl GhosttyView {
    /// Redraw in `delay_ms`, for an animation's next frame. It asks for one
    /// frame, which asks for the next: a view off screen draws none, so
    /// its animations wait until it shows again.
    pub(super) fn redraw_after(&self, delay_ms: u64) {
        if self.imp().animating.replace(true) {
            return;
        }
        let view = self.downgrade();
        let delay = Duration::from_millis(delay_ms.max(MIN_ANIMATION_DELAY));
        glib::timeout_add_local_once(delay, move || {
            if let Some(view) = view.upgrade() {
                view.imp().animating.set(false);
                view.queue_draw();
            }
        });
    }

    /// Hold the textures this frame draws and no others: an image's pixels
    /// are read only when its generation changes, and what is deleted or
    /// off screen is let go.
    pub(super) fn update_textures(&self, shared: &Shared, frame: &Frame) {
        let mut textures = self.imp().textures.borrow_mut();
        let mut last = std::mem::take(&mut *textures);
        for placement in &frame.placements {
            let key = (placement.image_id, placement.generation);
            if textures.contains_key(&key) {
                continue;
            }
            let texture = last.remove(&key).or_else(|| {
                let pixels = shared.engine.image_pixels(placement.image_id)?;
                texture(pixels)
            });
            if let Some(texture) = texture {
                textures.insert(key, texture);
            }
        }
    }

    /// Draw one layer's images, clipped to the grid. `frame` holds them in
    /// drawing order.
    pub(super) fn paint_images(&self, snapshot: &gtk4::Snapshot, frame: &Frame, layer: u8) {
        let textures = self.imp().textures.borrow();
        let (cell_w, cell_h) = self.cell_size();
        let grid = graphene::Rect::new(
            0.0,
            0.0,
            (i32::from(frame.info.cols) * cell_w) as f32,
            (i32::from(frame.info.rows) * cell_h) as f32,
        );
        for placement in frame.placements.iter().filter(|p| p.layer == layer) {
            let Some(texture) = textures.get(&(placement.image_id, placement.generation)) else {
                continue;
            };
            let size = (texture.width() as f32, texture.height() as f32);
            let Some((target, whole)) = image_rects(placement, (cell_w, cell_h), size) else {
                continue;
            };
            let Some(clip) = target.intersection(&grid) else {
                continue;
            };
            snapshot.push_clip(&clip);
            snapshot.append_texture(texture, &whole);
            snapshot.pop();
        }
    }
}

/// An image's pixels as a texture. Kitty's alpha is straight, as these
/// formats take it. `None` if they do not add up to the image's size.
fn texture(pixels: Pixels) -> Option<gdk::Texture> {
    let (format, bytes) = match pixels.format {
        ffi::FORMAT_RGB => (gdk::MemoryFormat::R8g8b8, 3),
        ffi::FORMAT_RGBA => (gdk::MemoryFormat::R8g8b8a8, 4),
        ffi::FORMAT_GRAY_ALPHA => (gdk::MemoryFormat::G8a8, 2),
        ffi::FORMAT_GRAY => (gdk::MemoryFormat::G8, 1),
        _ => return None,
    };
    let stride = pixels.width as usize * bytes;
    let width = i32::try_from(pixels.width).ok().filter(|&w| w > 0)?;
    let height = i32::try_from(pixels.height).ok().filter(|&h| h > 0)?;
    if pixels.data.len() != stride * pixels.height as usize {
        return None;
    }
    let data = glib::Bytes::from_owned(pixels.data);
    Some(gdk::MemoryTexture::new(width, height, format, &data, stride).upcast())
}

/// Where a placement goes, in widget pixels: its target rectangle, and
/// the rectangle the whole `image` scales to so that the source rectangle
/// lands on the target. Drawing the whole image clipped to the target
/// crops it with no copy. `None` for a placement with nothing to show.
fn image_rects(
    placement: &Placement,
    (cell_w, cell_h): (i32, i32),
    (image_w, image_h): (f32, f32),
) -> Option<(graphene::Rect, graphene::Rect)> {
    if placement.source_width == 0 || placement.source_height == 0 {
        return None;
    }
    let x = (i64::from(placement.col) * i64::from(cell_w) + i64::from(placement.offset_x)) as f32;
    let y = (i64::from(placement.row) * i64::from(cell_h) + i64::from(placement.offset_y)) as f32;
    let (width, height) = (placement.width as f32, placement.height as f32);
    let scale_x = width / placement.source_width as f32;
    let scale_y = height / placement.source_height as f32;
    let whole = graphene::Rect::new(
        x - placement.source_x as f32 * scale_x,
        y - placement.source_y as f32 * scale_y,
        image_w * scale_x,
        image_h * scale_y,
    );
    Some((graphene::Rect::new(x, y, width, height), whole))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(col: i32, row: i32) -> Placement {
        Placement {
            col,
            row,
            width: 40,
            height: 60,
            source_width: 40,
            source_height: 60,
            ..Placement::default()
        }
    }

    fn sides(rect: &graphene::Rect) -> [f32; 4] {
        [rect.x(), rect.y(), rect.width(), rect.height()]
    }

    #[test]
    fn an_image_sits_at_its_cell_plus_its_offset() {
        let offset = Placement {
            offset_x: 3,
            offset_y: 5,
            ..placement(2, 1)
        };
        let (target, whole) = image_rects(&offset, (10, 20), (40.0, 60.0)).unwrap();
        assert_eq!(sides(&target), [23.0, 25.0, 40.0, 60.0]);
        assert_eq!(sides(&whole), sides(&target));
        // Above the viewport it starts off the widget, to be clipped.
        let (target, _) = image_rects(&placement(0, -2), (10, 20), (40.0, 60.0)).unwrap();
        assert_eq!(sides(&target), [0.0, -40.0, 40.0, 60.0]);
    }

    #[test]
    fn the_source_rectangle_lands_on_the_target() {
        // The right half of a 40 × 60 image, drawn twice its size.
        let half = Placement {
            width: 40,
            height: 120,
            source_x: 20,
            source_width: 20,
            ..placement(1, 0)
        };
        let (target, whole) = image_rects(&half, (10, 20), (40.0, 60.0)).unwrap();
        assert_eq!(sides(&target), [10.0, 0.0, 40.0, 120.0]);
        assert_eq!(sides(&whole), [-30.0, 0.0, 80.0, 120.0]);
    }

    #[test]
    fn an_empty_source_rectangle_draws_nothing() {
        let empty = Placement {
            source_width: 0,
            ..placement(0, 0)
        };
        assert!(image_rects(&empty, (10, 20), (40.0, 60.0)).is_none());
    }

    #[test]
    fn every_stored_format_makes_a_texture() {
        let pixels = |format, data: &[u8]| Pixels {
            width: 2,
            height: 1,
            format,
            data: data.to_vec(),
        };
        for (format, bytes) in [
            (ffi::FORMAT_RGB, 3),
            (ffi::FORMAT_RGBA, 4),
            (ffi::FORMAT_GRAY_ALPHA, 2),
            (ffi::FORMAT_GRAY, 1),
        ] {
            let data = vec![9; 2 * bytes];
            let made = texture(pixels(format, &data)).expect("texture");
            assert_eq!((made.width(), made.height()), (2, 1));
            assert!(texture(pixels(format, &data[1..])).is_none(), "short");
        }
        assert!(texture(pixels(2, &[0; 8])).is_none(), "PNG is never stored");
    }
}
