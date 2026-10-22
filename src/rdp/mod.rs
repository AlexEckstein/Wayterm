//! RDP sessions via an xfreerdp subprocess (doc 08 §RDP).
//!
//! RDP is deliberately not a `Transport` (doc 03 §Transport factory): the
//! engine here owns a child process and an event stream instead of a byte
//! stream, mirroring how `vnc` stays display-free (doc 08 §VNC). The GTK
//! embedding (reparenting xfreerdp into a container widget) lives in
//! `ui::rdp_tab`; everything here is tokio-side and `Send`.
//!
//! - [`cert`]: TOFU trust resolver over the `rdp_trust` store table.
//! - [`process`]: argv builder, binary probe, and the child wrapper.

pub mod cert;
pub mod process;

pub use cert::{
    cert_arg, decide, fingerprint_sha256_hex, is_valid_fingerprint, CertDecision, CERT_ARG_PREFIX,
};
pub use process::{
    build_args, exit_event, find_binary_in, is_supported_version, minimal_env, parse_version,
    probe_binary, ErrorRing, RdpProcess, RdpProcessEvent, RdpSpec, BINARY_NAMES, CONNECTED_DELAY,
    CONNECT_ERROR_MARKER, MIN_FREERDP_MAJOR, RING_CAPACITY, TERMINATE_GRACE,
};
