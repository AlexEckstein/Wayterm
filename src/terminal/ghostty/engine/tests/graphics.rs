use gtk4::prelude::*;
use gtk4::{gdk, glib};

use super::super::png::{decodable, png_size};
use super::*;

/// A Kitty graphics command; `payload` is base64-encoded.
fn kitty(control: &str, payload: &[u8]) -> Vec<u8> {
    format!("\x1b_G{control};{}\x1b\\", glib::base64_encode(payload)).into_bytes()
}

/// A terminal that knows its cell size: 8 × 16 pixels.
fn sized_engine() -> Engine {
    let engine = engine();
    assert!(engine.resize(20, 5, 8, 16));
    engine
}

/// `w` × `h` opaque red RGBA pixels.
fn red(w: usize, h: usize) -> Vec<u8> {
    [255, 0, 0, 255].repeat(w * h)
}

/// Show image `id`, `w` × `h` RGBA pixels, at the cursor, replying to
/// nothing; `extra` adds to the command.
fn show(engine: &Engine, id: u32, (w, h): (usize, usize), extra: &str) {
    let control = format!("a=T,f=32,i={id},s={w},v={h},q=2{extra}");
    assert_eq!(engine.write(&kitty(&control, &red(w, h))), []);
}

fn placements(engine: &Engine) -> Vec<Placement> {
    frame(engine).placements
}

#[test]
fn the_storage_limit_is_the_one_png_sizes_are_checked_against() {
    let engine = engine();
    // SAFETY: valid terminal.
    let limit = unsafe { ffi::wt_ghostty_storage_limit(engine.ptr()) };
    assert_eq!(limit, ffi::KITTY_STORAGE_LIMIT);
}

#[test]
fn a_placed_image_comes_out_at_its_cell() {
    let engine = sized_engine();
    engine.write(b"\x1b[2;4H");
    show(&engine, 7, (16, 32), ",X=3,Y=5");
    let [placement] = placements(&engine)[..] else {
        panic!("{:?}", placements(&engine));
    };
    assert_eq!(placement.image_id, 7);
    assert_ne!(placement.generation, 0);
    assert_eq!((placement.col, placement.row), (3, 1));
    assert_eq!((placement.offset_x, placement.offset_y), (3, 5));
    assert_eq!((placement.width, placement.height), (16, 32));
    assert_eq!((placement.source_x, placement.source_y), (0, 0));
    assert_eq!((placement.source_width, placement.source_height), (16, 32));
    assert_eq!(placement.layer, ffi::LAYER_ABOVE_TEXT);

    let pixels = engine.image_pixels(7).expect("pixels");
    assert_eq!((pixels.width, pixels.height), (16, 32));
    assert_eq!(pixels.format, ffi::FORMAT_RGBA);
    assert_eq!(pixels.data, red(16, 32));
    assert_eq!(engine.image_pixels(8), None);
}

#[test]
fn a_placement_keeps_its_source_rectangle_and_size() {
    let engine = sized_engine();
    // Two by one cells of a 4 × 4 image's lower right quarter.
    show(&engine, 1, (4, 4), ",x=2,y=2,w=2,h=2,c=2,r=1");
    let [p] = placements(&engine)[..] else {
        panic!("no placement");
    };
    assert_eq!((p.source_x, p.source_y), (2, 2));
    assert_eq!((p.source_width, p.source_height), (2, 2));
    assert_eq!((p.width, p.height), (16, 16));
}

#[test]
fn placements_sort_into_drawing_order_by_layer_and_z() {
    let engine = sized_engine();
    let below_bg = i32::MIN / 2 - 1;
    for (id, z) in [(1, 5), (2, 1), (3, -1), (4, below_bg), (5, -7)] {
        show(&engine, id, (1, 1), &format!(",z={z}"));
    }
    let order: Vec<(u32, u8)> = placements(&engine)
        .iter()
        .map(|p| (p.image_id, p.layer))
        .collect();
    assert_eq!(
        order,
        [
            (4, ffi::LAYER_BELOW_BG),
            (5, ffi::LAYER_BELOW_TEXT),
            (3, ffi::LAYER_BELOW_TEXT),
            (2, ffi::LAYER_ABOVE_TEXT),
            (1, ffi::LAYER_ABOVE_TEXT),
        ]
    );
}

