//! Unit tests for the display-free segments of the status bar.

use crate::store::model::SessionType;
use crate::transport::bandwidth::Rate;
use crate::transport::disks::{Filesystem, Group, Usage};
use crate::transport::load::Memory;
use crate::transport::traffic::Savings;
use crate::transport::types::TransportState;

use super::{
    ColorKind, ConnectedSince, CpuUse, DiskReading, FILLING, FULL, Host, HostFigures, Item,
    LiveStatus, SessionInfo, StaleDisks, TabStatus, Uptime, bandwidth_segment, cpu_segment,
    disk_segment, elapsed_segment, endpoint_segment, format_elapsed, format_rate, format_uptime,
    fullness_color, memory_segment, savings_segment, shorten, state_color, state_segment,
    uptime_segment,
};

fn ssh_session() -> SessionInfo {
    SessionInfo {
        name: "prod".to_string(),
        kind: SessionType::SSH,
        user: Some("alex".to_string()),
        jump: Some("bastion".to_string()),
    }
}

#[test]
fn disconnected_is_red() {
    assert_eq!(state_color(&TransportState::Disconnected), ColorKind::Red);
    let segment = state_segment(&TransportState::Disconnected, "", 0);
    assert_eq!(segment.text, "Disconnected");
    assert_eq!(segment.tooltip, "Disconnected");
    let segment = state_segment(&TransportState::Disconnected, "db", 22);
    assert_eq!(segment.tooltip, "Not connected to db:22");
}

#[test]
fn the_transient_states_are_yellow_and_name_the_endpoint() {
    for (state, text, tooltip) in [
        (
            TransportState::Connecting,
            "Connecting…",
            "Connecting to example.com:22",
        ),
        (
            TransportState::VerifyingHostKey,
            "Verifying host key…",
            "Checking the host key of example.com:22",
        ),
        (
            TransportState::Authenticating,
            "Authenticating…",
            "Logging in to example.com:22",
        ),
        (
            TransportState::Reconnecting { attempt: 3 },
            "Reconnecting (attempt 3)…",
            "Connection lost, reconnecting to example.com:22, attempt 3",
        ),
    ] {
        assert_eq!(state_color(&state), ColorKind::Yellow, "{state:?}");
        let segment = state_segment(&state, "example.com", 22);
        assert_eq!(segment.text, text);
        assert_eq!(segment.tooltip, tooltip);
    }
}

#[test]
fn connected_is_green_and_leaves_the_endpoint_to_its_segment() {
    assert_eq!(state_color(&TransportState::Connected), ColorKind::Green);
    let segment = state_segment(&TransportState::Connected, "example.com", 2222);
    assert_eq!(segment.text, "Connected");
    assert_eq!(segment.tooltip, "Connected to example.com:2222");
    let segment = state_segment(&TransportState::Connected, "", 22);
    assert_eq!(segment.tooltip, "Connected");
}

#[test]
fn an_error_shows_its_whole_message_in_the_tooltip() {
    let message = "connection failed: refused".to_string();
    let state = TransportState::Error(message.clone());
    assert_eq!(state_color(&state), ColorKind::Red);
    let segment = state_segment(&state, "h", 22);
    assert_eq!(segment.text, message);
    assert_eq!(segment.tooltip, message);
}

#[test]
fn every_state_has_its_own_text() {
    let texts: std::collections::HashSet<String> = [
        TransportState::Disconnected,
        TransportState::Connecting,
        TransportState::VerifyingHostKey,
        TransportState::Authenticating,
        TransportState::Connected,
        TransportState::Reconnecting { attempt: 1 },
        TransportState::Error("e".to_string()),
    ]
    .iter()
    .map(|state| state_segment(state, "h", 22).text)
    .collect();
    assert_eq!(texts.len(), 7);
}

#[test]
fn no_endpoint_no_segment() {
    assert_eq!(endpoint_segment("", 22, Some(&ssh_session())), None);
}

#[test]
fn an_ssh_endpoint_lists_the_session_behind_it() {
    let segment = endpoint_segment("db.internal", 22, Some(&ssh_session())).unwrap();
    assert_eq!(segment.text, "db.internal:22");
    assert_eq!(
        segment.tooltip,
        "Session: prod\nType: SSH\nHost: db.internal\nPort: 22\nUser: alex\nJump host: bastion"
    );
}

