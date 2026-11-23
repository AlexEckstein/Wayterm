use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

/// `messages` data messages of `size` bytes each, after `handshake` bytes
/// of key exchange and auth, that cost `wire` bytes once authenticated.
fn measured(
    compressing: bool,
    handshake: usize,
    messages: usize,
    size: usize,
    wire: usize,
) -> Arc<Traffic> {
    let traffic = Traffic::new(compressing);
    traffic.add_wire(handshake);
    traffic.mark_authenticated();
    for _ in 0..messages {
        traffic.add_data(size, false);
    }
    traffic.add_wire(wire);
    traffic
}

#[test]
fn savings_compare_the_wire_with_the_uncompressed_estimate() {
    let plain = 100 * (1000 + DATA_OVERHEAD as usize);
    let half = measured(true, 5000, 100, 1000, plain / 2);
    assert_eq!(
        half.savings_percent(),
        Some(50),
        "the handshake is not counted"
    );
    let none = measured(true, 5000, 100, 1000, plain);
    assert_eq!(none.savings_percent(), Some(0));
    let worse = measured(true, 0, 100, 1000, plain * 2);
    assert_eq!(worse.savings_percent(), Some(0), "never below 0");
}

#[test]
fn savings_report_the_bytes_they_compare() {
    let plain = 100 * (1000 + DATA_OVERHEAD as usize);
    let savings = measured(true, 5000, 100, 1000, plain / 4).savings();
    assert_eq!(
        savings,
        Some(Savings {
            percent: 75,
            wire: (plain / 4) as u64,
            uncompressed: plain as u64,
        })
    );
}

#[test]
fn savings_need_compression_and_enough_traffic() {
    assert_eq!(measured(false, 0, 100, 1000, 1000).savings_percent(), None);
    let little = measured(true, 0, 1, 1000, 10);
    assert_eq!(little.savings_percent(), None);
    let traffic = Traffic::new(true);
    traffic.add_data(MIN_MEASURED as usize, true);
    assert!(traffic.savings_percent().is_some());
}

#[tokio::test]
async fn the_stream_counts_what_is_read_and_passes_writes_through() {
    let (near, mut far) = tokio::io::duplex(64);
    let traffic = Traffic::new(true);
    let mut stream = CountingStream::new(near, Arc::clone(&traffic));
    far.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
    stream.write_all(b"out").await.unwrap();
    let mut out = [0u8; 3];
    far.read_exact(&mut out).await.unwrap();
    assert_eq!(&out, b"out");
    assert_eq!(
        traffic.wire_in.load(Ordering::Relaxed),
        5,
        "writes are not counted"
    );
}