#[test]
fn a_placement_scrolls_with_the_text() {
    let engine = sized_engine();
    engine.write(b"\x1b[3;1H");
    // Three rows tall, from the third row.
    show(&engine, 1, (8, 48), ",C=1");
    let row = |engine: &Engine| placements(engine).first().map(|p| p.row);
    assert_eq!(row(&engine), Some(2));
    engine.write(b"\x1b[5;1H\r\n");
    assert_eq!(row(&engine), Some(1));
    // Partly gone: still reported, from above the viewport.
    engine.write(b"\r\n\r\n");
    assert_eq!(row(&engine), Some(-1));
    // Wholly gone, then back with the scrollback.
    engine.write(b"\r\n\r\n\r\n");
    assert_eq!(row(&engine), None);
    engine.scroll(Scroll::By(-4));
    assert_eq!(row(&engine), Some(0));
}

#[test]
fn a_deleted_image_disappears() {
    let engine = sized_engine();
    show(&engine, 1, (1, 1), "");
    show(&engine, 2, (1, 1), "");
    assert_eq!(placements(&engine).len(), 2);
    engine.write(b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
    let ids: Vec<u32> = placements(&engine).iter().map(|p| p.image_id).collect();
    assert_eq!(ids, [2]);
    assert_eq!(engine.image_pixels(1), None);
}

#[test]
fn a_reset_clears_images_and_leaves_graphics_working() {
    let engine = sized_engine();
    show(&engine, 1, (1, 1), "");
    engine.reset();
    assert_eq!(placements(&engine), []);
    show(&engine, 2, (1, 1), "");
    assert_eq!(placements(&engine).len(), 1);
}

#[test]
fn each_screen_has_its_own_images() {
    let engine = sized_engine();
    show(&engine, 1, (1, 1), "");
    engine.write(b"\x1b[?1049h");
    assert_eq!(placements(&engine), []);
    show(&engine, 2, (1, 1), "");
    assert_eq!(placements(&engine)[0].image_id, 2);
    engine.write(b"\x1b[?1049l");
    assert_eq!(placements(&engine)[0].image_id, 1);
}

#[test]
fn placeholder_cells_give_a_placement_per_row() {
    let engine = sized_engine();
    // A virtual placement of image 3, then its two cells on the second
    // row: the foreground names the image, the marks its row and column.
    let control = "a=T,U=1,f=32,i=3,s=2,v=1,c=2,r=1,q=2";
    assert_eq!(engine.write(&kitty(control, &red(2, 1))), []);
    engine.write("\r\n\x1b[38;5;3m\u{10eeee}\u{305}\u{305}".as_bytes());
    engine.write("\u{10eeee}\u{305}\u{30d}\x1b[39m".as_bytes());
    let [p] = placements(&engine)[..] else {
        panic!("{:?}", placements(&engine));
    };
    assert_eq!((p.image_id, p.col, p.row), (3, 0, 1));
    assert_eq!(p.layer, ffi::LAYER_BELOW_TEXT);
}

#[test]
fn nothing_is_placed_before_the_cell_size_is_known() {
    let engine = engine();
    show(&engine, 1, (8, 16), "");
    assert_eq!(placements(&engine), []);
    assert!(engine.resize(20, 5, 8, 16));
    assert_eq!(placements(&engine).len(), 1);
}

#[test]
fn a_program_can_ask_for_graphics_support() {
    let engine = engine();
    let replies = engine.write(&kitty("a=q,f=24,i=31,s=1,v=1", &[0, 0, 0]));
    let reply = b"\x1b_Gi=31;OK\x1b\\".to_vec();
    assert_eq!(replies, [Effect::Pty(reply)]);
}

#[test]
fn a_remote_program_cannot_read_local_files() {
    let engine = sized_engine();
    let file = std::env::temp_dir().join("wayterm-kitty-medium-test.rgba");
    std::fs::write(&file, red(1, 1)).expect("write");
    let path = file.to_str().expect("utf-8 path").as_bytes();
    for medium in ["f", "t", "s"] {
        let control = format!("a=T,t={medium},f=32,i=9,s=1,v=1");
        let replies = engine.write(&kitty(&control, path));
        let refused = matches!(&replies[..], [Effect::Pty(reply)]
            if !reply.windows(2).any(|w| w == b"OK"));
        assert!(refused, "medium {medium}: {replies:?}");
        assert_eq!(engine.image_pixels(9), None, "medium {medium}");
    }
    assert!(file.exists(), "the temp-file medium deletes what it reads");
    std::fs::remove_file(file).expect("remove");
}

/// A PNG of `pixels`, RGBA rows of `width`.
fn png(width: usize, pixels: &[u8]) -> Vec<u8> {
    let height = pixels.len() / 4 / width;
    gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from(pixels),
        width * 4,
    )
    .save_to_png_bytes()
    .to_vec()
}

