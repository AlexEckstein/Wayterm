use std::io::Write;
use std::process::{Command, Stdio};

use super::*;

const STAT: &str = "cpu  100 20 30 800 50 5 5 0 10 0\n\
cpu0 50 10 15 400 25 2 3 0 5 0\n\
cpu1 50 10 15 400 25 3 2 0 5 0\n\
intr 12345 0 0 0\n\
ctxt 999\n";

const MEMINFO: &str = "MemTotal:        8000000 kB\n\
MemFree:         1000000 kB\n\
MemAvailable:    5000000 kB\n\
Buffers:          200000 kB\n\
Cached:          2000000 kB\n\
SwapCached:            0 kB\n\
SwapTotal:       2000000 kB\n\
SwapFree:        1500000 kB\n";

const LOADAVG: &str = "0.52 0.48 0.40 1/123 4567\n";

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
fn stat_gives_times_and_cores() {
    let (times, cores) = parse_stat(STAT).unwrap();
    assert_eq!(times.total, 1010);
    assert_eq!(cores, 2);
    assert_eq!(parse_stat("intr 1\n"), None);
}

#[test]
fn meminfo_falls_back_without_mem_available() {
    assert_eq!(parse_meminfo(MEMINFO), Some(memory(5_000_000)));
    let old_kernel: String = MEMINFO
        .lines()
        .filter(|line| !line.starts_with("MemAvailable"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(parse_meminfo(&old_kernel), Some(memory(3_200_000)));
    assert_eq!(memory(5_000_000).used(), 3_000_000 * 1024);
}

#[test]
fn samples_parse_only_when_marked_and_whole() {
    let line = "wayterm-load:2 0.52 0.48 0.40 8000000 5000000 2000000 1500000 100 20 30 800 50 5 5 0 10 0\n";
    let sample = parse_sample(line).unwrap();
    assert_eq!(sample.cores, 2);
    assert_eq!(sample.load_average, ["0.52", "0.48", "0.40"]);
    assert_eq!(sample.memory, memory(5_000_000));
    assert_eq!(sample.cpu.total, 1010);
    assert_eq!(parse_sample(line.trim_start_matches("wayterm-load:")), None);
    assert_eq!(
        parse_sample("wayterm-load:2 0.52 0.48 0.40 8000000 5000000 2000000 1500000"),
        None
    );
    assert_eq!(
        parse_sample("wayterm-load:2 0.52 0.48 0.40  5000000 2000000 1500000 1 2 3 4"),
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

/// Runs one pass of the script under this machine's `sh`, its `/proc`
/// paths replaced by `files`.
fn run_script(files: &[(&str, &str)]) -> Option<LoadSample> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut script = sample_script()
        .replace("while :; do", "for _ in 1; do")
        .replace(&format!("sleep {}", INTERVAL.as_secs()), "true");
    for (path, contents) in files {
        let stand_in = dir.path().join(path.trim_start_matches("/proc/"));
        std::fs::write(&stand_in, contents).unwrap();
        script = script.replace(path, stand_in.to_str().unwrap());
    }
    let mut sh = Command::new("sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sh");
    sh.stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = sh.wait_with_output().expect("sh exits");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(parse_sample)
}

#[test]
fn the_script_reads_the_proc_files() {
    let sample = run_script(&[
        ("/proc/stat", STAT),
        ("/proc/meminfo", MEMINFO),
        ("/proc/loadavg", LOADAVG),
    ])
    .expect("a sample");
    assert_eq!(
        sample,
        LoadSample {
            cores: 2,
            load_average: ["0.52", "0.48", "0.40"].map(str::to_string),
            cpu: cpu_times("100 20 30 800 50 5 5 0 10 0").unwrap(),
            memory: memory(5_000_000),
        }
    );
    let old_kernel = MEMINFO.replace("MemAvailable:    5000000 kB\n", "");
    let sample = run_script(&[
        ("/proc/stat", STAT),
        ("/proc/meminfo", &old_kernel),
        ("/proc/loadavg", LOADAVG),
    ])
    .expect("a sample");
    assert_eq!(sample.memory, memory(3_200_000));
}

/// On this (Linux) machine the script and the local reader agree.
#[test]
fn the_script_samples_this_machine() {
    let Some(expected) = local_sample() else {
        return;
    };
    let sample = run_script(&[]).expect("a sample");
    assert_eq!(sample.cores, expected.cores);
    assert_eq!(sample.memory.total, expected.memory.total);
    assert!(sample.cpu.total > 0);
}

#[test]
fn the_script_sleeps_for_the_interval() {
    assert!(sample_script().contains(&format!("sleep {}\n", INTERVAL.as_secs())));
}
