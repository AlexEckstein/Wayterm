//! Widget checks; they need a display and run from `ui::gtk_tests`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use super::*;
use crate::store::model::SessionType;
use crate::transport::bandwidth::Rate;
use crate::transport::disks::{Filesystem, Usage};
use crate::transport::load::Memory;
use crate::transport::traffic::Savings;

fn part_of(bar: &StatusBar, item: Item) -> &Part {
    bar.left
        .iter()
        .chain(&bar.right)
        .find(|(of, _)| *of == item)
        .map(|(_, part)| part)
        .expect("every item has a part")
}

fn tooltip(part: &Part) -> String {
    part.area
        .tooltip_text()
        .map(String::from)
        .unwrap_or_default()
}

fn shown(part: &Part) -> bool {
    part.area.is_visible()
}

pub(in crate::ui) fn segments_follow_the_state_with_tooltips() {
    let bar = StatusBar::new();
    assert_eq!(part_of(&bar, Item::State).label.text(), "Disconnected");
    assert!(
        !shown(part_of(&bar, Item::Endpoint))
            && !part_of(&bar, Item::Endpoint).separator.is_visible()
    );
    assert!(!shown(part_of(&bar, Item::Elapsed)) && !shown(part_of(&bar, Item::Savings)));

    let saving = Savings {
        percent: 40,
        wire: 60 * 1024,
        uncompressed: 100 * 1024,
    };
    let uptime = Uptime {
        secs: 2 * 86_400 + 3 * 3600,
        at: Instant::now(),
        boot_clock: "11/21/26 07:00:00".to_string(),
    };
    let host = HostFigures {
        of: Host::Server,
        uptime: Some(uptime),
        bandwidth: Some(Rate {
            interface: "eth0".to_string(),
            rx_per_sec: 2048,
            tx_per_sec: 10,
        }),
        load: Some(Load {
            busy: Some(12),
            cores: 4,
            load_average: ["0.52", "0.48", "0.40"].map(str::to_string),
            memory: Memory {
                total: 8 << 30,
                available: 5 << 30,
                swap_total: 0,
                swap_free: 0,
            },
        }),
        disks: Some(DiskReading {
            filesystems: vec![
                Filesystem {
                    mount: "/".to_string(),
                    usage: Usage {
                        size: 45 << 30,
                        used: 20 << 30,
                        available: 25 << 30,
                    },
                    pool: None,
                },
                Filesystem {
                    mount: "/var".to_string(),
                    usage: Usage {
                        size: 50 << 30,
                        used: 46 << 30,
                        available: 4 << 30,
                    },
                    pool: None,
                },
            ],
            stale: Vec::new(),
        }),
    };
    let live = LiveStatus {
        savings: Some(saving),
        host: Some(host),
    };
    bar.set_live_source(Some(Rc::new(move || live.clone())));
    let session = SessionInfo {
        name: "prod".to_string(),
        kind: SessionType::SSH,
        user: Some("alex".to_string()),
        jump: None,
    };
    let since = ConnectedSince {
        at: Instant::now() - Duration::from_secs(75),
        clock: "14:02:11".to_string(),
    };
    bar.set_state(
        &TransportState::Connected,
        "db",
        22,
        Some(&since),
        Some(&session),
    );
    assert_eq!(part_of(&bar, Item::State).label.text(), "Connected");
    assert_eq!(tooltip(part_of(&bar, Item::State)), "Connected to db:22");
    assert!(
        shown(part_of(&bar, Item::Endpoint))
            && part_of(&bar, Item::Endpoint).separator.is_visible()
    );
    assert_eq!(part_of(&bar, Item::Endpoint).label.text(), "db:22");
    assert!(tooltip(part_of(&bar, Item::Endpoint)).contains("User: alex"));
    assert_eq!(
        part_of(&bar, Item::Elapsed).label.text(),
        "1:15",
        "counted from `since`"
    );
    assert_eq!(
        tooltip(part_of(&bar, Item::Elapsed)),
        "Connected since 14:02:11"
    );
    assert_eq!(
        part_of(&bar, Item::Savings).label.text(),
        "Compression saves 40%"
    );
    assert_eq!(part_of(&bar, Item::Uptime).label.text(), "Up 2d 3h");
    assert_eq!(
        tooltip(part_of(&bar, Item::Uptime)),
        "Server up 2 days, 3 hours\nBooted 11/21/26 07:00:00"
    );
    assert_eq!(
        part_of(&bar, Item::Bandwidth).label.text(),
        "↓ 16.4 kbit/s ↑ 80 bit/s"
    );
    assert!(tooltip(part_of(&bar, Item::Bandwidth)).starts_with("Server network, eth0"));
    assert_eq!(part_of(&bar, Item::Cpu).label.text(), "CPU 12%");
    assert!(
        !part_of(&bar, Item::Cpu).separator.is_visible(),
        "first on its side"
    );
    assert_eq!(part_of(&bar, Item::Memory).label.text(), "RAM 38%");
    assert!(tooltip(part_of(&bar, Item::Memory)).starts_with("Server memory: 3.0 GB used"));
    assert_eq!(
        part_of(&bar, Item::Disk).label.text(),
        "Disk 66.0 GB / 95.0 GB"
    );
    assert!(
        part_of(&bar, Item::Disk)
            .label
            .label()
            .contains("foreground=\"#e0443e\""),
        "/var is 92% full"
    );
    assert!(
        tooltip(part_of(&bar, Item::Disk)).contains("/var    92%  46.0 GB used"),
        "the markup's text: {}",
        tooltip(part_of(&bar, Item::Disk))
    );
    assert!(
        part_of(&bar, Item::Memory).separator.is_visible()
            && part_of(&bar, Item::Disk).separator.is_visible()
    );
    assert!(part_of(&bar, Item::Bandwidth).separator.is_visible());
    assert!(part_of(&bar, Item::Uptime).separator.is_visible());
    assert!(
        part_of(&bar, Item::Elapsed).separator.is_visible()
            && part_of(&bar, Item::Savings).separator.is_visible()
    );
    assert!(bar.timer.borrow().is_some());

    bar.set_live_source(Some(Rc::new(LiveStatus::default)));
    bar.set_state(
        &TransportState::Connected,
        "db",
        22,
        Some(&since),
        Some(&session),
    );
    assert!(
        !shown(part_of(&bar, Item::Bandwidth))
            && !shown(part_of(&bar, Item::Uptime))
            && !shown(part_of(&bar, Item::Savings))
    );
    assert!(
        !shown(part_of(&bar, Item::Cpu))
            && !shown(part_of(&bar, Item::Memory))
            && !shown(part_of(&bar, Item::Disk))
    );
    assert!(
        !part_of(&bar, Item::Elapsed).separator.is_visible(),
        "no separator before the first shown segment"
    );

    bar.set_state(
        &TransportState::Reconnecting { attempt: 2 },
        "db",
        22,
        None,
        Some(&session),
    );
    assert_eq!(
        part_of(&bar, Item::State).label.text(),
        "Reconnecting (attempt 2)…"
    );
    assert!(shown(part_of(&bar, Item::Endpoint)));
    assert!(
        !shown(part_of(&bar, Item::Uptime))
            && !shown(part_of(&bar, Item::Elapsed))
            && !shown(part_of(&bar, Item::Savings))
    );
    assert!(!part_of(&bar, Item::Savings).separator.is_visible());
    assert!(bar.timer.borrow().is_none());
}
