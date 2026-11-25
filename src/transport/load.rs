//! A host's CPU and memory use, from the host script's `cpu` and `mem`
//! lines (`host_stats`; doc 04 §Server figures).

/// Prefix each sample, since login-shell startup files may print too.
const CPU: &str = "wayterm-cpu:";
const MEMORY: &str = "wayterm-mem:";

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
pub struct CpuSample {
    pub cores: u32,
    /// The 1, 5 and 15 minute load averages, as the kernel prints them.
    pub load_average: [String; 3],
    pub times: CpuTimes,
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

/// `wayterm-cpu:<cores> <load averages> <cpu line fields>`.
pub fn parse_cpu(line: &str) -> Option<CpuSample> {
    let rest = line.trim().strip_prefix(CPU)?;
    let mut fields = rest.splitn(5, ' ');
    let mut next = || fields.next();
    let cores = next()?.parse().ok()?;
    let load_average = [next()?, next()?, next()?].map(str::to_string);
    if load_average.iter().any(|l| l.parse::<f64>().is_err()) {
        return None;
    }
    Some(CpuSample {
        cores,
        load_average,
        times: cpu_times(next()?)?,
    })
}

/// `wayterm-mem:<total> <available> <swap total> <swap free>`, in KiB.
pub fn parse_memory(line: &str) -> Option<Memory> {
    let rest = line.trim().strip_prefix(MEMORY)?;
    let kib: Vec<u64> = rest
        .split(' ')
        .map(|field| field.parse::<u64>().ok().map(|kib| kib * 1024))
        .collect::<Option<_>>()?;
    let [total, available, swap_total, swap_free] = kib[..] else {
        return None;
    };
    Some(Memory {
        total,
        available,
        swap_total,
        swap_free,
    })
}

#[cfg(test)]
mod tests;
