//! OSC 9 and OSC 777: the engine's notification effects.

use super::*;

fn notification(title: &str, body: &str) -> Effect {
    Effect::Notification {
        title: title.into(),
        body: body.into(),
    }
}

#[test]
fn osc_9_raises_a_notification_without_a_title() {
    let engine = engine();
    assert_eq!(
        engine.write(b"\x1b]9;build finished\x1b\\"),
        vec![notification("", "build finished")]
    );
    assert_eq!(
        engine.write(b"\x1b]9;bel terminated\x07"),
        vec![notification("", "bel terminated")]
    );
}

#[test]
fn osc_777_raises_a_notification_with_its_title_and_body() {
    let engine = engine();
    assert_eq!(
        engine.write(b"\x1b]777;notify;CI;tests; all green\x1b\\"),
        vec![notification("CI", "tests; all green")]
    );
    assert_eq!(
        engine.write("\x1b]777;notify;caf\u{e9};\u{2713}\x07".as_bytes()),
        vec![notification("caf\u{e9}", "\u{2713}")]
    );
}

/// Effects stay in the order the bytes raised them, a notification
/// included, whichever callback delivered it.
#[test]
fn a_notification_keeps_its_place_among_effects() {
    let effects = engine().write(b"\x07\x1b]9;done\x07\x07");
    assert_eq!(
        effects,
        vec![Effect::Bell, notification("", "done"), Effect::Bell]
    );
}

/// Progress reports (OSC 9;4) and other extensions are not messages,
/// and neither is an OSC 777 that is not `notify`.
#[test]
fn other_osc_9_and_777_forms_raise_no_notification() {
    let engine = engine();
    for sequence in [
        &b"\x1b]9;4;1;50\x07"[..],
        b"\x1b]9;4;0\x07",
        b"\x1b]777;preexec;ls\x07",
        b"\x1b]777;notify;no body\x07",
    ] {
        assert_eq!(engine.write(sequence), vec![], "{sequence:?}");
    }
}

/// Remote bytes are not guaranteed text: nothing here may reach the
/// UI as anything but valid UTF-8, and a NUL must not survive.
#[test]
fn a_notification_with_hostile_bytes_stays_text() {
    let engine = engine();
    let effects = engine.write(b"\x1b]777;notify;a\x00b;\xff\xfe\x1b\\");
    let [Effect::Notification { title, body }] = effects.as_slice() else {
        panic!("one notification: {effects:?}");
    };
    assert!(!title.contains('\0') && !body.contains('\0'));
    assert!(body.is_empty() || body.contains('\u{fffd}'), "{body:?}");
}