#[test]
fn png_payloads_decode_to_rgba() {
    let engine = sized_engine();
    // Translucent pixels stay straight alpha.
    let pixels = [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 9, 8, 7, 255];
    let control = "a=T,f=100,i=4,q=2";
    assert_eq!(engine.write(&kitty(control, &png(2, &pixels))), []);
    assert_eq!(placements(&engine).len(), 1);
    let stored = engine.image_pixels(4).expect("pixels");
    assert_eq!((stored.width, stored.height), (2, 2));
    assert_eq!(stored.format, ffi::FORMAT_RGBA);
    assert_eq!(stored.data[..4], pixels[..4]);
    assert_eq!(stored.data[7], 128);
    assert_eq!(stored.data[11], 0);
    assert_eq!(stored.data[12..], pixels[12..]);
}

/// A reply naming an error, and no image `id`.
fn assert_refused(engine: &Engine, id: u32, payload: &[u8]) {
    let replies = engine.write(&kitty(&format!("a=T,f=100,i={id}"), payload));
    let refused = matches!(&replies[..], [Effect::Pty(reply)]
        if !reply.windows(2).any(|w| w == b"OK"));
    assert!(refused, "{replies:?}");
    assert_eq!(engine.image_pixels(id), None);
}

#[test]
fn a_png_too_big_to_store_is_refused_before_decoding() {
    let sized = |width: u32, height: u32| {
        let mut png = png(1, &red(1, 1));
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        png
    };
    assert_eq!(png_size(&sized(30_000, 7)), Some((30_000, 7)));
    // Exactly the limit, at four bytes a pixel, then one row more.
    assert!(decodable(&sized(8_000, 10_000)));
    assert!(!decodable(&sized(8_000, 10_001)));
    // Past what 32 bits hold, which must not wrap around.
    assert!(!decodable(&sized(u32::MAX, u32::MAX)));
    assert_refused(&sized_engine(), 5, &sized(8_000, 10_001));
}

#[test]
fn only_png_is_decoded() {
    let engine = sized_engine();
    let jpeg = b"\xff\xd8\xff\xe0 not a PNG, long enough to parse";
    assert!(!decodable(jpeg));
    assert_refused(&engine, 6, jpeg);
    assert_refused(&engine, 6, &png(1, &red(1, 1))[..30]);
    assert_refused(&engine, 6, b"");
}

#[test]
fn animations_advance_by_tick_and_stop() {
    let engine = sized_engine();
    // Red, then a blue frame for 40 ms; the first shows for 30 ms; play
    // on a loop.
    engine.write(&kitty("a=T,f=32,i=2,s=1,v=1,q=2", &[255, 0, 0, 255]));
    engine.write(&kitty("a=f,f=32,i=2,s=1,v=1,z=40,q=2", &[0, 0, 255, 255]));
    engine.write(b"\x1b_Ga=a,i=2,r=1,z=30,q=2\x1b\\");
    assert_eq!(engine.tick_animations(0), None, "not playing yet");
    engine.write(b"\x1b_Ga=a,i=2,s=3,q=2\x1b\\");

    let due = engine.tick_animations(0).expect("first tick");
    let first = placements(&engine)[0].generation;
    assert_eq!(engine.image_pixels(2).unwrap().data, [255, 0, 0, 255]);
    let later = engine.tick_animations(due).expect("second tick");
    assert!(later > due);
    let second = placements(&engine)[0].generation;
    assert_ne!(second, first, "a new frame is a new generation");
    assert_eq!(engine.image_pixels(2).unwrap().data, [0, 0, 255, 255]);

    engine.write(b"\x1b_Ga=a,i=2,s=1,q=2\x1b\\");
    assert_eq!(engine.tick_animations(later), None, "stopped");
}
