use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::store::model::{
    SessionConfig, DEFAULT_SERIAL_BAUD, SERIAL_DATA_BITS_DEFAULT, SERIAL_FLOW_CONTROL_DEFAULT,
    SERIAL_PARITY_DEFAULT, SERIAL_STOP_BITS_DEFAULT,
};
use crate::transport::types::{
    SharedState, Transport, TransportError, TransportEvent, TransportState, TransportType,
};

/// How long one blocking read waits before the reader re-checks its cancel
/// token. A timeout with no bytes is normal: serial TTYs never EOF.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// How long a write may stall (e.g. held off by flow control) in total.
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(5);
/// How long `disconnect` waits for the reader to notice its cancel token.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// The port settings of one session, mapped from the doc 06 vocabulary.
#[derive(Clone, Debug, PartialEq)]
struct PortSettings {
    device: String,
    baud_rate: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
}

impl PortSettings {
    /// Map a session row. Unknown or unsupported values are errors, never
    /// silently defaulted: a wrong frame format garbles the console.
    fn from_config(config: &SessionConfig) -> Result<Self, TransportError> {
        let device = config
            .serial_device
            .as_deref()
            .filter(|d| !d.is_empty())
            .ok_or_else(|| {
                TransportError::ConnectionFailed("no serial device configured".into())
            })?;
        Ok(Self {
            device: device.to_string(),
            baud_rate: config.serial_baud_rate.unwrap_or(DEFAULT_SERIAL_BAUD),
            data_bits: map_data_bits(config.serial_data_bits.unwrap_or(SERIAL_DATA_BITS_DEFAULT))?,
            parity: map_parity(
                config
                    .serial_parity
                    .as_deref()
                    .unwrap_or(SERIAL_PARITY_DEFAULT),
            )?,
            stop_bits: map_stop_bits(config.serial_stop_bits.unwrap_or(SERIAL_STOP_BITS_DEFAULT))?,
            flow_control: map_flow_control(
                config
                    .serial_flow_control
                    .as_deref()
                    .unwrap_or(SERIAL_FLOW_CONTROL_DEFAULT),
            )?,
        })
    }
}

fn unsupported(field: &str, value: impl std::fmt::Display) -> TransportError {
    TransportError::ConnectionFailed(format!("unsupported {field} '{value}'"))
}

/// `mark` and `space` are doc 06 vocabulary, but `serialport` 4.10 has no
/// such parity: they fail rather than open the port with the wrong frame.
fn map_parity(value: &str) -> Result<Parity, TransportError> {
    match value {
        "none" => Ok(Parity::None),
        "odd" => Ok(Parity::Odd),
        "even" => Ok(Parity::Even),
        "mark" | "space" => Err(TransportError::ConnectionFailed(format!(
            "serial parity '{value}' is not supported by this build"
        ))),
        other => Err(unsupported("serial parity", other)),
    }
}

fn map_flow_control(value: &str) -> Result<FlowControl, TransportError> {
    match value {
        "none" => Ok(FlowControl::None),
        "hardware" => Ok(FlowControl::Hardware),
        "software" => Ok(FlowControl::Software),
        other => Err(unsupported("serial flow control", other)),
    }
}

fn map_data_bits(value: u8) -> Result<DataBits, TransportError> {
    match value {
        5 => Ok(DataBits::Five),
        6 => Ok(DataBits::Six),
        7 => Ok(DataBits::Seven),
        8 => Ok(DataBits::Eight),
        other => Err(unsupported("serial data bits", other)),
    }
}

fn map_stop_bits(value: u8) -> Result<StopBits, TransportError> {
    match value {
        1 => Ok(StopBits::One),
        2 => Ok(StopBits::Two),
        other => Err(unsupported("serial stop bits", other)),
    }
}

/// Serial console transport over the `serialport` crate (doc 04 §Local and
/// serial transports). Reads block, so the reader runs on tokio's blocking
/// pool; `resize` is a no-op because a serial line has no window size.
pub struct SerialTransport {
    /// The row's port settings, or why it has none: mapped once, reported
    /// by every `connect`.
    settings: Result<PortSettings, String>,
    /// `device@baud`, as the row stores them.
    description: String,
    /// The write side; the reader task owns a cloned handle.
    port: Option<Arc<Mutex<Box<dyn SerialPort>>>>,
    reader_task: Option<JoinHandle<()>>,
    event_tx: mpsc::Sender<TransportEvent>,
    cancel_token: CancellationToken,
    /// Shared with the reader, which ends the connection when the device
    /// goes away.
    state: SharedState,
}

impl SerialTransport {
    pub fn new(config: &SessionConfig, event_tx: mpsc::Sender<TransportEvent>) -> Self {
        let settings = PortSettings::from_config(config).map_err(|err| match err {
            TransportError::ConnectionFailed(reason) => reason,
            other => other.to_string(),
        });
        let description = format!(
            "{}@{}",
            config.serial_device.as_deref().unwrap_or(""),
            config.serial_baud_rate.unwrap_or(DEFAULT_SERIAL_BAUD)
        );
        Self {
            settings,
            description,
            port: None,
            reader_task: None,
            state: SharedState::new(event_tx.clone()),
            event_tx,
            cancel_token: CancellationToken::new(),
        }
    }

