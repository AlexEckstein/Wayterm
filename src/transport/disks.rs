//! A host's filesystems and how full each is, sampled every [`INTERVAL`]:
//! over a script on the server for SSH tabs (doc 04 §Server load and
//! disks), with `statvfs` on this machine for local shells (doc 06 §Local
//! shell). Only real storage counts: memory-backed, overlay and read-only
//! image filesystems are skipped, a device mounted twice counts once, and
//! the datasets of a ZFS pool share its free space.

use std::collections::HashSet;
use std::ffi::CString;
use std::sync::Weak;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::transport::connection::SshConnection;
use crate::transport::{remote_script, sampling};

/// Disk use changes slowly, and `df` costs more than reading `/proc`.
pub const INTERVAL: Duration = Duration::from_secs(30);
const MOUNT: &str = "wayterm-mount:";
const DF: &str = "wayterm-df:";
const END: &str = "wayterm-disks";

/// Prints the mount table and `df`'s POSIX listing (KiB), then an end
/// line, every `{interval}` seconds until its output closes. It runs on a
/// channel of its own, so a `df` stuck on a hung network mount stalls only
/// this figure.
const SAMPLE_SCRIPT: &str = r#"[ -r /proc/mounts ] || exit 0
while :; do
  sed 's/^/wayterm-mount:/' /proc/mounts
  df -P -k 2>/dev/null | sed 's/^/wayterm-df:/'
  printf 'wayterm-disks\n' || exit 0
  sleep {interval}
done
"#;

fn sample_script() -> String {
    SAMPLE_SCRIPT.replace("{interval}", &INTERVAL.as_secs().to_string())
}

/// Not storage, or always full: memory-backed, overlays, read-only images
/// and the desktop's FUSE bridges.
const SKIPPED_TYPES: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "ramfs",
    "overlay",
    "aufs",
    "squashfs",
    "erofs",
    "composefs",
    "iso9660",
    "udf",
    "autofs",
    "efivarfs",
    "nsfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "fuse.lxcfs",
    "fuse.snapfuse",
];

/// One line of the mount table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mount {
    pub device: String,
    pub path: String,
    pub kind: String,
}

/// Sizes in bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub size: u64,
    pub used: u64,
    pub available: u64,
}

impl Usage {
    /// `df`'s Use%: used over what a user could fill (the reserved blocks
    /// excluded), rounded up.
    pub fn percent(&self) -> u8 {
        let usable = self.used + self.available;
        if usable == 0 {
            return 0;
        }
        self.used.saturating_mul(100).div_ceil(usable).min(100) as u8
    }
}

impl std::ops::Add for Usage {
    type Output = Usage;

