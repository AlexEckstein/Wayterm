use super::*;

fn mount(device: &str, path: &str, kind: &str) -> Mount {
    Mount {
        device: device.to_string(),
        path: path.to_string(),
        kind: kind.to_string(),
    }
}

fn usage(size_gib: u64, used_gib: u64, available_gib: u64) -> Usage {
    const GIB: u64 = 1 << 30;
    Usage {
        size: size_gib * GIB,
        used: used_gib * GIB,
        available: available_gib * GIB,
    }
}

#[test]
fn mount_lines_unescape_the_mount_point() {
    assert_eq!(
        parse_mount("/dev/sdb1 /mnt/My\\040Disk ext4 rw,relatime 0 0"),
        Some(mount("/dev/sdb1", "/mnt/My Disk", "ext4"))
    );
    assert_eq!(unescape("a\\134b\\011c\\"), "a\\b\tc\\");
    assert_eq!(parse_mount("/dev/sdb1 /mnt"), None);
}

#[test]
fn df_lines_keep_spaces_in_the_mount_point() {
    assert_eq!(
        parse_df("/dev/sdb1   1048576  524288  524288  50% /mnt/My Disk"),
        Some((
            "/mnt/My Disk".to_string(),
            Usage {
                size: 1 << 30,
                used: 1 << 29,
                available: 1 << 29,
            }
        ))
    );
    assert_eq!(
        parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on"),
        None
    );
    assert_eq!(parse_df("/dev/sdb1 1 1 1 50%"), None);
}

#[test]
fn percent_is_dfs_rounded_up_without_reserved_blocks() {
    assert_eq!(usage(50, 45, 0).percent(), 100);
    assert_eq!(usage(100, 46, 4).percent(), 92);
    assert_eq!(
        Usage {
            size: 1000,
            used: 501,
            available: 499
        }
        .percent(),
        51
    );
    assert_eq!(Usage::default().percent(), 0);
}

#[test]
fn only_real_storage_counts_once() {
    let mounts = [
        mount("rootfs", "/", "rootfs"),
        mount("/dev/sda2", "/", "btrfs"),
        mount("proc", "/proc", "proc"),
        mount("tmpfs", "/run", "tmpfs"),
        mount("/dev/sda2", "/home", "btrfs"),
        mount("/dev/loop3", "/snap/core/1", "squashfs"),
        mount("/dev/sda1", "/boot", "vfat"),
        mount("nas:/export", "/mnt/nas", "nfs4"),
        mount("/dev/sdc1", "/mnt/empty", "ext4"),
    ];
    let mut asked = Vec::new();
    let filesystems = real_filesystems(&mounts, |mount| {
        asked.push(mount.path.clone());
        match mount.path.as_str() {
            "/" => Some(usage(45, 20, 25)),
            "/boot" => Some(usage(1, 0, 1)),
            "/mnt/nas" => Some(usage(500, 100, 400)),
            _ => Some(Usage::default()),
        }
    });
    let paths: Vec<&str> = filesystems.iter().map(|f| f.mount.as_str()).collect();
    assert_eq!(paths, ["/", "/boot", "/mnt/nas"]);
    assert!(
        !asked.contains(&"/home".to_string()),
        "the subvolume shares /'s device"
    );
    assert!(!asked.contains(&"/run".to_string()) && !asked.contains(&"/snap/core/1".to_string()));
}

#[test]
fn a_device_falls_back_to_a_mount_df_listed() {
    let mounts = [
        mount("/dev/sda2", "/", "btrfs"),
        mount("/dev/sda2", "/home", "btrfs"),
    ];
    let filesystems = real_filesystems(&mounts, |mount| {
        (mount.path == "/home").then(|| usage(45, 20, 25))
    });
    assert_eq!(
        filesystems,
        [Filesystem {
            mount: "/home".to_string(),
            usage: usage(45, 20, 25),
            pool: None,
        }]
    );
}

fn dataset(mount: &str, pool: &str, used_gib: u64, available_gib: u64) -> Filesystem {
    Filesystem {
        mount: mount.to_string(),
        usage: usage(used_gib + available_gib, used_gib, available_gib),
        pool: Some(pool.to_string()),
    }
}

#[test]
fn zfs_datasets_name_their_pool() {
    let mounts = [
        mount("tank/root", "/", "zfs"),
        mount("tank/home", "/home", "zfs"),
        mount("/dev/sda1", "/boot", "ext4"),
    ];
    let filesystems = real_filesystems(&mounts, |_| Some(usage(10, 1, 9)));
    let pools: Vec<Option<&str>> = filesystems.iter().map(|f| f.pool.as_deref()).collect();
    assert_eq!(pools, [Some("tank"), None, Some("tank")]);
}

