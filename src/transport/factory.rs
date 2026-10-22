//! [`create_transport`]: session type → byte-stream transport (doc 03
//! §Transport factory).

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::store::model::{SessionConfig, SessionType};
use crate::transport::connection::SshConnection;
use crate::transport::local::LocalTransport;
use crate::transport::serial::SerialTransport;
use crate::transport::ssh::SSHTransport;
use crate::transport::types::{Transport, TransportError, TransportEvent};

/// The transport for `config`: SSH needs `connection`, local and serial
/// build their own. RDP/VNC are errors, never panics, so a malformed row
/// cannot crash the app.
pub fn create_transport(
    config: &SessionConfig,
    connection: Option<Arc<SshConnection>>,
    event_tx: mpsc::Sender<TransportEvent>,
    initial_rows: u32,
    initial_cols: u32,
) -> Result<Box<dyn Transport>, TransportError> {
    match config.session_type {
        SessionType::SSH => {
            let connection =
                connection.ok_or_else(|| TransportError::ChannelError("no connection".into()))?;
            let mut transport = SSHTransport::new(connection, event_tx, initial_rows, initial_cols);
            if let Some(suffix) = config.tmux_session_suffix() {
                transport.set_tmux_enabled(true, suffix);
            }
            Ok(Box::new(transport))
        }
        SessionType::Serial => Ok(Box::new(SerialTransport::new(config, event_tx))),
        SessionType::LocalShell => Ok(Box::new(LocalTransport::new(
            config,
            event_tx,
            initial_rows,
            initial_cols,
        ))),
        SessionType::RDP | SessionType::VNC => Err(TransportError::ChannelError(
            "RDP/VNC don't use byte-stream transport (doc 08)".into(),
        )),
    }
}

#[cfg(test)]
mod tests;
