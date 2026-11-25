use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::*;
use crate::transport::disks::{Group, Late};
use crate::transport::load::{Memory, cpu_times};

const UPTIME_FILE: &str = "12345.67 890123.45\n";

const STAT: &str = "cpu  100 20 30 800 50 5 5 0 10 0\n\
cpu0 50 10 15 400 25 2 3 0 5 0\n\
cpu1 50 10 15 400 25 3 2 0 5 0\n\
intr 12345 0 0 0\n";

const MEMINFO: &str = "MemTotal:        8000000 kB\n\
MemFree:         1000000 kB\n\
MemAvailable:    5000000 kB\n\
Buffers:          200000 kB\n\
Cached:          2000000 kB\n\
SwapCached:            0 kB\n\
SwapTotal:       2000000 kB\n\
SwapFree:        1500000 kB\n";

const LOADAVG: &str = "0.52 0.48 0.40 1/123 4567\n";

const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\n\
ens3\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n";

const ROUTE_HEADER_ONLY: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n";

/// Unreachable defaults on `lo` first, as the kernel lists them.
const IPV6_ROUTE: &str = "\
fd7a115ca1e000000000000000000000 30 00000000000000000000000000000000 00 00000000000000000000000000000000 00000400 00000001 00000000 00000001 tailscale0\n\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo\n\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00450003     ens3\n";

const DEV: &str = "Inter-|   Receive                            |  Transmit\n \
face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo: 9999 10 0 0 0 0 0 0 9999 10 0 0 0 0 0 0\n  \
ens3:123456 789 0 0 0 0 0 0 654321 456 0 0 0 0 0 0\n";

const MOUNTS: &str = "/dev/sda2 / ext4 rw 0 0\n\
tmpfs /run tmpfs rw 0 0\n\
systemd-1 /mnt/auto autofs rw 0 0\n\
portal /run/user/1000/doc fuse.portal rw 0 0\n\
/dev/sdb1 /mnt/My\\040Disk ext4 rw 0 0\n\
nas:/export /mnt/nas nfs4 rw 0 0\n\
me@host: /mnt/ssh fuse.sshfs rw 0 0\n";

fn memory(available_kib: u64) -> Memory {
    Memory {
        total: 8_000_000 * 1024,
        available: available_kib * 1024,
        swap_total: 2_000_000 * 1024,
        swap_free: 1_500_000 * 1024,
    }
}

/// A stand-in host: a `/proc` of fixture files, and a `bin` directory put
/// first on the script's `PATH`.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("proc/net")).unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        for (path, contents) in files {
            std::fs::write(dir.path().join("proc").join(path), contents).unwrap();
        }
        Self { dir }
    }

    /// Every figure: a whole Linux host.
    fn linux() -> Self {
        let fixture = Self::new(&[
            ("uptime", UPTIME_FILE),
            ("stat", STAT),
            ("meminfo", MEMINFO),
            ("loadavg", LOADAVG),
            ("net/route", ROUTE),
            ("net/dev", DEV),
            ("mounts", MOUNTS),
        ]);
        fixture.fake_df();
        fixture
    }

    fn proc(&self) -> std::path::PathBuf {
        self.dir.path().join("proc")
    }

    fn command(&self, name: &str, body: &str) {
        let path = self.dir.path().join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A `df` that logs its operands and lists each half full; a network
    /// mount hangs for `hang` seconds once the `hang` file exists.
    fn fake_df(&self) {
        let root = self.dir.path().display();
        self.command(
            "df",
            &format!(
                r#"printf '%s\n' "$@" >> '{root}/df.log'
echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'
for m; do
  case $m in -P|-k|--) continue ;; esac
  case $m in /mnt/nas) [ -e '{root}/hang' ] && sleep "$(cat '{root}/hang')" ;; esac
  echo "dev 1048576 524288 524288 50% $m"
done
"#
            ),
        );
    }

    fn df_operands(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("df.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with('-'))
            .map(str::to_string)
            .collect()
    }

    /// The script's output over `ticks` ticks of `interval`, reading the
    /// disks every `disk_ticks`.
    fn run(&self, interval: &str, disk_ticks: u64, ticks: usize) -> String {
        let proc = format!("{}/", self.proc().display());
        let bin = self.dir.path().join("bin");
        run_script(
            &render(interval, disk_ticks),
            Some(&proc),
            Some(&bin),
            ticks,
        )
    }
}

