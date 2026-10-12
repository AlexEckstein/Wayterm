use super::*;
use crate::store::model::{
    SessionType, SERIAL_DATA_BITS, SERIAL_FLOW_CONTROLS, SERIAL_PARITIES, SERIAL_STOP_BITS,
};

fn serial_config(device: &str) -> SessionConfig {
    SessionConfig {
        session_type: SessionType::Serial,
        serial_device: Some(device.into()),
        ..SessionConfig::default()
    }
}

fn drain_states(rx: &mut mpsc::Receiver<TransportEvent>) -> Vec<TransportState> {
    let mut states = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let TransportEvent::StateChanged(state) = event {
            states.push(state);
        }
    }
    states
}

#[test]
fn parity_vocabulary_maps_or_fails() {
    assert_eq!(map_parity("none").unwrap(), Parity::None);
    assert_eq!(map_parity("odd").unwrap(), Parity::Odd);
    assert_eq!(map_parity("even").unwrap(), Parity::Even);
    for value in ["mark", "space", "None", "", "bogus"] {
        assert!(
            matches!(map_parity(value), Err(TransportError::ConnectionFailed(_))),
            "{value}"
        );
    }
}

#[test]
fn flow_control_vocabulary_maps_or_fails() {
    assert_eq!(map_flow_control("none").unwrap(), FlowControl::None);
    assert_eq!(map_flow_control("hardware").unwrap(), FlowControl::Hardware);
    assert_eq!(map_flow_control("software").unwrap(), FlowControl::Software);
    for value in ["rts/cts", "xon", ""] {
        assert!(matches!(
            map_flow_control(value),
            Err(TransportError::ConnectionFailed(_))
        ));
    }
}

#[test]
fn data_and_stop_bits_map_or_fail() {
    let data: Vec<_> = SERIAL_DATA_BITS
        .iter()
        .map(|&b| map_data_bits(b).unwrap())
        .collect();
    assert_eq!(
        data,
        [
            DataBits::Five,
            DataBits::Six,
            DataBits::Seven,
            DataBits::Eight
        ]
    );
    let stop: Vec<_> = SERIAL_STOP_BITS
        .iter()
        .map(|&b| map_stop_bits(b).unwrap())
        .collect();
    assert_eq!(stop, [StopBits::One, StopBits::Two]);
    for bad in [0, 4, 9] {
        assert!(map_data_bits(bad).is_err());
    }
    for bad in [0, 3] {
        assert!(map_stop_bits(bad).is_err());
    }
}

#[test]
fn every_store_vocabulary_value_is_handled() {
    // Nothing in the store's lists may fall through to "unknown".
    for value in SERIAL_PARITIES {
        let message = match map_parity(value) {
            Ok(_) => continue,
            Err(e) => e.to_string(),
        };
        assert!(message.contains("not supported by this build"), "{value}");
    }
    for value in SERIAL_FLOW_CONTROLS {
        map_flow_control(value).unwrap();
    }
}

#[test]
fn settings_default_missing_fields_and_reject_bad_ones() {
    let settings = PortSettings::from_config(&serial_config("/dev/ttyUSB0")).unwrap();
    assert_eq!(
        settings,
        PortSettings {
            device: "/dev/ttyUSB0".into(),
            baud_rate: DEFAULT_SERIAL_BAUD,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        }
    );
    let mut config = serial_config("/dev/ttyUSB0");
    config.serial_parity = Some("weird".into());
    assert!(PortSettings::from_config(&config).is_err());
    config = SessionConfig::default();
    assert!(matches!(
        PortSettings::from_config(&config),
        Err(TransportError::ConnectionFailed(_))
    ));
}

#[test]
fn description_is_device_at_baud() {
    let (tx, _rx) = mpsc::channel(1);
    let mut config = serial_config("/dev/ttyUSB0");
    let transport = SerialTransport::new(&config, tx.clone());
    assert_eq!(transport.description(), "/dev/ttyUSB0@115200");
    config.serial_baud_rate = Some(9600);
    let transport = SerialTransport::new(&config, tx);
    assert_eq!(transport.description(), "/dev/ttyUSB0@9600");
    assert!(matches!(transport.transport_type(), TransportType::Serial));
}

#[tokio::test]
async fn missing_device_fails_cleanly() {
    let (tx, mut rx) = mpsc::channel(16);
    let mut transport = SerialTransport::new(&serial_config("/dev/wayterm-no-such-tty"), tx);
    let err = transport.connect().await.unwrap_err();
    assert!(
        matches!(err, TransportError::ConnectionFailed(ref m) if m.contains("/dev/wayterm-no-such-tty")),
        "{err:?}"
    );
    assert!(matches!(transport.state(), TransportState::Error(_)));
    let states = drain_states(&mut rx);
    assert_eq!(states[0], TransportState::Connecting);
    assert!(matches!(states[1], TransportState::Error(_)));
    assert!(matches!(
        transport.write(b"x").await,
        Err(TransportError::Disconnected)
    ));
}

#[tokio::test]
async fn unknown_vocabulary_fails_connect() {
    let (tx, _rx) = mpsc::channel(16);
    let mut config = serial_config("/dev/wayterm-no-such-tty");
    config.serial_flow_control = Some("rts".into());
    let mut transport = SerialTransport::new(&config, tx);
    let err = transport.connect().await.unwrap_err();
    assert!(
        matches!(err, TransportError::ConnectionFailed(ref m) if m.contains("flow control")),
        "{err:?}"
    );
}

#[tokio::test]
async fn disconnect_is_idempotent_and_resize_is_a_no_op() {
    let (tx, mut rx) = mpsc::channel(16);
    let mut transport = SerialTransport::new(&serial_config("/dev/wayterm-no-such-tty"), tx);
    transport.set_tmux_enabled(true, "ignored".into());
    assert_eq!(transport.state(), TransportState::Disconnected);
    transport.resize(24, 80).await.unwrap();
    transport.disconnect().await.unwrap();
    transport.disconnect().await.unwrap();
    assert_eq!(transport.state(), TransportState::Disconnected);
    assert!(drain_states(&mut rx).is_empty());

    let _ = transport.connect().await;
    transport.disconnect().await.unwrap();
    transport.disconnect().await.unwrap();
    assert_eq!(transport.state(), TransportState::Disconnected);
    assert_eq!(
        drain_states(&mut rx).last(),
        Some(&TransportState::Disconnected)
    );
}
