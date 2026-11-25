//! The traffic on a host's default-route interface, from the host
//! script's counter lines (`host_stats`; doc 04 §Server figures).

use std::time::Instant;

/// Prefixes each sample, since login-shell startup files may print too.
const MARKER: &str = "wayterm-net:";

/// One interface's byte counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Counters {
    pub interface: String,
    pub rx: u64,
    pub tx: u64,
}

/// Bytes per second over the last interval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rate {
    pub interface: String,
    pub rx_per_sec: u64,
    pub tx_per_sec: u64,
}

/// Turns consecutive samples into a [`Rate`].
#[derive(Debug, Default)]
pub struct RateMeter {
    last: Option<(Counters, Instant)>,
}

impl RateMeter {
    /// The rate since the previous sample: `None` for the first, after the
    /// interface changed, or when a counter went backwards (the interface
    /// was reset).
    pub fn add(&mut self, counters: Counters, at: Instant) -> Option<Rate> {
        let rate = self.last.as_ref().and_then(|(last, then)| {
            let millis = at.checked_duration_since(*then)?.as_millis() as u64;
            if millis == 0 || last.interface != counters.interface {
                return None;
            }
            let per_sec = |now: u64, before: u64| Some(now.checked_sub(before)? * 1000 / millis);
            Some(Rate {
                interface: counters.interface.clone(),
                rx_per_sec: per_sec(counters.rx, last.rx)?,
                tx_per_sec: per_sec(counters.tx, last.tx)?,
            })
        });
        self.last = Some((counters, at));
        rate
    }
}

/// A sample line from the script: `wayterm-net:<interface> <rx> <tx>`.
pub fn parse_sample(line: &str) -> Option<Counters> {
    let mut fields = line.trim().strip_prefix(MARKER)?.split_whitespace();
    let counters = Counters {
        interface: fields.next()?.to_string(),
        rx: fields.next()?.parse().ok()?,
        tx: fields.next()?.parse().ok()?,
    };
    fields.next().is_none().then_some(counters)
}

#[cfg(test)]
mod tests;