/// `script` under this machine's `sh` for `ticks` ticks, its `/proc/`
/// replaced by `proc`, with `bin` first on `PATH`; waits for the
/// background jobs too, since they hold its output open.
fn run_script(script: &str, proc: Option<&str>, bin: Option<&Path>, ticks: usize) -> String {
    let rounds: Vec<String> = (0..ticks).map(|n| n.to_string()).collect();
    let mut script = script.replace("while :; do", &format!("for _ in {}; do", rounds.join(" ")));
    if let Some(proc) = proc {
        script = script.replace("/proc/", proc);
    }
    let mut command = Command::new("sh");
    if let Some(bin) = bin {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", bin.display()));
    }
    let mut sh = command
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
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn samples(output: &str) -> Vec<Sample> {
    let mut parser = Parser::default();
    output
        .lines()
        .filter_map(|line| parser.line(line))
        .collect()
}

fn last_disks(samples: &[Sample]) -> Option<&Disks> {
    samples.iter().rev().find_map(|sample| match sample {
        Sample::Disks(disks) => Some(disks),
        _ => None,
    })
}

fn count(output: &str, line: &str) -> usize {
    output.lines().filter(|l| *l == line).count()
}

#[test]
fn the_script_reports_every_figure() {
    let fixture = Fixture::linux();
    let samples = samples(&fixture.run("0", 15, 1));
    assert_eq!(samples[0], Sample::Uptime(12345), "the uptime comes first");
    assert!(samples.contains(&Sample::Load(LoadSample {
        cores: 2,
        load_average: ["0.52", "0.48", "0.40"].map(str::to_string),
        cpu: cpu_times("100 20 30 800 50 5 5 0 10 0").unwrap(),
        memory: memory(5_000_000),
    })));
    assert!(samples.contains(&Sample::Net(Counters {
        interface: "ens3".to_string(),
        rx: 123456,
        tx: 654321,
    })));
    let disks = last_disks(&samples).expect("both groups read");
    let mounts: Vec<&str> = disks.filesystems.iter().map(|f| f.mount.as_str()).collect();
    assert_eq!(mounts, ["/", "/mnt/My Disk", "/mnt/nas", "/mnt/ssh"]);
    assert!(disks.late.is_empty());

    let operands = fixture.df_operands();
    for skipped in ["/run", "/mnt/auto", "/run/user/1000/doc"] {
        assert!(
            !operands.iter().any(|o| o == skipped),
            "{skipped}: {operands:?}"
        );
    }
    assert!(operands.iter().any(|o| o == "/mnt/My Disk"), "unescaped");
}

#[test]
fn local_and_network_mounts_get_their_own_df() {
    let fixture = Fixture::linux();
    let output = fixture.run("0", 15, 1);
    let group_of = |mount: &str| {
        output.lines().find_map(|line| {
            let rest = line.strip_prefix("wayterm-df:")?;
            rest.ends_with(&format!(" {mount}"))
                .then(|| rest.split(' ').next().unwrap().to_string())
        })
    };
    assert_eq!(group_of("/").as_deref(), Some("local"));
    assert_eq!(group_of("/mnt/My Disk").as_deref(), Some("local"));
    assert_eq!(group_of("/mnt/nas").as_deref(), Some("net"));
    assert_eq!(group_of("/mnt/ssh").as_deref(), Some("net"), "FUSE");
}

#[test]
fn a_hung_network_mount_skips_only_its_rounds() {
    let fixture = Fixture::linux();
    std::fs::write(fixture.dir.path().join("hang"), "3").unwrap();
    let output = fixture.run("0.5", 1, 4);
    assert_eq!(count(&output, "wayterm-disks-late:net"), 3);
    assert_eq!(count(&output, "wayterm-disks:net"), 1, "it finishes once");
    assert!(count(&output, "wayterm-disks:local") >= 3);
    let loads = output
        .lines()
        .filter(|l| l.starts_with("wayterm-load:"))
        .count();
    assert_eq!(loads, 4, "the load never waits");
    let nas = fixture.df_operands();
    assert_eq!(nas.iter().filter(|o| *o == "/mnt/nas").count(), 1);

    let samples = samples(&output);
    let late = samples.iter().find_map(|sample| match sample {
        Sample::Disks(disks) if !disks.late.is_empty() => Some(disks),
        _ => None,
    });
    let late = late.expect("a late report");
    assert_eq!(
        late.late,
        [Late {
            group: Group::Network,
            last_read: None,
        }]
    );
    assert!(
        late.filesystems.iter().any(|f| f.mount == "/"),
        "local read"
    );
    assert!(last_disks(&samples).unwrap().late.is_empty());
}

#[test]
fn old_kernels_lack_mem_available() {
    let meminfo = MEMINFO.replace("MemAvailable:    5000000 kB\n", "");
    let fixture = Fixture::new(&[("stat", STAT), ("meminfo", &meminfo), ("loadavg", LOADAVG)]);
    let samples = samples(&fixture.run("0", 15, 1));
    let Some(Sample::Load(load)) = samples.first() else {
        panic!("{samples:?}");
    };
    assert_eq!(load.memory, memory(3_200_000));
}

#[test]
fn the_interface_is_ipv4s_default_then_ipv6s() {
    let base = [("stat", STAT), ("meminfo", MEMINFO), ("loadavg", LOADAVG)];
    let interface = |route: &str, ipv6: &str| {
        let fixture = Fixture::new(&base);
        for (path, contents) in [
            ("net/route", route),
            ("net/ipv6_route", ipv6),
            ("net/dev", DEV),
        ] {
            std::fs::write(fixture.proc().join(path), contents).unwrap();
        }
        let samples = samples(&fixture.run("0", 15, 1));
        assert!(
            samples.iter().any(|s| matches!(s, Sample::Load(_))),
            "the load does not need a route"
        );
        samples.into_iter().find_map(|sample| match sample {
            Sample::Net(counters) => Some(counters.interface),
            _ => None,
        })
    };
    let ipv4 = ROUTE.replace("ens3", "eth0") + "";
    let dev = DEV.replace("ens3", "eth0");
    let fixture = Fixture::new(&base);
    for (path, contents) in [
        ("net/route", ipv4.as_str()),
        ("net/ipv6_route", IPV6_ROUTE),
        ("net/dev", &dev),
    ] {
        std::fs::write(fixture.proc().join(path), contents).unwrap();
    }
    assert!(
        samples(&fixture.run("0", 15, 1))
            .iter()
            .any(|s| matches!(s, Sample::Net(c) if c.interface == "eth0")),
        "IPv4 first"
    );
    assert_eq!(
        interface(ROUTE_HEADER_ONLY, IPV6_ROUTE).as_deref(),
        Some("ens3")
    );
    let unreachable_only: String = IPV6_ROUTE
        .lines()
        .take(2)
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(interface(ROUTE_HEADER_ONLY, &unreachable_only), None);
}

/// Without `/proc` (macOS, the BSDs) the uptime comes from `sysctl`, in
/// either form they print, and nothing else does.
#[test]
fn without_proc_the_uptime_comes_from_sysctl() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let boot = now - 3600;
    for printed in [
        format!("{{ sec = {boot}, usec = 123 }} Mon Nov 23 10:02:11 2026"),
        boot.to_string(),
    ] {
        let fixture = Fixture::new(&[]);
        fixture.command("sysctl", &format!("echo '{printed}'\n"));
        let samples = samples(&fixture.run("0", 15, 1));
        let [Sample::Uptime(secs)] = samples.as_slice() else {
            panic!("{printed}: {samples:?}");
        };
        assert!(secs.abs_diff(3600) <= 2, "{printed}: {secs}");
    }
    let fixture = Fixture::new(&[]);
    fixture.command("sysctl", "exit 1\n");
    assert_eq!(samples(&fixture.run("0", 15, 1)), []);
}

