//! A host's CPU and memory use, from the host script's load lines
//! (`host_stats`; doc 04 §Server figures).

/// Prefixes each sample, since login-shell startup files may print too.
const MARKER: &str = "wayterm-load:";

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

#[cfg(test)]
mod tests;