    fn fail(&self, err: TransportError) -> TransportError {
        self.state.set(TransportState::Error(err.to_string()));
        err
    }

    fn open(settings: &PortSettings) -> Result<Box<dyn SerialPort>, TransportError> {
        serialport::new(settings.device.as_str(), settings.baud_rate)
            .data_bits(settings.data_bits)
            .parity(settings.parity)
            .stop_bits(settings.stop_bits)
            .flow_control(settings.flow_control)
            .timeout(READ_TIMEOUT)
            .open()
            .map_err(|e| TransportError::ConnectionFailed(format!("{}: {e}", settings.device)))
    }
}

/// The blocking read loop: every chunk becomes `DataReceived`; a read error
/// (device unplugged, permission revoked) is `ConnectionLost`, and the
/// transport is no longer connected — a later `connect` must not be
/// refused as "already connected".
fn read_loop(
    mut port: Box<dyn SerialPort>,
    state: SharedState,
    event_tx: mpsc::Sender<TransportEvent>,
    cancel: CancellationToken,
) {
    let mut buf = [0u8; 4096];
    while !cancel.is_cancelled() {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                if event_tx
                    .blocking_send(TransportEvent::DataReceived(buf[..n].to_vec()))
                    .is_err()
                {
                    return; // tab closed
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                if !cancel.is_cancelled() {
                    // Recorded before it is reported, so `state()` agrees
                    // with the event.
                    state.set(TransportState::Disconnected);
                    let _ = event_tx.blocking_send(TransportEvent::ConnectionLost);
                }
                return;
            }
        }
    }
}

/// Write all of `data`, looping over partial writes. Timeouts (flow control
/// holding us off) retry until [`WRITE_STALL_LIMIT`].
fn write_all(port: &mut dyn SerialPort, mut data: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + WRITE_STALL_LIMIT;
    while !data.is_empty() {
        match port.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::TimedOut && Instant::now() < deadline => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[async_trait]
impl Transport for SerialTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        if self.state() == TransportState::Connected {
            return Err(TransportError::ChannelError("already connected".into()));
        }
        self.state.set(TransportState::Connecting);

        let settings = match &self.settings {
            Ok(settings) => settings.clone(),
            Err(reason) => {
                let err = TransportError::ConnectionFailed(reason.clone());
                return Err(self.fail(err));
            }
        };
        // Opening configures termios and takes a flock: blocking syscalls.
        let opened = tokio::task::spawn_blocking(move || {
            let port = SerialTransport::open(&settings)?;
            let reader = port
                .try_clone()
                .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;
            Ok((port, reader))
        })
        .await
        .map_err(|e| TransportError::ConnectionFailed(e.to_string()))
        .and_then(|r| r);
        let (port, reader) = match opened {
            Ok(pair) => pair,
            Err(e) => return Err(self.fail(e)),
        };

        // A fresh token per connect: a reconnect must not inherit a cancel.
        self.cancel_token = CancellationToken::new();
        let (state, event_tx, cancel) = (
            self.state.clone(),
            self.event_tx.clone(),
            self.cancel_token.clone(),
        );
        self.reader_task = Some(tokio::task::spawn_blocking(move || {
            read_loop(reader, state, event_tx, cancel)
        }));
        self.port = Some(Arc::new(Mutex::new(port)));
        self.state.set(TransportState::Connected);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.cancel_token.cancel();
        // Blocking tasks cannot be aborted; the reader exits within one
        // READ_TIMEOUT, and joining it closes its port handle.
        if let Some(task) = self.reader_task.take() {
            let _ = tokio::time::timeout(READER_JOIN_TIMEOUT, task).await;
        }
        self.port = None;
        self.state.set(TransportState::Disconnected);
        Ok(())
    }

    fn state(&self) -> TransportState {
        self.state.get()
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let port = self.port.clone().ok_or(TransportError::Disconnected)?;
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut port = port.lock().unwrap_or_else(|e| e.into_inner());
            write_all(port.as_mut(), &data)
        })
        .await
        .map_err(|e| TransportError::ChannelError(e.to_string()))?
        .map_err(TransportError::IoError)
    }

    async fn resize(&mut self, _rows: u32, _cols: u32) -> Result<(), TransportError> {
        Ok(())
    }

    /// Accepted and ignored: a serial console is a device, not a shell host
    /// session, so there is no remote tmux to persist through.
    fn set_tmux_enabled(&mut self, _enabled: bool, _session_name: String) {}

    fn transport_type(&self) -> TransportType {
        TransportType::Serial
    }

    fn description(&self) -> String {
        self.description.clone()
    }
}

#[cfg(test)]
mod tests;