#[test]
fn local_and_serial_endpoints_are_their_descriptions() {
    let local = SessionInfo {
        name: "Local shell".to_string(),
        kind: SessionType::LocalShell,
        user: Some("ignored".to_string()),
        jump: None,
    };
    let segment = endpoint_segment("/bin/zsh", 0, Some(&local)).unwrap();
    assert_eq!(segment.text, "/bin/zsh");
    assert_eq!(
        segment.tooltip,
        "Session: Local shell\nType: Local shell\nShell: /bin/zsh"
    );

    let serial = SessionInfo {
        name: "console".to_string(),
        kind: SessionType::Serial,
        ..SessionInfo::default()
    };
    let segment = endpoint_segment("/dev/ttyUSB0@115200", 0, Some(&serial)).unwrap();
    assert_eq!(segment.text, "/dev/ttyUSB0@115200");
    assert!(
        segment
            .tooltip
            .ends_with("Type: Serial\nLine: /dev/ttyUSB0@115200")
    );
}

#[test]
fn an_endpoint_without_a_session_shows_host_and_port() {
    let segment = endpoint_segment("vnc.lan", 5900, None).unwrap();
    assert_eq!(segment.text, "vnc.lan:5900");
    assert_eq!(segment.tooltip, "Host: vnc.lan\nPort: 5900");
}

#[test]
fn the_elapsed_tooltip_names_the_start() {
    let segment = elapsed_segment(754, "14:02:11");
    assert_eq!(segment.text, "12:34");
    assert_eq!(segment.tooltip, "Connected since 14:02:11");
    assert_eq!(elapsed_segment(0, "").tooltip, "Time connected");
}

#[test]
fn the_savings_tooltip_shows_the_bytes() {
    let segment = savings_segment(Savings {
        percent: 53,
        wire: 1024 * 1024,
        uncompressed: 2252 * 1024,
    });
    assert_eq!(segment.text, "Compression saves 53%");
    assert_eq!(
        segment.tooltip,
        "1.0 MB received, about 2.2 MB uncompressed\n\
         Counts the terminal, SFTP and forwards on this connection"
    );
}

#[test]
fn uptime_reads_in_the_two_largest_units() {
    assert_eq!(format_uptime(0), "0m");
    assert_eq!(format_uptime(59 * 60 + 59), "59m");
    assert_eq!(format_uptime(3600), "1h 0m");
    assert_eq!(format_uptime(5 * 3600 + 12 * 60), "5h 12m");
    assert_eq!(format_uptime(86_400), "1d 0h");
    assert_eq!(format_uptime(12 * 86_400 + 4 * 3600 + 7 * 60), "12d 4h");
}

#[test]
fn the_uptime_tooltip_spells_out_the_duration_and_boot() {
    let segment = uptime_segment(
        Host::Server,
        12 * 86_400 + 4 * 3600 + 7 * 60,
        "11/11/26 10:02:11",
    );
    assert_eq!(segment.text, "Up 12d 4h");
    assert_eq!(
        segment.tooltip,
        "Server up 12 days, 4 hours, 7 minutes\nBooted 11/11/26 10:02:11"
    );
    assert_eq!(
        uptime_segment(Host::Server, 86_400 + 60, "").tooltip,
        "Server up 1 day, 1 minute"
    );
    assert_eq!(
        uptime_segment(Host::ThisComputer, 30, "").tooltip,
        "This computer up less than a minute"
    );
}

#[test]
fn rates_are_bits_in_the_unit_that_fits() {
    assert_eq!(format_rate(0), "0 bit/s");
    assert_eq!(format_rate(100), "800 bit/s");
    assert_eq!(format_rate(124), "992 bit/s");
    assert_eq!(format_rate(125), "1.0 kbit/s");
    assert_eq!(format_rate(2048), "16.4 kbit/s");
    assert_eq!(format_rate(12_493), "99.9 kbit/s");
    assert_eq!(format_rate(12_494), "100 kbit/s");
    assert_eq!(format_rate(124_937), "999 kbit/s");
    assert_eq!(format_rate(124_938), "1.0 Mbit/s", "not 1000 kbit/s");
    assert_eq!(format_rate(125_000), "1.0 Mbit/s");
    assert_eq!(format_rate(31_250_000), "250 Mbit/s");
    assert_eq!(format_rate(125_000_000), "1.0 Gbit/s");
    assert_eq!(format_rate(1_250_000_000), "10.0 Gbit/s");
}