#[test]
fn a_pool_counts_its_free_space_once() {
    let filesystems = [
        dataset("/", "tank", 50, 600),
        dataset("/home", "tank", 300, 600),
        dataset("/var", "tank", 50, 400),
        Filesystem {
            mount: "/boot".to_string(),
            usage: usage(2, 1, 1),
            pool: None,
        },
    ];
    assert_eq!(
        pool_usage(&filesystems, "tank"),
        usage(1000, 400, 600),
        "a quota lowers only /var's free space"
    );
    assert_eq!(total_usage(&filesystems), usage(1002, 401, 601));
    assert_eq!(filesystems[0].percent(&filesystems), 40, "the pool's");
    assert_eq!(filesystems[3].percent(&filesystems), 50);
}

fn feed(collector: &mut Collector, lines: &[&str]) -> Option<Disks> {
    lines.iter().fold(None, |_, line| collector.line(line))
}

fn mounts_of(disks: &Disks) -> Vec<&str> {
    disks.filesystems.iter().map(|f| f.mount.as_str()).collect()
}

const TABLE: &[&str] = &[
    "wayterm-mount:/dev/sda2 / ext4 rw 0 0\n",
    "wayterm-mount:tmpfs /run tmpfs rw 0 0\n",
    "wayterm-mount:nas:/export /mnt/nas nfs4 rw 0 0\n",
    "motd from a login shell\n",
    "wayterm-mounts\n",
];

#[test]
fn each_group_reports_its_own_df() {
    let mut collector = Collector::default();
    assert_eq!(feed(&mut collector, TABLE), None);
    let lines = [
        "wayterm-df:net Filesystem 1024-blocks Used Available Capacity Mounted on\n",
        "wayterm-df:local Filesystem 1024-blocks Used Available Capacity Mounted on\n",
        "wayterm-load:interleaved, not a disk line\n",
        "wayterm-df:local /dev/sda2 47185920 20971520 26214400 45% /\n",
    ];
    assert_eq!(feed(&mut collector, &lines), None, "no group has ended");
    let disks = collector.line("wayterm-disks:local\n").expect("a reading");
    assert_eq!(mounts_of(&disks), ["/"]);
    assert_eq!(disks.filesystems[0].usage, usage(45, 20, 25));
    assert!(disks.late.is_empty());

    collector.line("wayterm-df:net nas:/export 1048576 524288 524288 50% /mnt/nas\n");
    let disks = collector.line("wayterm-disks:net\n").expect("a reading");
    assert_eq!(
        mounts_of(&disks),
        ["/", "/mnt/nas"],
        "the local reading stays"
    );
}

#[test]
fn a_late_group_keeps_its_last_reading() {
    let mut collector = Collector::default();
    feed(&mut collector, TABLE);
    feed(
        &mut collector,
        &[
            "wayterm-df:local /dev/sda2 47185920 20971520 26214400 45% /\n",
            "wayterm-disks:local\n",
        ],
    );
    let disks = collector
        .line("wayterm-disks-late:net\n")
        .expect("a report");
    assert_eq!(mounts_of(&disks), ["/"]);
    assert_eq!(
        disks.late,
        [Late {
            group: Group::Network,
            last_read: None
        }],
        "never read"
    );

    feed(
        &mut collector,
        &[
            "wayterm-df:net nas:/export 1048576 524288 524288 50% /mnt/nas\n",
            "wayterm-disks:net\n",
        ],
    );
    let disks = collector.line("wayterm-disks-late:net\n").unwrap();
    assert_eq!(mounts_of(&disks), ["/", "/mnt/nas"]);
    assert!(disks.late[0].last_read.is_some());
    let disks = collector.line("wayterm-disks:net\n").unwrap();
    assert!(disks.late.is_empty(), "the late job finished");
    assert_eq!(collector.line("wayterm-disks-late:other\n"), None);
}

#[test]
fn a_new_mount_table_drops_what_was_unmounted() {
    let mut collector = Collector::default();
    feed(&mut collector, TABLE);
    feed(
        &mut collector,
        &[
            "wayterm-df:net nas:/export 1048576 524288 524288 50% /mnt/nas\n",
            "wayterm-disks:net\n",
        ],
    );
    feed(
        &mut collector,
        &[
            "wayterm-mount:/dev/sda2 / ext4 rw 0 0\n",
            "wayterm-mounts\n",
        ],
    );
    let disks = collector.line("wayterm-disks-late:net\n").unwrap();
    assert_eq!(mounts_of(&disks), Vec::<&str>::new());
}
