mod clipboard;
mod graphics;
mod input;
mod links;
mod notifications;
mod screen;
mod search;
mod selection;

use super::*;

fn engine() -> Engine {
    Engine::new(20, 5).expect("engine")
}

fn frame(engine: &Engine) -> Frame {
    let mut frame = Frame::default();
    assert!(engine.snapshot(&mut frame));
    frame
}

#[test]
fn rust_mirrors_match_the_c_layout() {
    // SAFETY: plain getters.
    let (cell, info, span, placement, pixels) = unsafe {
        (
            ffi::wt_ghostty_cell_size(),
            ffi::wt_ghostty_frame_size(),
            ffi::wt_ghostty_span_size(),
            ffi::wt_ghostty_placement_size(),
            ffi::wt_ghostty_pixels_size(),
        )
    };
    assert_eq!(std::mem::size_of::<Cell>(), cell);
    assert_eq!(std::mem::size_of::<FrameInfo>(), info);
    assert_eq!(std::mem::size_of::<Span>(), span);
    assert_eq!(std::mem::size_of::<Placement>(), placement);
    assert_eq!(std::mem::size_of::<ffi::WtPixels>(), pixels);
    // SAFETY: plain getters.
    let (grapheme, grapheme_len) = unsafe {
        (
            ffi::wt_ghostty_cell_grapheme_offset(),
            ffi::wt_ghostty_cell_grapheme_len_offset(),
        )
    };
    assert_eq!(std::mem::offset_of!(Cell, grapheme), grapheme);
    assert_eq!(std::mem::offset_of!(Cell, grapheme_len), grapheme_len);
    // SAFETY: plain getter.
    let link = unsafe { ffi::wt_ghostty_cell_link_offset() };
    assert_eq!(std::mem::offset_of!(Cell, link), link);
    // SAFETY: plain getters.
    let (layer, format) = unsafe {
        (
            ffi::wt_ghostty_placement_layer_offset(),
            ffi::wt_ghostty_pixels_format_offset(),
        )
    };
    assert_eq!(std::mem::offset_of!(Placement, layer), layer);
    assert_eq!(std::mem::offset_of!(ffi::WtPixels, format), format);
    let (underline, underline_rgb, underline_has_rgb) = unsafe {
        (
            ffi::wt_ghostty_cell_underline_offset(),
            ffi::wt_ghostty_cell_underline_rgb_offset(),
            ffi::wt_ghostty_cell_underline_has_rgb_offset(),
        )
    };
    assert_eq!(std::mem::offset_of!(Cell, underline), underline);
    assert_eq!(std::mem::offset_of!(Cell, underline_rgb), underline_rgb);
    assert_eq!(
        std::mem::offset_of!(Cell, underline_has_rgb),
        underline_has_rgb
    );
}