#[test]
fn the_bandwidth_segment_names_the_interface_and_whose_it_is() {
    let rate = Rate {
        interface: "ens3".to_string(),
        rx_per_sec: 187_500,
        tx_per_sec: 25,
    };
    let segment = bandwidth_segment(Host::Server, &rate);
    assert_eq!(segment.text, "↓ 1.5 Mbit/s ↑ 200 bit/s");
    assert_eq!(
        segment.tooltip,
        "Server network, ens3 (the default route)\n\
         Receiving 1.5 Mbit/s, sending 200 bit/s\n\
         All traffic on the interface, sampled every 2 s"
    );
    assert!(
        bandwidth_segment(Host::ThisComputer, &rate)
            .tooltip
            .starts_with("This computer network, ens3")
    );
}

#[test]
fn format_elapsed_seconds_and_minutes() {
    assert_eq!(format_elapsed(0), "0:00");
    assert_eq!(format_elapsed(1), "0:01");
    assert_eq!(format_elapsed(59), "0:59");
    assert_eq!(format_elapsed(60), "1:00");
    assert_eq!(format_elapsed(754), "12:34");
    assert_eq!(format_elapsed(3599), "59:59");
}

#[test]
fn format_elapsed_hours() {
    assert_eq!(format_elapsed(3600), "1:00:00");
    assert_eq!(format_elapsed(3661), "1:01:01");
    assert_eq!(format_elapsed(86_400), "24:00:00");
}

#[test]
fn color_kind_hex_colors_are_distinct() {
    let colors = [
        ColorKind::Green.hex(),
        ColorKind::Yellow.hex(),
        ColorKind::Red.hex(),
    ];
    let unique: std::collections::HashSet<&str> = colors.into_iter().collect();
    assert_eq!(unique.len(), 3);
    for color in unique {
        assert!(
            color.starts_with('#') && color.len() == 7,
            "{color} is not a #rrggbb colour"
        );
    }
}

#[test]
fn fullness_turns_amber_at_80_and_red_at_90() {
    assert_eq!(fullness_color(0), None);
    assert_eq!(fullness_color(79), None);
    assert_eq!(fullness_color(80), Some(FILLING));
    assert_eq!(fullness_color(89), Some(FILLING));
    assert_eq!(fullness_color(90), Some(FULL));
    assert_eq!(fullness_color(100), Some(FULL));
}

#[test]
fn the_cpu_segment_adds_cores_and_load_averages() {
    let averages = ["0.52", "0.48", "0.40"].map(str::to_string);
    let segment = cpu_segment(Host::Server, 12, 4, &averages);
    assert_eq!(segment.text, "CPU 12%");
    assert_eq!(
        segment.tooltip,
        "Server CPU 12% busy, 4 cores\n\
         Load average 0.52, 0.48, 0.40 (1, 5, 15 min)\n\
         Sampled every 2 s"
    );
    assert_eq!(segment.color, None, "only the disks warn");
    assert!(
        cpu_segment(Host::ThisComputer, 0, 1, &averages)
            .tooltip
            .starts_with("This computer CPU 0% busy, 1 core\n")
    );
}

#[test]
fn the_memory_segment_counts_what_is_not_available() {
    const GIB: u64 = 1 << 30;
    let mut memory = Memory {
        total: 8 * GIB,
        available: 5 * GIB,
        swap_total: 2 * GIB,
        swap_free: 2 * GIB - 512 * 1024 * 1024,
    };
    let segment = memory_segment(Host::Server, &memory).unwrap();
    assert_eq!(segment.text, "RAM 38%");
    assert_eq!(
        segment.tooltip,
        "Server memory: 3.0 GB used of 8.0 GB (38%)\n5.0 GB available\n\
         Swap: 512.0 MB used of 2.0 GB"
    );
    memory.swap_total = 0;
    assert!(
        memory_segment(Host::Server, &memory)
            .unwrap()
            .tooltip
            .ends_with("\nNo swap")
    );
    memory.total = 0;
    assert_eq!(memory_segment(Host::Server, &memory), None);
}

