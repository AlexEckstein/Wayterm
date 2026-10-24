use tokio::sync::mpsc;

use super::*;
use crate::store::model::SessionConfig;
use crate::transport::types::TransportType;

#[test]
fn create_transport_rejects_what_it_cannot_build() {
    let (tx, _rx) = mpsc::channel(1);
    let error = |session_type| {
        let config = SessionConfig {
            session_type,
            ..SessionConfig::default()
        };
        match create_transport(&config, None, tx.clone(), 24, 80) {
            Err(TransportError::ChannelError(message)) => message,
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("must not build a transport"),
        }
    };
    assert_eq!(error(SessionType::SSH), "no connection");
    assert!(error(SessionType::RDP).contains("doc 08"));
    assert!(error(SessionType::VNC).contains("doc 08"));
    let serial = SessionConfig {
        session_type: SessionType::Serial,
        ..SessionConfig::default()
    };
    let transport = create_transport(&serial, None, tx.clone(), 24, 80).unwrap();
    assert!(matches!(transport.transport_type(), TransportType::Serial));
}

#[test]
fn create_transport_builds_a_local_shell() {
    let (tx, _rx) = mpsc::channel(1);
    let config = SessionConfig {
        session_type: SessionType::LocalShell,
        ..SessionConfig::default()
    };
    let transport = create_transport(&config, None, tx, 24, 80).expect("local transport");
    assert!(matches!(
        transport.transport_type(),
        TransportType::LocalPTY
    ));
}
