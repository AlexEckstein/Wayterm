//! Directory entries as the sidebar lists them: their order, and the
//! permission and date text, whichever task read them.

use crate::sftp::commands::DirEntry;

impl DirEntry {
    /// An entry with its date and permissions formatted; either is empty
    /// when the file system did not say.
    pub(super) fn new(
        name: String,
        size: u64,
        mtime: Option<u32>,
        mode: Option<u32>,
        is_dir: bool,
    ) -> Self {
        Self {
            name,
            size,
            modified: mtime.map(format_timestamp).unwrap_or_default(),
            permissions: mode.map(format_perms).unwrap_or_default(),
            is_dir,
        }
    }
}

/// Directories first, then case-insensitive alphabetical (doc 07).
pub(super) fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// `rwxr-xr-x` from a mode word, ignoring file-type and setuid/sticky bits.
fn format_perms(mode: u32) -> String {
    let mode = mode & 0o777;
    let mut out = String::with_capacity(9);
    for group in [mode >> 6, mode >> 3, mode] {
        for (bit, ch) in [(0o4, 'r'), (0o2, 'w'), (0o1, 'x')] {
            out.push(if group & bit != 0 { ch } else { '-' });
        }
    }
    out
}

/// `YYYY-MM-DD HH:MM` in UTC (no timezone database dependency).
fn format_timestamp(secs: u32) -> String {
    let days = i64::from(secs) / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Days since 1970-01-01 → (year, month, day): Howard Hinnant's
/// `civil_from_days`, exact for every `u32` timestamp.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        DirEntry::new(name.to_string(), 0, None, None, is_dir)
    }

    #[test]
    fn dir_entries_sort_directories_first_then_case_insensitive() {
        let mut entries = vec![
            entry("beta", false),
            entry("Alpha", false),
            entry("gamma", true),
            entry("Beta", true),
            entry("alpha", true),
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "gamma", "Alpha", "beta"]);
    }

    #[test]
    fn format_perms_masks_file_type_bits_to_rwx_style() {
        assert_eq!(format_perms(0o100644), "rw-r--r--");
        assert_eq!(format_perms(0o40755), "rwxr-xr-x");
        assert_eq!(format_perms(0o100750), "rwxr-x---");
        assert_eq!(format_perms(0o100777), "rwxrwxrwx");
        assert_eq!(format_perms(0), "---------");
        // setuid/setgid/sticky are not part of the nine-character display
        assert_eq!(format_perms(0o104755), "rwxr-xr-x");
    }

    #[test]
    fn format_timestamp_formats_utc_minutes() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00");
        assert_eq!(format_timestamp(86_399), "1970-01-01 23:59");
        assert_eq!(format_timestamp(86_400), "1970-01-02 00:00");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13");
        // Past the year 2100, where naive date math goes wrong
        assert_eq!(format_timestamp(u32::MAX), "2106-02-07 06:28");
    }

    #[test]
    fn an_entry_without_a_date_or_mode_shows_neither() {
        let bare = entry("a", false);
        assert_eq!(
            (bare.modified.as_str(), bare.permissions.as_str()),
            ("", "")
        );
        let full = DirEntry::new("a".into(), 1, Some(0), Some(0o644), false);
        assert_eq!(full.modified, "1970-01-01 00:00");
        assert_eq!(full.permissions, "rw-r--r--");
    }
}