#[test]
fn uptime_lines_are_marked_whole_seconds() {
    assert_eq!(parse_uptime("wayterm-uptime:12345\n"), Some(12345));
    assert_eq!(parse_uptime("wayterm-uptime:\n"), None);
    assert_eq!(parse_uptime("wayterm-uptime:-5\n"), None);
    assert_eq!(parse_uptime("12345\n"), None);
}

/// On this (Linux) machine the script reads the real `/proc`.
#[test]
fn the_script_samples_this_machine() {
    if !Path::new("/proc/stat").exists() {
        return;
    }
    let samples = samples(&run_script(&render("0", 15), None, None, 1));
    assert!(matches!(samples.first(), Some(Sample::Uptime(_))));
    let load = samples.iter().find_map(|sample| match sample {
        Sample::Load(load) => Some(load),
        _ => None,
    });
    let load = load.expect("a load sample");
    assert!(load.cores > 0 && load.memory.total > 0 && load.cpu.total > 0);
    assert!(last_disks(&samples).is_some(), "a disk reading");
}

#[test]
fn the_script_ticks_every_interval_and_reads_disks_every_30_s() {
    let script = script();
    assert!(script.contains(&format!("sleep {}\n", INTERVAL.as_secs())));
    assert!(script.contains("t=15\n"));
}
