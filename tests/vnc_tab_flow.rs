//! Display-free state tests for the VNC tab (doc 08 §VNC): the pure mapping
//! functions the GTK handlers call. No GTK widgets are built here.

use wayterm::store::model::{SessionConfig, SessionType, VncOptions};
use wayterm::transport::types::TransportState;
use wayterm::ui::sftp_sidebar::supports_sftp;
use wayterm::ui::vnc_tab::{
    apply_button, blit_rows, button_bit, effective_options, placement, state_for, to_framebuffer,
    wheel_bit, Placement,
};
use wayterm::vnc::VncUiEvent;

#[test]
fn scaled_placement_fits_and_centers() {
    // 400x200 widget, 100x100 desktop: scale 2, centered horizontally.
    let p = placement((400, 200), (100, 100), true);
    assert_eq!(
        p,
        Placement {
            scale: 2.0,
            off_x: 100.0,
            off_y: 0.0
        }
    );
    // Upscales and downscales alike.
    assert_eq!(placement((50, 50), (100, 100), true).scale, 0.5);
}

#[test]
fn unscaled_placement_is_one_to_one_centered() {
    let p = placement((400, 300), (100, 100), false);
    assert_eq!((p.scale, p.off_x, p.off_y), (1.0, 150.0, 100.0));
}

#[test]
fn coordinates_map_through_the_placement() {
    let fb = (100, 100);
    let scaled = placement((400, 200), fb, true);
    assert_eq!(to_framebuffer(&scaled, fb, 100.0, 0.0), (0, 0));
    assert_eq!(to_framebuffer(&scaled, fb, 300.0, 100.0), (100 - 1, 50));
    assert_eq!(to_framebuffer(&scaled, fb, 200.0, 100.0), (50, 50));

    let flat = placement((400, 300), fb, false);
    assert_eq!(to_framebuffer(&flat, fb, 160.0, 110.0), (10, 10));
}

#[test]
fn coordinates_clamp_outside_the_picture_and_survive_degenerate_input() {
    let fb = (100, 100);
    let p = placement((400, 200), fb, true);
    assert_eq!(to_framebuffer(&p, fb, -50.0, -50.0), (0, 0));
    assert_eq!(to_framebuffer(&p, fb, 9999.0, 9999.0), (99, 99));
    assert_eq!(to_framebuffer(&p, fb, f64::NAN, f64::INFINITY), (0, 99));
    // No desktop yet / zero-size widget: no panic, origin.
    assert_eq!(
        to_framebuffer(&placement((0, 0), (0, 0), true), (0, 0), 5.0, 5.0),
        (0, 0)
    );
    let zero = placement((0, 0), fb, true);
    assert_eq!(to_framebuffer(&zero, fb, 5.0, 5.0), (0, 0));
}

#[test]
fn buttons_and_wheel_map_to_rfb_bits() {
    assert_eq!(
        [1, 2, 3, 8, 0].map(button_bit),
        [1, 2, 4, 0, 0],
        "left, middle, right; unknown buttons are ignored"
    );
    assert_eq!(wheel_bit(-1.0), Some(8));
    assert_eq!(wheel_bit(2.5), Some(16));
    assert_eq!(wheel_bit(0.0), None);

    let held = apply_button(0, 1, true);
    let both = apply_button(held, 4, true);
    assert_eq!(both, 5);
    assert_eq!(apply_button(both, 1, false), 4);
    assert_eq!(
        apply_button(4, 0, true),
        4,
        "unknown button changes nothing"
    );
}

#[test]
fn options_default_to_scaled_and_interactive() {
    let mut session = SessionConfig::default();
    assert_eq!(effective_options(&session), (true, false));
    session.vnc_options = Some(VncOptions {
        quality: 5,
        scaling: false,
        view_only: true,
    });
    assert_eq!(effective_options(&session), (false, true));
}

#[test]
fn engine_events_map_to_page_state() {
    assert_eq!(
        state_for(&VncUiEvent::Ended("cancelled".into())),
        Some(TransportState::Disconnected)
    );
    assert_eq!(
        state_for(&VncUiEvent::Ended("server closed".into())),
        Some(TransportState::Error("server closed".into()))
    );
    for quiet in [
        VncUiEvent::Dirtied,
        VncUiEvent::Resolved(1, 1),
        VncUiEvent::Bell,
        VncUiEvent::Clipboard("x".into()),
    ] {
        assert_eq!(state_for(&quiet), None, "{quiet:?}");
    }
}

#[test]
fn blit_maps_bgra_words_to_cairo_rgb24_exactly() {
    // 2x2 desktop, stride padded to 12 bytes per row.
    let src = [0xFF11_2233, 0xFF44_5566, 0xFF77_8899, 0xFFAA_BBCC];
    let mut dst = [0xEEu8; 24];
    blit_rows(&src, 2, &mut dst, 12);
    for (i, word) in src.iter().enumerate() {
        let at = (i / 2) * 12 + (i % 2) * 4;
        assert_eq!(dst[at..at + 4], word.to_ne_bytes());
        // Read back the way cairo does: one native word, r in bits 16..24.
        let back = u32::from_ne_bytes(dst[at..at + 4].try_into().unwrap());
        assert_eq!(back & 0x00FF_FFFF, word & 0x00FF_FFFF);
    }
    assert_eq!(dst[8..12], [0xEE; 4], "row padding is untouched");
}

#[test]
fn blit_tolerates_short_buffers() {
    let mut dst = [0u8; 6];
    blit_rows(&[1, 2, 3, 4], 2, &mut dst, 8);
    assert_eq!(dst[..4], 1u32.to_ne_bytes());
    blit_rows(&[1], 0, &mut dst, 8);
    blit_rows(&[1], 1, &mut dst, 0);
}

#[test]
fn vnc_tabs_get_no_sftp_sidebar() {
    assert!(!supports_sftp(&SessionType::VNC));
    assert!(supports_sftp(&SessionType::SSH));
}
