//! The session data model (doc 06). Serde spellings are the database
//! contract, locked by the tests — changing one is a schema break.

use serde::{Deserialize, Serialize};

/// `sessions.type`: `ssh`, `rdp`, `vnc`, `serial`, `local`.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionType {
    #[default]
    #[serde(rename = "ssh")]
    SSH,
    #[serde(rename = "rdp")]
    RDP,
    #[serde(rename = "vnc")]
    VNC,
    #[serde(rename = "serial")]
    Serial,
    #[serde(rename = "local")]
    LocalShell,
}

/// `sessions.auth_method`: `password`, `key`, `agent`,
/// `keyboard-interactive`, `none`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    Key,
    Agent,
    #[serde(rename = "keyboard-interactive")]
    KeyboardInteractive,
    None,
}

/// `sessions.host_key_policy` (doc 11): `strict`, `accept-new`, `off`;
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

/// Port-forward direction (doc 11). Only `Local` is planned for v1; all
/// three are in the wire format so the JSON stays stable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    Local,
    Remote,
    Dynamic,
}

/// One entry of the `sessions.forward_rules` JSON array (doc 11).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForwardRule {
    pub kind: ForwardKind,
    pub bind_addr: String,
    pub bind_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub enabled: bool,
}

/// The `sessions.ssh_options` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SshOptions {
    pub compression: bool,
    pub keepalive_interval: Option<u32>,
    pub keepalive_count: Option<u32>,
}

/// The `sessions.rdp_options` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RdpOptions {
    pub audio: bool,
    pub clipboard: bool,
    pub fonts: bool,
    pub printer: bool,
}

/// The `sessions.vnc_options` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VncOptions {
    /// 0-9.
    pub quality: u8,
    pub scaling: bool,
    pub view_only: bool,
}

/// Prefix of every Wayterm tmux session on the remote host, so they never
/// collide with the user's own (doc 04 §Tmux Session Persistence).
/// `SSHTransport` adds it; the stored name is the bare suffix.
pub const TMUX_NAME_PREFIX: &str = "wayterm-";

/// A saved session — one `sessions` row (doc 06). Never holds a secret:
/// `auth_method` says which method, the keyring holds the credential.
/// `#[serde(default)]` keeps JSON written before a field existed readable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub id: i64,
    pub name: String,
    pub session_type: SessionType,
    pub folder_id: Option<i64>,

    // Common connection fields.
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth_method: Option<AuthMethod>,

    // SSH.
    pub key_path: Option<String>,
    /// `None` = the global setting.
    pub host_key_policy: Option<HostKeyPolicy>,
    /// ProxyJump-style hop through another session (doc 11; depth ≤ 1).
    pub jump_session_id: Option<i64>,
    pub forward_rules: Option<Vec<ForwardRule>>,
    /// Run the shell inside a remote tmux session (doc 04). Stored for
    /// every type, read only by the SSH transport.
    pub tmux_enabled: bool,
    /// Bare tmux name (no [`TMUX_NAME_PREFIX`]); `None` = the session name.
    pub tmux_session_name: Option<String>,
    pub ssh_options: Option<SshOptions>,

    // RDP.
    pub rdp_domain: Option<String>,
    pub rdp_width: Option<u32>,
    pub rdp_height: Option<u32>,
    pub rdp_color_depth: Option<u32>,
    pub rdp_compression: bool,
    pub rdp_options: Option<RdpOptions>,

    // VNC.
    pub vnc_color_depth: Option<u32>,
    pub vnc_options: Option<VncOptions>,

    // Serial.
    pub serial_device: Option<String>,
    pub serial_baud_rate: Option<u32>,
    pub serial_data_bits: Option<u8>,
    pub serial_parity: Option<String>,
    pub serial_stop_bits: Option<u8>,
    pub serial_flow_control: Option<String>,

    // Local shell.
    pub local_shell: Option<String>,
    pub local_working_dir: Option<String>,

    // Terminal appearance (per-session override, empty = use global).
    pub preferred_backend: Option<String>,
    pub color_scheme: Option<String>,
    pub font_family: Option<String>,
    pub font_size: Option<f64>,
    pub opacity: Option<f64>,

    // Metadata.
    pub notes: Option<String>,
    pub last_connected: Option<String>,
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

/// One `folders` row (doc 06).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub sort_order: i64,
    pub icon: Option<String>,
}

// Value vocabularies of the enum-like per-type columns. Doc 06 names only
// the defaults; these lists are the single source for the dialog and the
// mapper. Adding a value changes the wire vocabulary.

pub const SERIAL_PARITIES: [&str; 5] = ["none", "odd", "even", "mark", "space"];
pub const SERIAL_PARITY_DEFAULT: &str = "none";
pub const SERIAL_FLOW_CONTROLS: [&str; 3] = ["none", "hardware", "software"];
pub const SERIAL_FLOW_CONTROL_DEFAULT: &str = "none";
pub const SERIAL_DATA_BITS: [u8; 4] = [5, 6, 7, 8];
pub const SERIAL_DATA_BITS_DEFAULT: u8 = 8;
pub const SERIAL_STOP_BITS: [u8; 2] = [1, 2];
pub const SERIAL_STOP_BITS_DEFAULT: u8 = 1;
/// The serial group's starting baud rate (doc 06 names no default).
pub const DEFAULT_SERIAL_BAUD: u32 = 115_200;
/// Bits per pixel; doc 08 falls back to 32.
pub const RDP_COLOR_DEPTHS: [u32; 3] = [16, 24, 32];
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
