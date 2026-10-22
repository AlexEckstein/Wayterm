//! The session data model (doc 09). Serde spellings are the database
//! contract, locked by the tests — changing one is a schema break.

use serde::{Deserialize, Serialize};

/// `sessions.type`: `ssh`, `rdp`, `vnc`, `serial`, `local`.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionType {
    /// `ssh`.
    #[default]
    #[serde(rename = "ssh")]
    SSH,
    /// `rdp`.
    #[serde(rename = "rdp")]
    RDP,
    /// `vnc`.
    #[serde(rename = "vnc")]
    VNC,
    /// `serial`.
    #[serde(rename = "serial")]
    Serial,
    /// `local`: a local shell.
    #[serde(rename = "local")]
    LocalShell,
}

/// What each protocol supports. Every match is exhaustive, so a new type
/// must answer each question before it compiles.
impl SessionType {
    /// The protocol's registered port; serial and local sessions have none.
    pub fn default_port(&self) -> Option<u16> {
        match self {
            Self::SSH => Some(22),
            Self::RDP => Some(3389),
            Self::VNC => Some(5900),
            Self::Serial | Self::LocalShell => None,
        }
    }

    /// Only SSH sessions get an SFTP sidebar (doc 07 §Availability).
    pub fn supports_sftp(&self) -> bool {
        match self {
            Self::SSH => true,
            Self::RDP | Self::VNC | Self::Serial | Self::LocalShell => false,
        }
    }

    /// Only an SSH session can be another's jump host (doc 04 §Jump hosts).
    pub fn can_be_jump_host(&self) -> bool {
        match self {
            Self::SSH => true,
            Self::RDP | Self::VNC | Self::Serial | Self::LocalShell => false,
        }
    }

    /// Whether the session dialog offers to save a typed password to the
    /// keyring (doc 05, "Store on save").
    pub fn stores_password_on_save(&self) -> bool {
        match self {
            Self::SSH => true,
            Self::RDP | Self::VNC | Self::Serial | Self::LocalShell => false,
        }
    }
}

/// `sessions.auth_method`: `password`, `key`, `agent`,
/// `keyboard-interactive`, `none`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// `password`.
    Password,
    /// `key`: a private key file.
    Key,
    /// `agent`: `SSH_AUTH_SOCK`.
    Agent,
    /// `keyboard-interactive`.
    #[serde(rename = "keyboard-interactive")]
    KeyboardInteractive,
    /// `none`.
    None,
}

/// `sessions.host_key_policy` (doc 04): `strict`, `accept-new`, `off`;
/// NULL means the global setting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyPolicy {
    /// Unknown → prompt; changed → hard fail.
    Strict,
    /// Unknown → record silently; changed → hard fail.
    #[serde(rename = "accept-new")]
    AcceptNew,
    /// No verification (lab use only).
    Off,
}

/// Port-forward direction (doc 04 §Port forwarding). Only `Local` runs;
/// all three are in the wire format so the JSON stays stable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// `local`: a port here to a host the server reaches (`ssh -L`).
    Local,
    /// `remote`: stored, never started.
    Remote,
    /// `dynamic` (SOCKS): stored, never started.
    Dynamic,
}

/// One entry of the `sessions.forward_rules` JSON array (doc 04).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForwardRule {
    /// Direction.
    pub kind: ForwardKind,
    /// Bind address; empty means loopback.
    pub bind_addr: String,
    /// Bind port.
    pub bind_port: u16,
    /// Destination host, as the server resolves it.
    pub remote_host: String,
    /// Destination port.
    pub remote_port: u16,
    /// Start at connect.
    pub enabled: bool,
}

/// The `sessions.ssh_options` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SshOptions {
    /// Stored, not applied.
    pub compression: bool,
    /// Stored, not applied.
    pub keepalive_interval: Option<u32>,
    /// Stored, not applied.
    pub keepalive_count: Option<u32>,
    /// Run `ssh -W` as the jump stream when the jump hop rejects
    /// direct-tcpip (doc 04 §Jump hosts). Off by default; rows written
    /// before this field existed load as `false`.
    #[serde(default)]
    pub ssh_w_fallback: bool,
}

