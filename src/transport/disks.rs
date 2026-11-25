//! A host's filesystems and how full each is, read every [`INTERVAL`] by
//! the host script's `df` jobs (`host_stats`; doc 04 §Server figures).
//! Only real storage counts: memory-backed, overlay and read-only image
//! filesystems are skipped, a device mounted twice counts once, and the
//! datasets of a ZFS pool share its free space. The local and the network
//! filesystems are read apart, so a hung network mount stalls only its
//! own figures.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Disk use changes slowly, and `df` costs more than reading `/proc`.
pub const INTERVAL: Duration = Duration::from_secs(30);
const MOUNT: &str = "wayterm-mount:";
const MOUNTS_END: &str = "wayterm-mounts";
const DF: &str = "wayterm-df:";
const END: &str = "wayterm-disks:";
const LATE: &str = "wayterm-disks-late:";

/// Not storage, or always full: memory-backed, overlays, read-only images
/// and the desktop's FUSE bridges. The script never runs `df` on them (an
/// autofs mount point would mount), and a path they mount over is hidden.
pub(crate) const SKIPPED_TYPES: &[&str] = &[
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

/// Shell `case` patterns for the filesystems read by the network `df` job:
/// network filesystems, and FUSE ones, which hang the same way when their
/// daemon stops (`fuseblk` is a local block device).
pub(crate) const NETWORK_TYPES: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ncpfs",
    "9p",
    "afs",
    "ceph",
    "glusterfs",
    "lustre",
    "gpfs",
    "beegfs",
    "orangefs",
    "davfs",
    "fuse",
    "fuse.*",
];

/// Which `df` job read a filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Group {
    Local,
    Network,
}

impl Group {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "net" => Some(Self::Network),
            _ => None,
        }
    }
}

/// A group whose `df` had not finished when the next round was due, so its
/// figures are from `last_read` (never, when `None`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Late {
    pub group: Group,
    pub last_read: Option<Instant>,
}

/// The filesystems from each group's last finished `df`, and the groups
/// whose `df` is late.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Disks {
    pub filesystems: Vec<Filesystem>,
    pub late: Vec<Late>,
}

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
/// subvolumes) keeps the shortest one that has a usage; a zero-sized
/// filesystem is no storage.
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

/// Builds [`Disks`] from the script's lines: the mount table of the last
/// round, and each group's last finished `df`. The groups' lines may come
/// interleaved, since their jobs run side by side.
#[derive(Debug, Default)]
pub struct Collector {
    table: Vec<Mount>,
    mounts: Vec<Mount>,
    local: GroupReads,
    network: GroupReads,
}

#[derive(Debug, Default)]
struct GroupReads {
    pending: Vec<(String, Usage)>,
    read: Vec<(String, Usage)>,
    last_read: Option<Instant>,
    late: bool,
}

impl Collector {
    /// Take one line; a group's end or late line returns the disks.
    pub fn line(&mut self, line: &str) -> Option<Disks> {
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(mount) = line.strip_prefix(MOUNT) {
            self.table.extend(parse_mount(mount));
        } else if line.trim() == MOUNTS_END {
            self.mounts = std::mem::take(&mut self.table);
        } else if let Some(rest) = line.strip_prefix(DF) {
            let (group, df) = rest.split_once(' ')?;
            let usage = parse_df(df)?;
            self.group(Group::parse(group)?).pending.push(usage);
        } else if let Some(group) = line.strip_prefix(END) {
            let reads = self.group(Group::parse(group.trim())?);
            reads.read = std::mem::take(&mut reads.pending);
            reads.last_read = Some(Instant::now());
            reads.late = false;
            return Some(self.disks());
        } else if let Some(group) = line.strip_prefix(LATE) {
            self.group(Group::parse(group.trim())?).late = true;
            return Some(self.disks());
        }
        None
    }

    fn group(&mut self, group: Group) -> &mut GroupReads {
        match group {
            Group::Local => &mut self.local,
            Group::Network => &mut self.network,
        }
    }

    fn disks(&self) -> Disks {
        let reads = [(Group::Local, &self.local), (Group::Network, &self.network)];
        let filesystems = real_filesystems(&self.mounts, |mount| {
            reads.iter().find_map(|(_, reads)| {
                reads
                    .read
                    .iter()
                    .rev()
                    .find(|(path, _)| *path == mount.path)
                    .map(|(_, usage)| *usage)
            })
        });
        let late = reads
            .iter()
            .filter(|(_, reads)| reads.late)
            .map(|&(group, reads)| Late {
                group,
                last_read: reads.last_read,
            })
            .collect();
        Disks { filesystems, late }
    }
}

#[cfg(test)]
mod tests;
