//! A host's CPU and memory use, sampled every [`INTERVAL`]: over a script
//! on the server for SSH tabs (doc 04 §Server load and disks), from this
//! machine's `/proc` for local shells (doc 06 §Local shell).

use std::sync::Weak;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::transport::connection::SshConnection;
use crate::transport::{remote_script, sampling};

pub const INTERVAL: Duration = Duration::from_secs(2);
/// Prefixes each sample, since login-shell startup files may print too.
const MARKER: &str = "wayterm-load:";

/// Prints the core count, the load averages, memory and swap (KiB) and
/// the aggregate `cpu` line's times every `{interval}` seconds until its
/// output closes. Kernels before 3.14 have no `MemAvailable`; free plus
/// buffers plus cache stands in.
const SAMPLE_SCRIPT: &str = r#"[ -r /proc/stat ] && [ -r /proc/meminfo ] && [ -r /proc/loadavg ] || exit 0
n=$(grep -c '^cpu[0-9]' /proc/stat)
while :; do
  read -r _ c < /proc/stat
  read -r l1 l5 l15 _ < /proc/loadavg
  mt= ma= mf=0 mb=0 mc=0 st=0 sf=0
  while read -r k v _; do
    case $k in
      MemTotal:) mt=$v ;;
      MemFree:) mf=$v ;;
      MemAvailable:) ma=$v ;;
      Buffers:) mb=$v ;;
      Cached:) mc=$v ;;
      SwapTotal:) st=$v ;;
      SwapFree:) sf=$v; break ;;
    esac
  done < /proc/meminfo
  [ -n "$ma" ] || ma=$((mf + mb + mc))
  printf 'wayterm-load:%s %s %s %s %s %s %s %s %s\n' \
    "$n" "$l1" "$l5" "$l15" "$mt" "$ma" "$st" "$sf" "$c" || exit 0
  sleep {interval}
done
"#;

fn sample_script() -> String {
    SAMPLE_SCRIPT.replace("{interval}", &INTERVAL.as_secs().to_string())
}

/// Time the CPUs spent, in clock ticks since boot, summed over all cores.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// Memory and swap in bytes. Used memory is what is not available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Memory {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadSample {
    pub cores: u32,
    /// The 1, 5 and 15 minute load averages, as the kernel prints them.
    pub load_average: [String; 3],
    pub cpu: CpuTimes,
    pub memory: Memory,
}

/// The busy share between consecutive [`CpuTimes`].
#[derive(Debug, Default)]
pub struct CpuMeter {
    last: Option<CpuTimes>,
}

impl CpuMeter {
    /// Percent busy since the previous sample: `None` for the first, or
    /// when the times went backwards.
    pub fn add(&mut self, times: CpuTimes) -> Option<u8> {
        let percent = self.last.and_then(|last| {
            let total = times.total.checked_sub(last.total)?;
            let busy = times.busy.checked_sub(last.busy)?;
            (total > 0 && busy <= total).then(|| ((busy * 100 + total / 2) / total) as u8)
        });
        self.last = Some(times);
        percent
    }
}

/// The aggregate `cpu` line's fields after the label: user, nice, system,
/// idle, iowait, irq, softirq, steal (the guest fields are already in
/// user and nice). Idle and iowait are idle.
pub fn cpu_times(fields: &str) -> Option<CpuTimes> {
    let times: Vec<u64> = fields
        .split_whitespace()
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if times.len() < 4 {
        return None;
    }
    let total = times.iter().sum();
    let idle = times[3] + times.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total - idle,
        total,
    })
}

/// A sample line from the script.
pub fn parse_sample(line: &str) -> Option<LoadSample> {
    let rest = line.trim().strip_prefix(MARKER)?;
    let mut fields = rest.splitn(9, ' ');
    let mut next = || fields.next();
    let cores = next()?.parse().ok()?;
    let load_average = [next()?, next()?, next()?].map(str::to_string);
    if load_average.iter().any(|l| l.parse::<f64>().is_err()) {
        return None;
    }
    let mut kib = || -> Option<u64> { Some(next()?.parse::<u64>().ok()? * 1024) };
    let memory = Memory {
        total: kib()?,
        available: kib()?,
        swap_total: kib()?,
        swap_free: kib()?,
    };
    Some(LoadSample {
        cores,
        load_average,
        cpu: cpu_times(next()?)?,
        memory,
    })
}

/// `/proc/stat`'s aggregate times and its count of `cpuN` lines.
pub fn parse_stat(stat: &str) -> Option<(CpuTimes, u32)> {
    let times = cpu_times(stat.lines().next()?.strip_prefix("cpu ")?)?;
    let cores = stat
        .lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count();
    Some((times, cores as u32))
}

/// `/proc/meminfo`'s memory and swap, with the pre-3.14 stand-in for
/// `MemAvailable`.
pub fn parse_meminfo(meminfo: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<u64> {
        meminfo.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == name).then(|| words.next()?.parse::<u64>().ok())?
        })
    };
    let available = field("MemAvailable:").or_else(|| {
        Some(field("MemFree:")? + field("Buffers:").unwrap_or(0) + field("Cached:").unwrap_or(0))
    })?;
    Some(Memory {
        total: field("MemTotal:")? * 1024,
        available: available * 1024,
        swap_total: field("SwapTotal:").unwrap_or(0) * 1024,
        swap_free: field("SwapFree:").unwrap_or(0) * 1024,
    })
}

/// This machine's sample.
pub fn local_sample() -> Option<LoadSample> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let (cpu, cores) = parse_stat(&read("/proc/stat")?)?;
    let loadavg = read("/proc/loadavg")?;
    let mut averages = loadavg.split_whitespace().map(str::to_string);
    let load_average = [averages.next()?, averages.next()?, averages.next()?];
    Some(LoadSample {
        cores,
        load_average,
        cpu,
        memory: parse_meminfo(&read("/proc/meminfo")?)?,
    })
}

/// Send the server's samples to `samples` until its receiver is dropped or
/// the server stops (`remote_script::stream`).
pub async fn stream_server(connection: Weak<SshConnection>, samples: mpsc::Sender<LoadSample>) {
    remote_script::stream(connection, &sample_script(), parse_sample, samples).await;
}

/// Send this machine's samples to `samples` until its receiver is dropped.
pub async fn stream_local(samples: mpsc::Sender<LoadSample>) {
    sampling::poll_local(INTERVAL, local_sample, samples).await;
}

#[cfg(test)]
mod tests;