/// The `sessions.rdp_options` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RdpOptions {
    /// Redirect audio.
    pub audio: bool,
    /// Share the clipboard.
    pub clipboard: bool,
    /// Smooth fonts.
    pub fonts: bool,
    /// Redirect printers.
    pub printer: bool,
}

/// The `sessions.vnc_options` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VncOptions {
    /// 0-9.
    pub quality: u8,
    /// Fit the tab instead of 1:1.
    pub scaling: bool,
    /// Send no input.
    pub view_only: bool,
}

/// Prefix of every Wayterm tmux session on the remote host, so they never
/// collide with the user's own (doc 04 §Tmux session persistence).
/// `SSHTransport` adds it; the stored name is the bare suffix.
pub const TMUX_NAME_PREFIX: &str = "wayterm-";

/// A saved session — one `sessions` row (doc 09). Never holds a secret:
/// `auth_method` says which method, the keyring holds the credential.
/// `#[serde(default)]` keeps JSON written before a field existed readable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Row id; 0 before the first save.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// Protocol.
    pub session_type: SessionType,
    /// Containing folder; `None` = root.
    pub folder_id: Option<i64>,

    // Common connection fields.
    /// Host name or address.
    pub host: Option<String>,
    /// `None` = the type's default port.
    pub port: Option<u16>,
    /// Login user.
    pub username: Option<String>,
    /// SSH auth method; `None` = password.
    pub auth_method: Option<AuthMethod>,

    // SSH.
    /// Private key for the `key` method.
    pub key_path: Option<String>,
    /// `None` = the global setting.
    pub host_key_policy: Option<HostKeyPolicy>,
    /// ProxyJump-style hop through another session (doc 04; depth ≤ 1).
    pub jump_session_id: Option<i64>,
    /// Port-forward rules (SSH).
    pub forward_rules: Option<Vec<ForwardRule>>,
    /// Run the shell inside a remote tmux session (doc 04). Stored for
    /// every type, read only by the SSH transport.
    pub tmux_enabled: bool,
    /// Bare tmux name (no [`TMUX_NAME_PREFIX`]); `None` = the session name.
    pub tmux_session_name: Option<String>,
    /// SSH options.
    pub ssh_options: Option<SshOptions>,

    // RDP.
    /// Windows domain.
    pub rdp_domain: Option<String>,
    /// Remote desktop width; `None` = 1024.
    pub rdp_width: Option<u32>,
    /// Remote desktop height; `None` = 768.
    pub rdp_height: Option<u32>,
    /// Color depth: 16, 24 or 32; `None` = 32.
    pub rdp_color_depth: Option<u32>,
    /// Compress the stream.
    pub rdp_compression: bool,
    /// Clipboard, fonts, audio, printer.
    pub rdp_options: Option<RdpOptions>,

    // VNC.
    /// Stored, not applied.
    pub vnc_color_depth: Option<u32>,
    /// Scaling, view-only, quality.
    pub vnc_options: Option<VncOptions>,

    // Serial.
    /// Device path.
    pub serial_device: Option<String>,
    /// Line speed.
    pub serial_baud_rate: Option<u32>,
    /// 5–8.
    pub serial_data_bits: Option<u8>,
    /// `none`, `odd`, `even`, `mark`, `space`.
    pub serial_parity: Option<String>,
    /// 1–2.
    pub serial_stop_bits: Option<u8>,
    /// `none`, `hardware`, `software`.
    pub serial_flow_control: Option<String>,

    // Local shell.
    /// Shell to run; `None` = `$SHELL`.
    pub local_shell: Option<String>,
    /// Starting directory; `None` = Wayterm's.
    pub local_working_dir: Option<String>,

    // Terminal appearance (per-session override, empty = use global).
    /// `terminal_backend`: stored, not read.
    pub preferred_backend: Option<String>,
    /// `System`, `Light` or `Dark`; `None` = the global default.
    pub color_scheme: Option<String>,
    /// Stored, not applied.
    pub font_family: Option<String>,
    /// Stored, not applied.
    pub font_size: Option<f64>,
    /// Stored, not applied.
    pub opacity: Option<f64>,
    /// Icon name (themed or a custom SVG's); `None` = the type's default.
    /// Left out of the JSON when unset, so the wire format of a session
    /// without one is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,

    // Metadata.
    /// Stored; nothing updates it.
    pub last_connected: Option<String>,
    /// Stored; nothing updates it.
    pub connect_count: u32,
}

