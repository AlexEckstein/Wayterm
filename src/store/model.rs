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
///
/// `#[serde(default)]` is what keeps stored rows readable as the struct
/// grows: a row/JSON payload written before a field existed deserializes
/// with that field's `Default` instead of failing. The serialized form is
/// unchanged — the attribute only relaxes *deserialization*.
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

// ---------------------------------------------------------------------------
// Per-type value vocabularies
//
// Doc 06 stores the enum-like per-type columns as TEXT/INTEGER and names the
// default in the schema comment, but only the default is enumerated:
//
//     serial_data_bits    INTEGER DEFAULT 8
//     serial_parity       TEXT DEFAULT 'none'
//     serial_stop_bits    INTEGER DEFAULT 1
//     serial_flow_control TEXT DEFAULT 'none'
//     rdp_color_depth     INTEGER            -- no default in the schema
//
// The constants below are the single source of truth for the *remaining*
// values (doc 08 §RDP pins the color-depth fallback to 32 bpp). Each
// `*_DEFAULT` restates the documented default, so the dialog, the DropDown
// models, and the config mapper cannot drift apart. Adding a value to a
// list changes the wire vocabulary for stored rows — treat it like a new
// enum variant.
// ---------------------------------------------------------------------------

/// `sessions.serial_parity` vocabulary — POSIX termios parity modes.
/// Doc 06 default: `none`.
pub const SERIAL_PARITIES: [&str; 5] = ["none", "odd", "even", "mark", "space"];

/// Doc 06 `serial_parity TEXT DEFAULT 'none'`.
pub const SERIAL_PARITY_DEFAULT: &str = "none";

/// `sessions.serial_flow_control` vocabulary.
/// Doc 06 default: `none`.
pub const SERIAL_FLOW_CONTROLS: [&str; 3] = ["none", "hardware", "software"];

/// Doc 06 `serial_flow_control TEXT DEFAULT 'none'`.
pub const SERIAL_FLOW_CONTROL_DEFAULT: &str = "none";

/// `sessions.serial_data_bits` vocabulary.
pub const SERIAL_DATA_BITS: [u8; 4] = [5, 6, 7, 8];

/// Doc 06 `serial_data_bits INTEGER DEFAULT 8`.
pub const SERIAL_DATA_BITS_DEFAULT: u8 = 8;

/// `sessions.serial_stop_bits` vocabulary.
pub const SERIAL_STOP_BITS: [u8; 2] = [1, 2];

/// Doc 06 `serial_stop_bits INTEGER DEFAULT 1`.
pub const SERIAL_STOP_BITS_DEFAULT: u8 = 1;

/// `sessions.rdp_color_depth` vocabulary (bits per pixel).
pub const RDP_COLOR_DEPTHS: [u32; 3] = [16, 24, 32];

/// Doc 06 stores color depth as a nullable INTEGER with no default; doc 08
/// §RDP spawns xfreerdp with `/bpp:{rdp_color_depth.unwrap_or(32)}`, so 32 is
/// the documented fallback.
pub const RDP_COLOR_DEPTH_DEFAULT: u32 = 32;

/// Resolve a DropDown index to the value stored in the session row.
///
/// An out-of-range index cannot come from a GTK DropDown but does occur in
/// hand-built [`DialogFields`](crate::ui::session_dialog::DialogFields), so it
/// resolves to `default` rather than panicking.
pub fn vocab_value<T: Copy + PartialEq>(values: &[T], index: u32, default: T) -> T {
    match values.get(index as usize) {
        Some(value) => *value,
        None => default,
    }
}

/// Resolve a stored value to the DropDown index that selects it, for
/// pre-filling a row (doc-06 column → dialog widget).
///
/// A missing or unrecognised value selects `default`'s index, which is what
/// the mapper will write back — so editing and saving an unrecognised value
/// normalises it to the documented default instead of silently keeping it.
pub fn vocab_index<T: Copy + PartialEq>(values: &[T], value: Option<T>, default: T) -> u32 {
    match value.and_then(|v| values.iter().position(|c| *c == v)) {
        Some(pos) => pos as u32,
        None => values.iter().position(|c| *c == default).unwrap_or(0) as u32,
    }
}

#[cfg(test)]
mod tests;