fn filesystem(mount: &str, size_gib: u64, used_gib: u64, available_gib: u64) -> Filesystem {
    const GIB: u64 = 1 << 30;
    Filesystem {
        mount: mount.to_string(),
        usage: Usage {
            size: size_gib * GIB,
            used: used_gib * GIB,
            available: available_gib * GIB,
        },
        pool: None,
    }
}

fn dataset(mount: &str, used_gib: u64, available_gib: u64) -> Filesystem {
    Filesystem {
        pool: Some("tank".to_string()),
        ..filesystem(mount, used_gib + available_gib, used_gib, available_gib)
    }
}

#[test]
fn the_disk_segment_totals_and_colours_by_the_fullest() {
    let filesystems = [
        filesystem("/", 45, 20, 25),
        filesystem("/boot", 2, 1, 1),
        filesystem("/var", 50, 46, 4),
    ];
    let segment = disk_segment(Host::Server, &filesystems, &[]).unwrap();
    assert_eq!(segment.text, "Disk 67.0 GB / 97.0 GB");
    assert_eq!(segment.color, Some(FULL), "/var is 92% full; the total 70%");
    assert_eq!(
        segment.tooltip,
        "Server filesystems\n\
         /       45%  20.0 GB used, 25.0 GB free of 45.0 GB\n\
         /boot   50%  1.0 GB used, 1.0 GB free of 2.0 GB\n\
         /var    92%  46.0 GB used, 4.0 GB free of 50.0 GB\n\
         Total   70%  67.0 GB used, 30.0 GB free of 97.0 GB\n\
         Sampled every 30 s"
    );
    assert_eq!(
        segment.tooltip_markup.as_deref(),
        Some(
            "Server filesystems\n\
             <tt>/       45%  20.0 GB used, 25.0 GB free of 45.0 GB</tt>\n\
             <tt>/boot   50%  1.0 GB used, 1.0 GB free of 2.0 GB</tt>\n\
             <tt><span foreground=\"#e0443e\">/var    92%  46.0 GB used, 4.0 GB free of 50.0 GB</span></tt>\n\
             <tt>Total   70%  67.0 GB used, 30.0 GB free of 97.0 GB</tt>\n\
             Sampled every 30 s"
        )
    );
}

#[test]
fn one_filesystem_needs_no_total_and_room_needs_no_colour() {
    let segment = disk_segment(Host::ThisComputer, &[filesystem("/", 45, 20, 25)], &[]).unwrap();
    assert_eq!(segment.color, None);
    assert_eq!(
        segment.tooltip,
        "This computer filesystems\n\
         /   45%  20.0 GB used, 25.0 GB free of 45.0 GB\n\
         Sampled every 30 s"
    );
    let filling = disk_segment(Host::Server, &[filesystem("/", 100, 85, 15)], &[]).unwrap();
    assert_eq!(filling.color, Some(FILLING));
    assert_eq!(disk_segment(Host::Server, &[], &[]), None);
}

#[test]
fn long_mount_points_lose_their_middle() {
    assert_eq!(shorten("/srv/data"), "/srv/data");
    let long = "/mnt/backups/2026/november/weekly";
    let short = shorten(long);
    assert_eq!(short.chars().count(), 24);
    assert_eq!(short, "/mnt/backup…ember/weekly");
}

#[test]
fn markup_in_a_mount_point_is_escaped() {
    let segment = disk_segment(Host::Server, &[filesystem("/mnt/a<b>&c", 10, 1, 9)], &[]).unwrap();
    assert!(
        segment
            .tooltip_markup
            .unwrap()
            .contains("<tt>/mnt/a&lt;b&gt;&amp;c")
    );
}

#[test]
fn a_stale_reading_says_so_in_the_tooltip() {
    let stale = [
        StaleDisks {
            group: Group::Network,
            last_read: Some("14:02:11".to_string()),
        },
        StaleDisks {
            group: Group::Local,
            last_read: None,
        },
    ];
    let segment = disk_segment(Host::Server, &[filesystem("/", 45, 20, 25)], &stale).unwrap();
    let lines: Vec<&str> = segment.tooltip.lines().collect();
    assert_eq!(
        lines[lines.len() - 3..],
        [
            "Network filesystems not updated since 14:02:11: df has not finished (a hung mount?)",
            "Local filesystems not read yet: df has not finished (a hung mount?)",
            "Sampled every 30 s",
        ]
    );
    assert!(segment.text.starts_with("Disk "), "the bar is unchanged");
    assert_eq!(segment.tooltip_markup.unwrap().lines().count(), lines.len());
}

