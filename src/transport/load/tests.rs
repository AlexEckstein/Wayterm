use super::*;

fn memory(available_kib: u64) -> Memory {
    Memory {
        total: 8_000_000 * 1024,
        available: available_kib * 1024,
        swap_total: 2_000_000 * 1024,
        swap_free: 1_500_000 * 1024,
    }
}

#[test]
fn cpu_times_count_idle_and_iowait_as_idle() {
    assert_eq!(
        cpu_times("100 20 30 800 50 5 5 0 10 0"),
        Some(CpuTimes {
            busy: 160,
            total: 1010,
        }),
        "guest fields are already in user and nice"
    );
    assert_eq!(
        cpu_times("10 0 10 80"),
        Some(CpuTimes {
            busy: 20,
            total: 100
        })
    );
    assert_eq!(cpu_times("10 0 10"), None);
    assert_eq!(cpu_times("10 x 10 80"), None);
}

#[test]
fn cpu_lines_parse_only_when_marked_and_whole() {
    let line = "wayterm-cpu:2 0.52 0.48 0.40 100 20 30 800 50 5 5 0 10 0\n";
    let sample = parse_cpu(line).unwrap();
    assert_eq!(sample.cores, 2);
    assert_eq!(sample.load_average, ["0.52", "0.48", "0.40"]);
    assert_eq!(sample.times.total, 1010);
    assert_eq!(parse_cpu(line.trim_start_matches("wayterm-cpu:")), None);
    assert_eq!(parse_cpu("wayterm-cpu:2 0.52 0.48 0.40"), None);
    assert_eq!(parse_cpu("wayterm-cpu:2 0.52 x 0.40 1 2 3 4"), None);
}

#[test]
fn memory_lines_parse_only_when_marked_and_whole() {
    let line = "wayterm-mem:8000000 5000000 2000000 1500000\n";
    assert_eq!(parse_memory(line), Some(memory(5_000_000)));
    assert_eq!(parse_memory(line.trim_start_matches("wayterm-mem:")), None);
    assert_eq!(parse_memory("wayterm-mem:8000000 5000000 2000000"), None);
    assert_eq!(
        parse_memory("wayterm-mem:8000000  2000000 1500000"),
        None,
        "an empty field"
    );
}

#[test]
fn the_meter_needs_two_samples_and_skips_resets() {
    let mut meter = CpuMeter::default();
    assert_eq!(meter.add(CpuTimes { busy: 0, total: 0 }), None);
    assert_eq!(
        meter.add(CpuTimes {
            busy: 25,
            total: 200
        }),
        Some(13),
        "rounded"
    );
    assert_eq!(meter.add(CpuTimes { busy: 5, total: 10 }), None);
    assert_eq!(
        meter.add(CpuTimes { busy: 5, total: 10 }),
        None,
        "no time passed"
    );
    assert_eq!(
        meter.add(CpuTimes {
            busy: 105,
            total: 110
        }),
        Some(100)
    );
}
