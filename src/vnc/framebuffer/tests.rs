use super::*;

const B: u32 = OPAQUE_BLACK;

/// BGRA wire bytes for `0x00RRGGBB` values, as vnc-rs delivers them.
fn wire(rgb: &[u32]) -> Vec<u8> {
    rgb.iter()
        .flat_map(|p| [*p as u8, (*p >> 8) as u8, (*p >> 16) as u8, 0])
        .collect()
}

/// A buffer where pixel (x, y) holds `y * 100 + x` (opaque), for
/// position-revealing assertions.
fn numbered(w: u16, h: u16) -> VncPixels {
    let mut fb = VncPixels::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = u32::from(y) * 100 + u32::from(x);
            fb.write_rect((x, y, 1, 1), &wire(&[v])).unwrap();
        }
    }
    fb
}

fn at(fb: &VncPixels, x: u16, y: u16) -> u32 {
    fb.pixels()[usize::from(y) * usize::from(fb.dims().0) + usize::from(x)] & 0x00FF_FFFF
}

#[test]
fn new_is_opaque_black_and_zero_sized_is_empty() {
    let fb = VncPixels::new(3, 2);
    assert_eq!(fb.dims(), (3, 2));
    assert_eq!(fb.pixels(), &[B; 6]);
    for (w, h) in [(0, 0), (0, 5), (5, 0)] {
        let fb = VncPixels::new(w, h);
        assert_eq!(fb.dims(), (w, h));
        assert!(fb.pixels().is_empty());
    }
}

#[test]
fn known_bytes_read_back_as_the_expected_u32() {
    let mut fb = VncPixels::new(2, 1);
    // [b, g, r, a] network order: blue=0x11 green=0x22 red=0x33 alpha=0.
    fb.write_rect(
        (0, 0, 2, 1),
        &[0x11, 0x22, 0x33, 0x00, 0xAA, 0xBB, 0xCC, 0x7F],
    )
    .unwrap();
    assert_eq!(fb.pixels(), &[0xFF33_2211, 0xFFCC_BBAA]);
}

#[test]
fn write_rect_lands_at_its_position_without_touching_neighbors() {
    let mut fb = VncPixels::new(4, 3);
    fb.write_rect((1, 1, 2, 1), &wire(&[0x00FF_0000, 0x0000_FF00]))
        .unwrap();
    assert_eq!(at(&fb, 1, 1), 0xFF0000);
    assert_eq!(at(&fb, 2, 1), 0x00FF00);
    let touched = fb.pixels().iter().filter(|p| **p != B).count();
    assert_eq!(touched, 2);
}

#[test]
fn write_rect_clamps_x_overrun() {
    let mut fb = VncPixels::new(4, 2);
    // 3 wide at x=3: only column 3 is in bounds; rows must not wrap.
    let data = wire(&[1, 2, 3, 4, 5, 6]);
    fb.write_rect((3, 0, 3, 2), &data).unwrap();
    assert_eq!(at(&fb, 3, 0), 1);
    assert_eq!(at(&fb, 3, 1), 4);
    let touched = fb.pixels().iter().filter(|p| **p != B).count();
    assert_eq!(touched, 2, "x overrun must not wrap into the next row");
}

#[test]
fn write_rect_clamps_y_overrun() {
    let mut fb = VncPixels::new(2, 3);
    let data = wire(&[1, 2, 3, 4, 5, 6]);
    fb.write_rect((0, 2, 2, 3), &data).unwrap();
    assert_eq!((at(&fb, 0, 2), at(&fb, 1, 2)), (1, 2));
    let touched = fb.pixels().iter().filter(|p| **p != B).count();
    assert_eq!(touched, 2);
}

#[test]
fn write_rect_fully_out_of_bounds_or_empty_is_a_no_op() {
    let mut fb = VncPixels::new(4, 4);
    let before = fb.clone();
    fb.write_rect((4, 0, 2, 2), &wire(&[1, 2, 3, 4])).unwrap();
    fb.write_rect((0, 4, 2, 2), &wire(&[1, 2, 3, 4])).unwrap();
    fb.write_rect((u16::MAX, u16::MAX, 2, 2), &wire(&[1, 2, 3, 4]))
        .unwrap();
    fb.write_rect((1, 1, 0, 3), &[]).unwrap();
    fb.write_rect((1, 1, 3, 0), &[]).unwrap();
    assert_eq!(fb, before);
}

#[test]
fn write_rect_rejects_a_mismatched_stride_and_writes_nothing() {
    let mut fb = VncPixels::new(4, 4);
    let before = fb.clone();
    let err = fb.write_rect((0, 0, 2, 2), &wire(&[1, 2, 3])).unwrap_err();
    assert_eq!(
        err,
        FramebufferError::RectLength {
            expected: 16,
            got: 12
        }
    );
    assert_eq!(fb, before);
}

#[test]
fn write_to_a_zero_sized_buffer_is_a_no_op() {
    let mut fb = VncPixels::new(0, 0);
    fb.write_rect((0, 0, 1, 1), &wire(&[1])).unwrap();
    fb.copy((0, 0, 1, 1), (0, 0, 1, 1));
    assert!(fb.pixels().is_empty());
}

