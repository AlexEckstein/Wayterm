use super::format_size;

#[test]
fn format_size_uses_binary_units_and_keeps_bytes_exact() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1023), "1023 B");
    assert_eq!(format_size(1024), "1.0 KB");
    assert_eq!(format_size(1536), "1.5 KB");
    assert_eq!(format_size(1024 * 1024), "1.0 MB");
    assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    // The unit table stops at PB, so the largest u64 saturates there instead
    // of inventing an unit.
    assert_eq!(format_size(u64::MAX), "16384.0 PB");
}