#[test]
fn a_zfs_pool_counts_once_and_colours_its_datasets() {
    let filesystems = [
        dataset("/", 50, 600),
        filesystem("/boot", 2, 1, 1),
        dataset("/home", 300, 600),
        dataset("/var", 50, 400),
    ];
    let segment = disk_segment(Host::Server, &filesystems, &[]).unwrap();
    assert_eq!(
        segment.text, "Disk 401.0 GB / 1002.0 GB",
        "the pool's 600 GB free counted once"
    );
    assert_eq!(segment.color, None);
    assert_eq!(
        segment.tooltip,
        "Server filesystems\n\
         /       40%  50.0 GB used, 600.0 GB free in pool tank\n\
         /boot   50%  1.0 GB used, 1.0 GB free of 2.0 GB\n\
         /home   40%  300.0 GB used, 600.0 GB free in pool tank\n\
         /var    40%  50.0 GB used, 600.0 GB free in pool tank\n\
         Total   41%  401.0 GB used, 601.0 GB free of 1002.0 GB\n\
         Sampled every 30 s"
    );

    let full = [dataset("/", 50, 50), dataset("/home", 900, 50)];
    let segment = disk_segment(Host::Server, &full, &[]).unwrap();
    assert_eq!(segment.color, Some(FULL), "the pool is 95% full");
    assert!(
        segment
            .tooltip_markup
            .unwrap()
            .contains("<span foreground=\"#e0443e\">/       95%  50.0 GB used"),
        "a small dataset in a full pool warns too"
    );
}

fn connected_tab() -> TabStatus {
    TabStatus {
        state: TransportState::Connected,
        host: "db".to_string(),
        port: 22,
        session: Some(ssh_session()),
        since: Some(ConnectedSince::now()),
    }
}

fn shown(tab: &TabStatus, live: &LiveStatus) -> Vec<Item> {
    Item::LEFT
        .into_iter()
        .chain(Item::RIGHT)
        .filter(|item| item.segment(tab, live).is_some())
        .collect()
}

#[test]
fn each_item_waits_for_its_figure() {
    let tab = connected_tab();
    let mut live = LiveStatus::default();
    assert_eq!(
        shown(&tab, &live),
        [Item::State, Item::Endpoint, Item::Elapsed]
    );
    let mut host = HostFigures::new(Host::Server);
    host.cpu = Some(CpuUse {
        busy: None,
        cores: 4,
        load_average: ["0.1", "0.2", "0.3"].map(str::to_string),
    });
    host.memory = Some(Memory {
        total: 8 << 30,
        available: 4 << 30,
        swap_total: 0,
        swap_free: 0,
    });
    host.uptime = Some(Uptime::measured(60));
    live.host = Some(host.clone());
    assert_eq!(
        shown(&tab, &live),
        [
            Item::State,
            Item::Endpoint,
            Item::Memory,
            Item::Uptime,
            Item::Elapsed
        ],
        "the CPU waits for a second sample"
    );
    host.cpu.as_mut().unwrap().busy = Some(5);
    host.disks = Some(DiskReading {
        filesystems: vec![filesystem("/", 45, 20, 25)],
        stale: Vec::new(),
    });
    host.bandwidth = Some(Rate {
        interface: "eth0".to_string(),
        rx_per_sec: 1,
        tx_per_sec: 1,
    });
    live.host = Some(host);
    live.savings = Some(Savings {
        percent: 10,
        wire: 90,
        uncompressed: 100,
    });
    assert_eq!(
        shown(&tab, &live),
        Item::LEFT
            .into_iter()
            .chain(Item::RIGHT)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        Item::Cpu.segment(&tab, &live).unwrap().tooltip,
        cpu_segment(
            Host::Server,
            5,
            4,
            &["0.1", "0.2", "0.3"].map(str::to_string)
        )
        .tooltip,
    );
}

#[test]
fn only_a_connected_tab_counts_its_time() {
    let tab = TabStatus {
        state: TransportState::Disconnected,
        since: None,
        ..connected_tab()
    };
    assert_eq!(
        shown(&tab, &LiveStatus::default()),
        [Item::State, Item::Endpoint]
    );
}