    fn add(self, other: Usage) -> Usage {
        Usage {
            size: self.size + other.size,
            used: self.used + other.used,
            available: self.available + other.available,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Filesystem {
    pub mount: String,
    pub usage: Usage,
    /// The ZFS pool a dataset belongs to. Every dataset reports the pool's
    /// free space as its own, so a pool's space is reckoned across them.
    pub pool: Option<String>,
}

impl Filesystem {
    /// How full it is: a ZFS dataset is as full as its pool.
    pub fn percent(&self, all: &[Filesystem]) -> u8 {
        match &self.pool {
            Some(pool) => pool_usage(all, pool).percent(),
            None => self.usage.percent(),
        }
    }
}

/// The space `pool`'s datasets among `filesystems` share: their used space
/// summed, and the pool's free space once. A quota only lowers what a
/// dataset reports free, so the largest is the pool's.
pub fn pool_usage(filesystems: &[Filesystem], pool: &str) -> Usage {
    let datasets = filesystems
        .iter()
        .filter(|f| f.pool.as_deref() == Some(pool));
    let (used, available) = datasets.fold((0, 0), |(used, available), f| {
        (used + f.usage.used, available.max(f.usage.available))
    });
    Usage {
        size: used + available,
        used,
        available,
    }
}

/// The total across `filesystems`, each ZFS pool counted once.
pub fn total_usage(filesystems: &[Filesystem]) -> Usage {
    let mut pools = HashSet::new();
    filesystems
        .iter()
        .filter_map(|f| match &f.pool {
            None => Some(f.usage),
            Some(pool) => pools
                .insert(pool.as_str())
                .then(|| pool_usage(filesystems, pool)),
        })
        .fold(Usage::default(), |sum, usage| sum + usage)
}

/// The kernel escapes space, tab, newline and backslash as `\ooo`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|digits| {
            std::str::from_utf8(digits)
                .ok()
                .and_then(|d| u8::from_str_radix(d, 8).ok())
        });
        match (bytes[i], octal) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `/proc/mounts` line: device, mount point, type, options.
pub fn parse_mount(line: &str) -> Option<Mount> {
    let mut fields = line.split_whitespace();
    Some(Mount {
        device: unescape(fields.next()?),
        path: unescape(fields.next()?),
        kind: fields.next()?.to_string(),
    })
}

/// A `df -P -k` line: device, size, used, available, capacity, then the
/// mount point, which may hold spaces. The header is not one.
pub fn parse_df(line: &str) -> Option<(String, Usage)> {
    let mut rest = line.trim();
    let mut fields = Vec::with_capacity(5);
    for _ in 0..5 {
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    let kib = |field: &str| field.parse::<u64>().ok().map(|k| k * 1024);
    let usage = Usage {
        size: kib(fields[1])?,
        used: kib(fields[2])?,
        available: kib(fields[3])?,
    };
    (!rest.is_empty()).then(|| (rest.to_string(), usage))
}

/// The real filesystems among `mounts`, sorted by mount point, with
/// `usage` asked of each candidate. A mount point mounted over keeps only
/// its last mount; a device mounted at several points (bind mounts, btrfs
/// subvolumes) keeps the shortest one that has a usage (GNU `df` lists only
/// one of them); a zero-sized filesystem is no storage.
pub fn real_filesystems(
    mounts: &[Mount],
    mut usage: impl FnMut(&Mount) -> Option<Usage>,
) -> Vec<Filesystem> {
    let mut seen_paths = HashSet::new();
    let mut visible: Vec<&Mount> = mounts
        .iter()
        .rev()
        .filter(|mount| seen_paths.insert(mount.path.as_str()))
        .filter(|mount| !SKIPPED_TYPES.contains(&mount.kind.as_str()))
        .collect();
    visible.sort_by(|a, b| (a.path.len(), &a.path).cmp(&(b.path.len(), &b.path)));
    let mut seen_devices = HashSet::new();
    let mut filesystems: Vec<Filesystem> = visible
        .into_iter()
        .filter_map(|mount| {
            if seen_devices.contains(mount.device.as_str()) {
                return None;
            }
            let usage = usage(mount).filter(|usage| usage.size > 0)?;
            seen_devices.insert(mount.device.as_str());
            Some(Filesystem {
                mount: mount.path.clone(),
                usage,
                pool: (mount.kind == "zfs").then(|| {
                    let pool = mount.device.split('/').next().unwrap_or_default();
                    pool.to_string()
                }),
            })
        })
        .collect();
    filesystems.sort_by(|a, b| a.mount.cmp(&b.mount));
    filesystems
}

/// Collects one round of the script's lines into filesystems.
#[derive(Debug, Default)]
pub struct Collector {
    mounts: Vec<Mount>,
    df: Vec<(String, Usage)>,
}

impl Collector {
    /// Take one line; the round's end line returns the filesystems.
    pub fn line(&mut self, line: &str) -> Option<Vec<Filesystem>> {
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(mount) = line.strip_prefix(MOUNT) {
            self.mounts.extend(parse_mount(mount));
        } else if let Some(df) = line.strip_prefix(DF) {
            self.df.extend(parse_df(df));
        } else if line.trim() == END {
            let mounts = std::mem::take(&mut self.mounts);
            let df = std::mem::take(&mut self.df);
            return Some(real_filesystems(&mounts, |mount| {
                df.iter()
                    .rev()
                    .find(|(path, _)| *path == mount.path)
                    .map(|(_, usage)| *usage)
            }));
        }
        None
    }
}

// The block counts are narrower than u64 on 32-bit targets.
#[allow(clippy::unnecessary_cast)]
fn statvfs(path: &str) -> Option<Usage> {
    let path = CString::new(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is written on success.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled `stat`.
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize as u64;
    Some(Usage {
        size: stat.f_blocks as u64 * block,
        used: (stat.f_blocks as u64).saturating_sub(stat.f_bfree as u64) * block,
        available: stat.f_bavail as u64 * block,
    })
}

/// This machine's real filesystems. `statvfs` can block on a hung network
/// mount, so this runs off the GTK thread.
pub fn local_filesystems() -> Option<Vec<Filesystem>> {
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let mounts: Vec<Mount> = table.lines().filter_map(parse_mount).collect();
    Some(real_filesystems(&mounts, |mount| statvfs(&mount.path)))
}

/// Send the server's filesystems to `samples` every interval until its
/// receiver is dropped or the server stops (`remote_script::stream`).
pub async fn stream_server(
    connection: Weak<SshConnection>,
    samples: mpsc::Sender<Vec<Filesystem>>,
) {
    let mut collector = Collector::default();
    remote_script::stream(
        connection,
        &sample_script(),
        move |line| collector.line(line),
        samples,
    )
    .await;
}

/// Send this machine's filesystems to `samples` until its receiver is
/// dropped.
pub async fn stream_local(samples: mpsc::Sender<Vec<Filesystem>>) {
    sampling::poll_local(INTERVAL, local_filesystems, samples).await;
}

#[cfg(test)]
mod tests;
