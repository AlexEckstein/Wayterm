use std::time::Duration;

use super::*;

fn counters(rx: u64, tx: u64) -> Counters {
    Counters {
        interface: "ens3".to_string(),
        rx,
        tx,
    }
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
