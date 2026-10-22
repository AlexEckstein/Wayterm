//! Recording (doc 12 §Recording): timestamped input chunks in, steps out.
//! Pure — the caller supplies the timestamps — so the pause arithmetic is
//! tested without a clock.

use std::time::Instant;

use crate::macros::model::MacroStep;

/// Turns the input a terminal sends into [`MacroStep`]s, each holding the
/// pause since the previous chunk.
#[derive(Debug, Default)]
pub struct Recorder {
    /// Steps so far.
    steps: Vec<MacroStep>,
    /// When the previous chunk arrived.
    last: Option<Instant>,
}

impl Recorder {
    /// An empty recording.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `data`, sent at `at`. The first chunk gets no pause — the
    /// time before it is the user reaching the terminal — and a chunk in
    /// the same millisecond as the previous one joins its step. Mouse and
    /// focus reports are skipped: they are not keystrokes.
    pub fn record(&mut self, data: &[u8], at: Instant) {
        if data.is_empty() || is_pointer_or_focus_report(data) {
            return;
        }
        let delay_ms = self.last.map_or(0, |last| {
            let ms = at.saturating_duration_since(last).as_millis();
            u32::try_from(ms).unwrap_or(u32::MAX)
        });
        self.last = Some(at);
        match self.steps.last_mut() {
            Some(previous) if delay_ms == 0 => previous.keys.extend_from_slice(data),
            _ => self.steps.push(MacroStep {
                delay_ms,
                keys: data.to_vec(),
            }),
        }
    }

    /// The steps recorded so far.
    pub fn steps(&self) -> &[MacroStep] {
        &self.steps
    }

    /// The recorded steps.
    pub fn finish(self) -> Vec<MacroStep> {
        self.steps
    }
}

/// Whether `data` is a mouse or focus report. VTE sends those through the
/// same channel as keystrokes once an application turns them on (tmux,
/// vim, htop do), each as a chunk of its own, so a whole chunk is matched:
/// focus in/out (`CSI I`, `CSI O`), and mouse reports in the X10/normal
/// (`CSI M` + 3 bytes), SGR (`CSI < b;x;y M|m`) and urxvt (`CSI b;x;y M`)
/// encodings. No key sends any of these.
pub fn is_pointer_or_focus_report(data: &[u8]) -> bool {
    let Some(params) = data.strip_prefix(b"\x1b[") else {
        return false;
    };
    match params {
        b"I" | b"O" => true,
        [b'M', rest @ ..] => rest.len() >= 3,
        [b'<', rest @ ..] => match rest.split_last() {
            Some((b'M' | b'm', numbers)) => three_numbers(numbers),
            _ => false,
        },
        _ => match params.split_last() {
            Some((b'M', numbers)) => three_numbers(numbers),
            _ => false,
        },
    }
}

/// `b;x;y`: three non-empty runs of digits.
fn three_numbers(text: &[u8]) -> bool {
    let fields: Vec<&[u8]> = text.split(|&b| b == b';').collect();
    fields.len() == 3
        && fields
            .iter()
            .all(|field| !field.is_empty() && field.iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn step(delay_ms: u32, keys: &[u8]) -> MacroStep {
        MacroStep {
            delay_ms,
            keys: keys.to_vec(),
        }
    }

    #[test]
    fn each_keystroke_keeps_the_pause_before_it() {
        let t0 = Instant::now();
        let mut recorder = Recorder::new();
        recorder.record(b"l", at(t0, 4000));
        recorder.record(b"s", at(t0, 4180));
        recorder.record(b"\r", at(t0, 5500));
        assert_eq!(
            recorder.finish(),
            [step(0, b"l"), step(180, b"s"), step(1320, b"\r")],
            "the 4 s before the first key are not recorded"
        );
    }

    #[test]
    fn chunks_in_the_same_millisecond_share_a_step() {
        let t0 = Instant::now();
        let mut recorder = Recorder::new();
        recorder.record(b"a", t0);
        recorder.record(b"b", t0 + Duration::from_micros(900));
        recorder.record(b"c", t0 + Duration::from_micros(3100));
        assert_eq!(
            recorder.steps(),
            [step(0, b"ab"), step(2, b"c")],
            "the pause runs from the latest chunk, b"
        );
    }

    #[test]
    fn mouse_and_focus_reports_are_skipped_without_resetting_the_pause() {
        let t0 = Instant::now();
        let mut recorder = Recorder::new();
        recorder.record(b"x", t0);
        recorder.record(b"\x1b[O", at(t0, 100));
        recorder.record(b"\x1b[<0;12;5M", at(t0, 200));
        recorder.record(b"", at(t0, 250));
        recorder.record(b"y", at(t0, 300));
        assert_eq!(recorder.finish(), [step(0, b"x"), step(300, b"y")]);
    }

    #[test]
    fn a_huge_pause_saturates() {
        let t0 = Instant::now();
        let mut recorder = Recorder::new();
        recorder.record(b"a", t0);
        recorder.record(b"b", t0 + Duration::from_secs(60 * 24 * 3600));
        assert_eq!(recorder.steps()[1].delay_ms, u32::MAX);
    }

    #[test]
    fn reports_are_told_apart_from_keys() {
        for report in [
            &b"\x1b[I"[..],
            b"\x1b[O",
            b"\x1b[M !!",
            b"\x1b[M\xc3\x80!!",
            b"\x1b[<0;12;5M",
            b"\x1b[<35;80;24m",
            b"\x1b[32;12;5M",
        ] {
            assert!(is_pointer_or_focus_report(report), "{report:?}");
        }
        for key in [
            &b"\x1b[A"[..],
            b"\x1bOA",
            b"\x1b[1;5C",
            b"\x1b[3~",
            b"\x1bOM",
            b"\x1b[M",
            b"\x1b[<0;12M",
            b"\x1b[1;5M",
            b"I",
            b"\x1b",
        ] {
            assert!(!is_pointer_or_focus_report(key), "{key:?}");
        }
    }
}
