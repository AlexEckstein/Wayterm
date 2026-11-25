//! Formatting shared across the UI: byte counts and local clock times.

use std::time::Duration;

use gtk4::glib;

/// Human-readable byte count, e.g. `1023 B`, `1.2 KB`, `4.0 MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The local time `ago` before now in a `strftime` `format`, e.g. "%X"
/// for "14:02:11"; empty when the local time is unknown.
pub fn local_clock(ago: Duration, format: &str) -> String {
    glib::DateTime::now_local()
        .and_then(|now| now.add_seconds(-ago.as_secs_f64()))
        .and_then(|then| then.format(format))
        .map(String::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
