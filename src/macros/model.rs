//! The macro data model (doc 12). Serde spellings are the `macros.steps`
//! wire format, locked by the tests — changing one is a schema break.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One step of a macro: wait `delay_ms`, then send `keys` as if typed.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroStep {
    /// The pause before the keys, in milliseconds.
    pub delay_ms: u32,
    /// The bytes the terminal sent for the keystroke — bytes, not key
    /// names (doc 12 §Bytes, not keys, not keys). Empty is a pure pause.
    pub keys: Vec<u8>,
}

impl MacroStep {
    pub fn delay(&self) -> Duration {
        Duration::from_millis(self.delay_ms.into())
    }
}

/// A saved macro: one row of the `macros` table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Macro {
    /// The row id; 0 until stored.
    pub id: i64,
    pub name: String,
    pub steps: Vec<MacroStep>,
}

impl Macro {
    /// How long playback takes: every pause added up.
    pub fn duration(&self) -> Duration {
        total_delay(&self.steps)
    }

    /// The panel's one-line description, e.g. `12 steps · 3.4 s`.
    pub fn summary(&self) -> String {
        summary(&self.steps)
    }
}

pub fn total_delay(steps: &[MacroStep]) -> Duration {
    steps.iter().map(MacroStep::delay).sum()
}

/// `N steps · <duration>` for `steps`.
pub fn summary(steps: &[MacroStep]) -> String {
    let noun = if steps.len() == 1 { "step" } else { "steps" };
    format!(
        "{} {noun} · {}",
        steps.len(),
        format_duration(total_delay(steps))
    )
}

/// Tenths of a second under a minute, whole seconds above.
pub fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{} min {} s", secs / 60, secs % 60)
    }
}

/// The `macros.steps` column: a JSON array of steps.
pub fn steps_to_json(steps: &[MacroStep]) -> serde_json::Result<String> {
    serde_json::to_string(steps)
}

pub fn steps_from_json(json: &str) -> serde_json::Result<Vec<MacroStep>> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(delay_ms: u32, keys: &[u8]) -> MacroStep {
        MacroStep {
            delay_ms,
            keys: keys.to_vec(),
        }
    }

    /// The wire format doc 12 documents: `delay_ms` and `keys` as a byte
    /// array, so non-UTF-8 input survives the database.
    #[test]
    fn steps_wire_format_is_locked() {
        let steps = [step(0, b"l"), step(120, b"\r"), step(5, &[0xff])];
        let json = steps_to_json(&steps).unwrap();
        assert_eq!(
            json,
            r#"[{"delay_ms":0,"keys":[108]},{"delay_ms":120,"keys":[13]},{"delay_ms":5,"keys":[255]}]"#
        );
        assert_eq!(steps_from_json(&json).unwrap(), steps);
    }

    #[test]
    fn summary_counts_steps_and_adds_up_pauses() {
        let mut m = Macro {
            name: "deploy".into(),
            steps: vec![step(0, b"l"), step(1200, b"s"), step(2250, b"\r")],
            ..Macro::default()
        };
        assert_eq!(m.duration(), Duration::from_millis(3450));
        assert_eq!(m.summary(), "3 steps · 3.5 s");
        m.steps.truncate(1);
        assert_eq!(m.summary(), "1 step · 0.0 s");
    }

    #[test]
    fn durations_switch_to_minutes_at_one_minute() {
        assert_eq!(format_duration(Duration::from_millis(59_940)), "59.9 s");
        assert_eq!(format_duration(Duration::from_millis(60_000)), "1 min 0 s");
        assert_eq!(
            format_duration(Duration::from_millis(754_999)),
            "12 min 34 s"
        );
    }
}
