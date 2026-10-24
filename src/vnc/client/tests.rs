use super::*;
use ::vnc::{DesktopLayout, DesktopReason, DesktopStatus, DesktopUpdate, Rect, Screen};

fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[test]
fn set_resolution_resizes_and_reports() {
    let mut px = VncPixels::new(2, 2);
    let applied = apply_event(&mut px, VncEvent::SetResolution(Screen::from((4, 3))));
    assert_eq!(applied, Applied::Ui(VncUiEvent::Resolved(4, 3)));
    assert_eq!(px.dims(), (4, 3));
}

#[test]
fn desktop_update_with_layout_resizes_without_one_is_ignored() {
    let mut px = VncPixels::new(1, 1);
    let update = |layout| DesktopUpdate {
        reason: DesktopReason::Server,
        status: DesktopStatus::Success,
        layout,
    };
    let layout = DesktopLayout {
        width: 5,
        height: 4,
        screens: vec![],
    };
    assert_eq!(
        apply_event(&mut px, VncEvent::DesktopUpdate(update(Some(layout)))),
        Applied::Ui(VncUiEvent::Resolved(5, 4))
    );
    assert_eq!(px.dims(), (5, 4));
    assert_eq!(
        apply_event(&mut px, VncEvent::DesktopUpdate(update(None))),
        Applied::Ignored
    );
    assert_eq!(px.dims(), (5, 4));
}

#[test]
fn an_oversized_desktop_is_fatal_and_allocates_nothing() {
    let mut px = VncPixels::new(2, 2);
    let huge = Screen::from((u16::MAX, u16::MAX));
    assert!(matches!(
        apply_event(&mut px, VncEvent::SetResolution(huge)),
        Applied::Fatal(_)
    ));
    let layout = DesktopLayout {
        width: 16_385,
        height: 2,
        screens: vec![],
    };
    let update = DesktopUpdate {
        reason: DesktopReason::Server,
        status: DesktopStatus::Success,
        layout: Some(layout),
    };
    assert!(matches!(
        apply_event(&mut px, VncEvent::DesktopUpdate(update)),
        Applied::Fatal(_)
    ));
    assert_eq!(px, VncPixels::new(2, 2));
}

#[test]
fn raw_image_lands_as_bgra_pixels() {
    let mut px = VncPixels::new(2, 1);
    let applied = apply_event(
        &mut px,
        VncEvent::RawImage(rect(1, 0, 1, 1), vec![0x11, 0x22, 0x33, 0x00]),
    );
    assert_eq!(applied, Applied::Dirty);
    assert_eq!(px.pixels()[1], 0xFF33_2211);
}

#[test]
fn raw_image_with_wrong_length_is_fatal_and_writes_nothing() {
    let mut px = VncPixels::new(2, 1);
    let before = px.clone();
    let applied = apply_event(&mut px, VncEvent::RawImage(rect(0, 0, 2, 1), vec![0; 3]));
    assert!(matches!(applied, Applied::Fatal(_)));
    assert_eq!(px, before);
}

#[test]
fn copy_moves_pixels() {
    let mut px = VncPixels::new(3, 1);
    apply_event(
        &mut px,
        VncEvent::RawImage(rect(0, 0, 1, 1), vec![9, 8, 7, 0]),
    );
    let applied = apply_event(&mut px, VncEvent::Copy(rect(2, 0, 1, 1), rect(0, 0, 1, 1)));
    assert_eq!(applied, Applied::Dirty);
    assert_eq!(px.pixels()[2], px.pixels()[0]);
}

#[test]
fn bell_text_error_and_ignored_variants() {
    let mut px = VncPixels::new(1, 1);
    assert_eq!(
        apply_event(&mut px, VncEvent::Bell),
        Applied::Ui(VncUiEvent::Bell)
    );
    assert_eq!(
        apply_event(&mut px, VncEvent::Text("hi".into())),
        Applied::Ui(VncUiEvent::Clipboard("hi".into()))
    );
    assert_eq!(
        apply_event(&mut px, VncEvent::Error("boom".into())),
        Applied::Fatal("boom".into())
    );
    for ignored in [
        VncEvent::SetCursor(rect(0, 0, 1, 1), vec![0; 4]),
        VncEvent::JpegImage(rect(0, 0, 1, 1), vec![0; 4]),
    ] {
        assert_eq!(apply_event(&mut px, ignored), Applied::Ignored);
    }
    assert_eq!(px, VncPixels::new(1, 1), "ignored events never paint");
}

#[test]
fn full_input_channel_drops_and_counts() {
    let (input, mut rx) = VncInput::new(2);
    assert!(input.send(X11Event::Refresh));
    assert!(input.send(X11Event::Refresh));
    assert!(!input.send(X11Event::FullRefresh));
    assert!(!input.send(X11Event::FullRefresh));
    assert_eq!(input.dropped(), 2);
    rx.try_recv().unwrap();
    assert!(input.send(X11Event::Refresh), "space freed, queues again");
    assert_eq!(input.dropped(), 2);
}

#[test]
fn connect_errors_map_to_domain_errors_without_echoing_secrets() {
    use ::vnc::VncError as E;
    assert!(matches!(
        map_connect_error(E::NoPassword, false),
        VncError::AuthRequired
    ));
    assert!(matches!(
        map_connect_error(E::WrongPassword, true),
        VncError::AuthFailed(_)
    ));
    assert!(matches!(
        map_connect_error(E::General("Authentication failed".into()), true),
        VncError::AuthFailed(m) if m == "Authentication failed"
    ));
    assert!(matches!(
        map_connect_error(E::General("too many".into()), false),
        VncError::Handshake(_)
    ));
    assert!(matches!(
        map_connect_error(E::ConnectError, true),
        VncError::Handshake(_)
    ));
    assert!(matches!(
        map_connect_error(E::IoError(std::io::ErrorKind::UnexpectedEof.into()), true),
        VncError::Io(_)
    ));
}

#[test]
fn default_encodings_include_raw_and_the_pseudo_encodings_the_decoder_requires() {
    for needed in [
        VncEncoding::Raw,
        VncEncoding::DesktopSizePseudo,
        VncEncoding::CursorPseudo,
        VncEncoding::CopyRect,
    ] {
        assert!(DEFAULT_ENCODINGS.contains(&needed), "{needed:?}");
    }
}
