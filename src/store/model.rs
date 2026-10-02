//! Typed data model for saved sessions and folders (doc 06).
//!
//! Transcribed from docs/06-session-store.md §"Session Config Struct" —
//! field names, types, and serde wire formats are the database contract
//! and are locked by the tests in [`tests`]. The wire strings asserted
//! there come from doc 06's SQL column comments (`sessions.type`,
//! `sessions.auth_method`, `sessions.host_key_policy`, and the
//! `forward_rules` JSON example) — treat any change as a schema break.

use serde::{Deserialize, Serialize};

/// Protocol of a saved session.
///
/// Wires exactly to the `sessions.type` column vocabulary from doc 06's
/// schema comment: `ssh`, `rdp`, `vnc`, `serial`, `local`.
///
/// Variant names are verbatim from doc 06 §"Session Config Struct".
#[allow(clippy::upper_case_acronyms)] // SSH/RDP/VNC are the doc-06 spellings
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionType {
    #[default]
    // snake_case alone yields "s_s_h"; the wire string is doc 06's 'ssh'.
    #[serde(rename = "ssh")]
    SSH,
    #[serde(rename = "rdp")]
    RDP,
    #[serde(rename = "vnc")]
    VNC,
    #[serde(rename = "serial")]
    Serial,
    /// Wires as `local`, as doc 06's `sessions.type` comment specifies —
    /// not snake_case `local_shell`.
    #[serde(rename = "local")]
    LocalShell,
}

/// Authentication method for a session.
///
/// Wires exactly to the `sessions.auth_method` column vocabulary from
/// doc 06's schema comment: `password`, `key`, `agent`,
/// `keyboard-interactive`, `none`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    Key,
    Agent,
    /// Renamed so the wire format is `keyboard-interactive` with a
    /// hyphen, as doc 06's `sessions.auth_method` comment specifies —
    /// not snake_case `keyboard_interactive`. See doc 11 — not
    /// optional, 2FA/OTP needs it.
    #[serde(rename = "keyboard-interactive")]
    KeyboardInteractive,
    None,
}

/// Host-key verification policy (doc 11). None = use global setting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyPolicy {
    /// Unknown → prompt; changed → hard fail (default).
    Strict,
    /// Renamed so the wire format is `accept-new` with a hyphen, as doc
    /// 06's `sessions.host_key_policy` comment specifies (matching
    /// OpenSSH's `StrictHostKeyChecking=accept-new`). Unknown →
    /// auto-accept + record; changed → hard fail.
    #[serde(rename = "accept-new")]
    AcceptNew,
    /// No verification — lab/testing only, warning badge.
    Off,
}

/// Direction of a port-forward rule (doc 11 §Port forwarding).
///
/// All three variants wire from day one (`local`, `remote`, `dynamic`)
/// so the `forward_rules` JSON column format is stable when v2 adds
/// Remote and Dynamic support.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// Local forward — the only kind wired in v1.
    Local,
    /// Remote forward (`tcpip-forward`) — v2 (doc 11).
    Remote,
    /// Dynamic SOCKS forward — v2 (doc 11).
    Dynamic,
}

/// One port-forward rule (doc 11 §Port forwarding).
///
/// Serialized into the `sessions.forward_rules` JSON column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForwardRule {
    /// Local (v1); Remote/Dynamic in v2.
    pub kind: ForwardKind,
    /// e.g. `"127.0.0.1"`.
    pub bind_addr: String,
    pub bind_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub enabled: bool,
}

/// SSH tuning knobs — serialized into the `sessions.ssh_options` JSON
/// column (doc 06: "compression, keepalive, etc.").
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SshOptions {
    pub compression: bool,
    pub keepalive_interval: Option<u32>,
    pub keepalive_count: Option<u32>,
}

/// RDP peripheral redirections — serialized into the
/// `sessions.rdp_options` JSON column (doc 06: "audio, clipboard,
/// fonts, etc.").
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RdpOptions {
    pub audio: bool,
    pub clipboard: bool,
    pub fonts: bool,
    pub printer: bool,
}

/// VNC connection options — serialized into the `sessions.vnc_options`
/// JSON column (doc 06: "quality, scaling, etc.").
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VncOptions {
    /// 0-9.
    pub quality: u8,
    pub scaling: bool,
    pub view_only: bool,
}

/// A saved session — the Rust mirror of the `sessions` table (doc 06).
///
/// No credential material: secrets live in the OS keyring only
/// (docs 06 §Credential Storage / 11). `auth_method` records *which*
/// method to use, never the secret itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// None = use the global `host_key_policy` setting (doc 11).
    pub host_key_policy: Option<HostKeyPolicy>,
    /// ProxyJump-style hop through the referenced session (doc 11);
    /// max depth 1 in v1.
    pub jump_session_id: Option<i64>,
    /// Local forwards (doc 11).
    pub forward_rules: Option<Vec<ForwardRule>>,
    pub tmux_enabled: bool,
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

/// A session folder — the Rust mirror of the `folders` table (doc 06).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub sort_order: i64,
    pub icon: Option<String>,
}

#[cfg(test)]
mod tests;
