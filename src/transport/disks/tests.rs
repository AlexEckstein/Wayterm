use std::io::Write;
use std::process::{Command, Stdio};

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
        }]
    );
}

#[test]
fn the_collector_ends_a_round_at_its_end_line() {
    let mut collector = Collector::default();
    let lines = [
        "wayterm-mount:/dev/sda2 / ext4 rw 0 0\n",
        "wayterm-mount:tmpfs /run tmpfs rw 0 0\n",
        "motd from a login shell\n",
        "wayterm-df:Filesystem 1024-blocks Used Available Capacity Mounted on\n",
        "wayterm-df:/dev/sda2 47185920 20971520 26214400 45% /\n",
        "wayterm-df:tmpfs 1024 0 1024 0% /run\n",
    ];
    for line in lines {
        assert_eq!(collector.line(line), None);
    }
    let filesystems = collector.line("wayterm-disks\n").expect("a round");
    assert_eq!(
        filesystems,
        [Filesystem {
            mount: "/".to_string(),
            usage: usage(45, 20, 25),
        }]
    );
    assert_eq!(
        collector.line("wayterm-disks\n"),
        Some(Vec::new()),
        "each round starts empty"
    );
}

fn run_script() -> Option<Vec<Filesystem>> {
    let script = sample_script()
        .replace("while :; do", "for _ in 1; do")
        .replace(&format!("sleep {}", INTERVAL.as_secs()), "true");
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
    let mut collector = Collector::default();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| collector.line(line))
}

/// On this (Linux) machine the script and `statvfs` list the same
/// filesystems.
#[test]
fn the_script_lists_this_machines_filesystems() {
    let Some(expected) = local_filesystems() else {
        return;
    };
    let listed = run_script().expect("a round");
    let mounts = |filesystems: &[Filesystem]| -> Vec<String> {
        filesystems.iter().map(|f| f.mount.clone()).collect()
    };
    assert_eq!(mounts(&listed), mounts(&expected));
}

#[test]
fn the_script_sleeps_for_the_interval() {
    assert!(sample_script().contains(&format!("sleep {}\n", INTERVAL.as_secs())));
}