impl SessionConfig {
    /// The bare tmux name to hand `SSHTransport`, or `None` when tmux is
    /// off. A blank stored name falls back to the session name.
    pub fn tmux_session_suffix(&self) -> Option<String> {
        if !self.tmux_enabled {
            return None;
        }
        let stored = self
            .tmux_session_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        Some(match stored {
            Some(name) => strip_tmux_prefix(name).to_string(),
            None => self.name.trim().to_string(),
        })
    }

    /// The full remote name (`wayterm-<suffix>`), as `tmux list-sessions`
    /// shows it; `None` when tmux is off.
    pub fn tmux_remote_name(&self) -> Option<String> {
        self.tmux_session_suffix()
            .map(|suffix| format!("{}{}", TMUX_NAME_PREFIX, strip_tmux_prefix(&suffix)))
    }

    /// What the dialog's tmux entry shows, ticked or not: the full remote
    /// name that would be used.
    pub fn tmux_name_for_dialog(&self) -> String {
        self.tmux_remote_name()
            .unwrap_or_else(|| format!("{}{}", TMUX_NAME_PREFIX, self.name.trim()))
    }
}

/// Drop one leading [`TMUX_NAME_PREFIX`] (the dialog shows full names; the
/// row stores the bare suffix).
pub fn strip_tmux_prefix(name: &str) -> &str {
    name.strip_prefix(TMUX_NAME_PREFIX).unwrap_or(name)
}

/// One `folders` row (doc 09).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Folder {
    /// Row id.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// Parent folder; `None` = root.
    pub parent_id: Option<i64>,
    /// Order among siblings.
    pub sort_order: i64,
    /// Icon name (themed or a custom SVG's); `None` = the default.
    pub icon: Option<String>,
}

// Value vocabularies of the enum-like per-type columns. Doc 09 names only
// the defaults; these lists are the single source for the dialog and the
// mapper. Adding a value changes the wire vocabulary.

/// Stored `serial_parity` values.
pub const SERIAL_PARITIES: [&str; 5] = ["none", "odd", "even", "mark", "space"];
/// Parity for a NULL row.
pub const SERIAL_PARITY_DEFAULT: &str = "none";
/// Stored `serial_flow_control` values.
pub const SERIAL_FLOW_CONTROLS: [&str; 3] = ["none", "hardware", "software"];
/// Flow control for a NULL row.
pub const SERIAL_FLOW_CONTROL_DEFAULT: &str = "none";
/// Selectable data bits.
pub const SERIAL_DATA_BITS: [u8; 4] = [5, 6, 7, 8];
/// Data bits for a NULL row.
pub const SERIAL_DATA_BITS_DEFAULT: u8 = 8;
/// Selectable stop bits.
pub const SERIAL_STOP_BITS: [u8; 2] = [1, 2];
/// Stop bits for a NULL row.
pub const SERIAL_STOP_BITS_DEFAULT: u8 = 1;
/// The serial group's starting baud rate (the schema names no default).
pub const DEFAULT_SERIAL_BAUD: u32 = 115_200;
/// Bits per pixel; doc 08 falls back to 32.
pub const RDP_COLOR_DEPTHS: [u32; 3] = [16, 24, 32];
/// `/bpp` for a NULL `rdp_color_depth`.
pub const RDP_COLOR_DEPTH_DEFAULT: u32 = 32;

/// DropDown index → stored value; out of range falls back to `default`.
pub fn vocab_value<T: Copy + PartialEq>(values: &[T], index: u32, default: T) -> T {
    values.get(index as usize).copied().unwrap_or(default)
}

/// Stored value → DropDown index. A missing or unknown value selects the
/// default, so saving normalises it.
pub fn vocab_index<T: Copy + PartialEq>(values: &[T], value: Option<T>, default: T) -> u32 {
    match value.and_then(|v| values.iter().position(|c| *c == v)) {
        Some(pos) => pos as u32,
        None => values.iter().position(|c| *c == default).unwrap_or(0) as u32,
    }
}

#[cfg(test)]
mod tests;
