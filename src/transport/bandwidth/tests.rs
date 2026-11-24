use std::io::Write;
use std::process::{Command, Stdio};

use super::*;

const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\n\
ens3\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n";

const DEV: &str = "Inter-|   Receive                            |  Transmit\n \
face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo: 9999 10 0 0 0 0 0 0 9999 10 0 0 0 0 0 0\n  \
ens3:123456 789 0 0 0 0 0 0 654321 456 0 0 0 0 0 0\n";

fn counters(rx: u64, tx: u64) -> Counters {
    Counters {
        interface: "ens3".to_string(),
        rx,
        tx,
    }
}

#[test]
fn the_default_route_names_the_interface() {
    assert_eq!(default_route_interface(ROUTE).as_deref(), Some("ens3"));
    let no_default = ROUTE.lines().take(2).collect::<Vec<_>>().join("\n");
    assert_eq!(default_route_interface(&no_default), None);
}

#[test]
fn dev_counters_are_received_and_sent_bytes() {
    assert_eq!(
        interface_counters(DEV, "ens3"),
        Some(counters(123456, 654321))
    );
    assert_eq!(interface_counters(DEV, "eth9"), None);
}

#[test]
fn samples_parse_only_when_marked_and_whole() {
    assert_eq!(
        parse_sample("wayterm-net:ens3 10 20\n"),
        Some(counters(10, 20))
    );
    assert_eq!(parse_sample("ens3 10 20"), None);
    assert_eq!(parse_sample("wayterm-net:ens3 10"), None);
    assert_eq!(parse_sample("wayterm-net:ens3 10 20 30"), None);
    assert_eq!(parse_sample("wayterm-net:ens3 x 20"), None);
}

#[test]
fn the_meter_needs_two_samples_and_skips_resets() {
    let mut meter = RateMeter::default();
    let start = Instant::now();
    assert_eq!(meter.add(counters(1000, 500), start), None);
    let rate = meter.add(counters(5000, 2500), start + Duration::from_secs(2));
    assert_eq!(
        rate,
        Some(Rate {
            interface: "ens3".to_string(),
            rx_per_sec: 2000,
            tx_per_sec: 1000,
        })
    );
    assert_eq!(
        meter.add(counters(10, 10), start + Duration::from_secs(4)),
        None,
        "a counter that went backwards"
    );
    let rate = meter.add(counters(410, 10), start + Duration::from_secs(6));
    assert_eq!(rate.map(|r| r.rx_per_sec), Some(200));
    let other = Counters {
        interface: "wlan0".to_string(),
        rx: 9999,
        tx: 9999,
    };
    assert_eq!(meter.add(other, start + Duration::from_secs(8)), None);
}

/// The script is POSIX `sh`; on this (Linux) machine its first sample is
/// this machine's default-route counters.
#[test]
fn the_script_samples_this_machine() {
    let Some(expected) = local_counters() else {
        return;
    };
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
    let text = String::from_utf8_lossy(&output.stdout);
    let sample = text.lines().find_map(parse_sample).expect("a sample");
    assert_eq!(sample.interface, expected.interface);
    assert!(sample.rx >= expected.rx && sample.tx >= expected.tx);
}

#[test]
fn the_script_sleeps_for_the_interval() {
    assert!(sample_script().contains(&format!("sleep {}\n", INTERVAL.as_secs())));
}
