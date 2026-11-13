use super::super::effects::{clipboard_effect, CLIPBOARD_MAX_BYTES};
use super::*;

fn clipboard(destination: ClipboardDestination, text: &str) -> Effect {
    Effect::ClipboardWrite {
        destination,
        text: text.into(),
    }
}

/// What the terminal answered, all replies in order.
fn replies(effects: &[Effect]) -> Vec<u8> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Pty(reply) => Some(reply.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

#[test]
fn osc_52_writes_carry_their_destination_and_decoded_text() {
    use ClipboardDestination::{Selection, System};
    let engine = engine();
    for (sequence, destination) in [
        (&b"\x1b]52;c;aGVsbG8=\x07"[..], System),
        (b"\x1b]52;c;aGVsbG8=\x1b\\", System),
        (b"\x1b]52;;aGVsbG8=\x07", System),
        (b"\x1b]52;p;aGVsbG8=\x07", Selection),
        (b"\x1b]52;s;aGVsbG8=\x07", Selection),
    ] {
        assert_eq!(
            engine.write(sequence),
            vec![clipboard(destination, "hello")],
            "{}",
            sequence.escape_ascii()
        );
    }
}

#[test]
fn an_osc_52_write_may_span_reads_and_lack_its_padding() {
    let engine = engine();
    assert!(engine.write(b"\x1b]52;c;aGVs").is_empty());
    assert_eq!(
        engine.write(b"bG8\x07"),
        vec![clipboard(ClipboardDestination::System, "hello")]
    );
}

#[test]
fn utf_8_survives_an_osc_52_write() {
    // "grüße ✓"
    assert_eq!(
        engine().write(b"\x1b]52;c;Z3LDvMOfZSDinJM=\x07"),
        vec![clipboard(ClipboardDestination::System, "grüße ✓")]
    );
}

#[test]
fn malformed_base64_writes_nothing_at_all() {
    let engine = engine();
    for payload in ["***", "SGVs!!!bG8=", "aGVs bG8="] {
        let sequence = format!("\x1b]52;c;{payload}\x07");
        assert!(engine.write(sequence.as_bytes()).is_empty(), "{payload:?}");
    }
}

#[test]
fn a_control_byte_inside_the_sequence_never_reaches_the_decoder() {
    assert_eq!(
        engine().write(b"\x1b]52;c;aGVs\nbG8=\x07"),
        vec![clipboard(ClipboardDestination::System, "hello")]
    );
}

#[test]
fn a_write_that_is_not_text_writes_nothing_at_all() {
    let engine = engine();
    // A lone 0xff, then "a\0b", which GDK would cut at the NUL.
    assert!(engine.write(b"\x1b]52;c;/w==\x07").is_empty());
    assert!(engine.write(b"\x1b]52;c;YQBi\x07").is_empty());
}

#[test]
fn the_size_guard_takes_the_limit_and_refuses_past_it() {
    let system = ClipboardDestination::System;
    let at_limit = vec![b'a'; CLIPBOARD_MAX_BYTES];
    assert!(clipboard_effect(system, &at_limit).is_some());
    let over = vec![b'a'; CLIPBOARD_MAX_BYTES + 1];
    assert_eq!(clipboard_effect(system, &over), None);
}

#[test]
fn a_write_that_is_not_text_is_logged_without_its_contents() {
    let (buffer, guard) = crate::logging::tests::capture(None);
    assert_eq!(
        clipboard_effect(ClipboardDestination::System, b"hunter2\0tail"),
        None
    );
    drop(guard);
    let output = buffer.contents();
    let message = "ignored a clipboard write that is not text";
    let line = output
        .lines()
        .find(|line| line.contains(message))
        .unwrap_or_else(|| panic!("no '{message}' line in: {output}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(!output.contains("hunter2"), "{output}");
}

#[test]
fn a_large_write_arrives_whole_and_one_past_the_parser_drops() {
    // "QUJD" is "ABC".
    let sequence = |quads: usize| {
        let mut bytes = b"\x1b]52;c;".to_vec();
        bytes.extend(b"QUJD".repeat(quads));
        bytes.push(0x07);
        bytes
    };
    let engine = engine();
    let quads = 5 * 1024 * 1024 / 3;
    // Compared here, not by assert_eq!, which would print all of it.
    let whole = clipboard(ClipboardDestination::System, &"ABC".repeat(quads));
    assert!(engine.write(&sequence(quads)) == [whole]);
    assert!(engine.write(&sequence(2 * 1024 * 1024 + 1)).is_empty());
    // The parser recovers: the next write goes through.
    assert_eq!(
        engine.write(b"\x1b]52;c;aGVsbG8=\x07"),
        vec![clipboard(ClipboardDestination::System, "hello")]
    );
}

#[test]
fn a_clipboard_query_is_never_answered() {
    let engine = engine();
    for sequence in [
        &b"\x1b]52;c;?\x07"[..],
        b"\x1b]52;p;?\x07",
        b"\x1b]52;s;?\x1b\\",
    ] {
        assert!(engine.write(sequence).is_empty());
    }
}

#[test]
fn an_osc_52_clear_changes_nothing() {
    assert!(engine().write(b"\x1b]52;c;\x07").is_empty());
}

#[test]
fn osc_1337_copy_shares_the_write_path() {
    assert_eq!(
        engine().write(b"\x1b]1337;Copy=:aVRlcm0y\x1b\\"),
        vec![clipboard(ClipboardDestination::System, "iTerm2")]
    );
}

/// `text/plain` as OSC 5522 spells a MIME type, then a transaction of
/// the given data packets and its commit.
fn kitty_write(engine: &Engine, packets: &[&str]) -> Vec<Effect> {
    let mut effects = engine.write(b"\x1b]5522;type=write\x1b\\");
    for packet in packets {
        let packet = format!("\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;{packet}\x1b\\");
        effects.extend(engine.write(packet.as_bytes()));
    }
    effects.extend(engine.write(b"\x1b]5522;type=wdata\x1b\\"));
    effects
}

#[test]
fn kitty_clipboard_writes_stay_refused() {
    let engine = engine();
    for _ in 0..2 {
        let effects = kitty_write(&engine, &["R2hvc3R0eQ=="]);
        assert_eq!(
            String::from_utf8_lossy(&replies(&effects)),
            "\x1b]5522;type=write:status=EFBIG\x1b\\"
        );
        assert!(effects.iter().all(|e| matches!(e, Effect::Pty(_))));
        // A reset does not lift it.
        engine.reset();
    }
}

#[test]
fn kitty_clipboard_writes_without_data_stay_refused() {
    let engine = engine();
    for packets in [&[][..], &[""]] {
        let effects = kitty_write(&engine, packets);
        assert_eq!(
            String::from_utf8_lossy(&replies(&effects)),
            "\x1b]5522;type=write:status=ENOSYS\x1b\\"
        );
        assert!(effects.iter().all(|e| matches!(e, Effect::Pty(_))));
    }
}