#[test]
fn copy_moves_a_region_between_disjoint_areas() {
    let mut fb = numbered(6, 4);
    fb.copy((4, 2, 2, 2), (0, 0, 2, 2));
    for dy in 0..2 {
        for dx in 0..2 {
            assert_eq!(at(&fb, 4 + dx, 2 + dy), u32::from(dy) * 100 + u32::from(dx));
        }
    }
    // Source is untouched.
    assert_eq!(at(&fb, 1, 1), 101);
}

#[test]
fn copy_overlapping_toward_bottom_right_does_not_smear() {
    let mut fb = numbered(5, 5);
    // 3x3 block at (0,0) -> (1,1): overlaps itself on both axes.
    fb.copy((1, 1, 3, 3), (0, 0, 3, 3));
    for dy in 0..3u16 {
        for dx in 0..3u16 {
            assert_eq!(
                at(&fb, 1 + dx, 1 + dy),
                u32::from(dy) * 100 + u32::from(dx),
                "dst ({dx},{dy})"
            );
        }
    }
}

#[test]
fn copy_overlapping_toward_top_left_does_not_smear() {
    let mut fb = numbered(5, 5);
    fb.copy((0, 0, 3, 3), (1, 1, 3, 3));
    for dy in 0..3u16 {
        for dx in 0..3u16 {
            assert_eq!(
                at(&fb, dx, dy),
                (u32::from(dy) + 1) * 100 + u32::from(dx) + 1,
                "dst ({dx},{dy})"
            );
        }
    }
}

#[test]
fn copy_horizontal_overlap_in_one_row_does_not_smear() {
    let mut fb = numbered(6, 1);
    fb.copy((1, 0, 4, 1), (0, 0, 4, 1));
    assert_eq!(
        (1..5).map(|x| at(&fb, x, 0)).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn copy_clamps_to_both_regions_and_ignores_out_of_bounds() {
    let mut fb = numbered(4, 4);
    // Destination overruns the right edge: only 1 column fits.
    fb.copy((3, 0, 3, 2), (0, 0, 3, 2));
    assert_eq!((at(&fb, 3, 0), at(&fb, 3, 1)), (0, 100));
    // Source overruns the bottom edge: only 1 row fits.
    let mut fb = numbered(4, 4);
    fb.copy((0, 0, 2, 3), (0, 3, 2, 3));
    assert_eq!((at(&fb, 0, 0), at(&fb, 1, 0)), (300, 301));
    assert_eq!(at(&fb, 0, 1), 100, "rows beyond the clamp stay untouched");
    // Entirely out of bounds either side: no-op.
    let mut fb = numbered(4, 4);
    let before = fb.clone();
    fb.copy((9, 9, 2, 2), (0, 0, 2, 2));
    fb.copy((0, 0, 2, 2), (9, 9, 2, 2));
    fb.copy((0, 0, 0, 2), (1, 1, 0, 2));
    assert_eq!(fb, before);
}

#[test]
fn resize_grow_keeps_pixels_and_fills_black() {
    let mut fb = numbered(2, 2);
    fb.resize(3, 3);
    assert_eq!(fb.dims(), (3, 3));
    assert_eq!(fb.pixels().len(), 9);
    assert_eq!(
        [at(&fb, 0, 0), at(&fb, 1, 0), at(&fb, 0, 1), at(&fb, 1, 1)],
        [0, 1, 100, 101]
    );
    for (x, y) in [(2, 0), (2, 1), (0, 2), (1, 2), (2, 2)] {
        assert_eq!(fb.pixels()[y * 3 + x], B, "({x},{y}) is new");
    }
}

#[test]
fn resize_shrink_crops_to_the_overlap() {
    let mut fb = numbered(4, 3);
    fb.resize(2, 2);
    assert_eq!(fb.dims(), (2, 2));
    assert_eq!(
        fb.pixels().iter().map(|p| p & 0xFFFFFF).collect::<Vec<_>>(),
        vec![0, 1, 100, 101]
    );
}

#[test]
fn resize_mixed_and_to_from_empty() {
    let mut fb = numbered(3, 3);
    fb.resize(5, 1);
    assert_eq!(
        (0..3).map(|x| at(&fb, x, 0)).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(fb.pixels()[3], B);
    fb.resize(0, 0);
    assert!(fb.pixels().is_empty());
    fb.resize(2, 2);
    assert_eq!(fb.pixels(), &[B; 4]);
}

#[test]
fn size_limit_covers_real_desktops_and_refuses_absurd_ones() {
    for (w, h) in [
        (0, 0),
        (1920, 1080),
        (7680, 4320),
        (15360, 4320),
        (16384, 4096),
    ] {
        assert!(VncPixels::fits(w, h), "{w}x{h}");
    }
    for (w, h) in [(16385, 1), (1, 16385), (16384, 4097), (u16::MAX, u16::MAX)] {
        assert!(!VncPixels::fits(w, h), "{w}x{h}");
    }
}

#[test]
fn snapshot_copies_the_pixels_and_reuses_the_buffer() {
    let fb = numbered(2, 2);
    let mut out = vec![9; 17];
    fb.write_snapshot(&mut out);
    assert_eq!(out, fb.pixels());
}
