//! Server-certificate trust for RDP (TOFU, SSH-like).
//!
//! Wayterm manages RDP server trust itself instead of delegating it to
//! xfreerdp: the known fingerprints live in the `rdp_trust` store table
//! (one row per `(host, port)`), and the resolved fingerprint is passed to
//! xfreerdp as `/cert:fingerprint:sha256:<hex>`. In fingerprint mode
//! xfreerdp writes nothing to its own known-hosts store, so our table is
//! the single source of truth. `/cert:ignore` is never emitted (there is a
//! unit test pinning that negative path in [`crate::rdp::process`]).
//!
//! ## Trust matrix (as probed against FreeRDP 3.15 / xrdp)
//!
//! | Stored | Seen    | Outcome                                     |
//! |--------|---------|---------------------------------------------|
//! | none   | fp      | prompt the user (TOFU); on approval, record |
//! | fp     | same fp | pass the fingerprint token, connect         |
//! | fp     | other   | hard fail, never auto-accept (possible MITM)|
//! | any    | none    | error: no server certificate to check       |
//!
//! [`decide`] maps `(stored, seen)` to a [`CertDecision`]; [`cert_arg`]
//! maps a decision to the argv token (`Some` only when a fingerprint was
//! resolved — `None` means the caller must prompt first or abort). Both are
//! pure and display-free.
//!
//! The fingerprint form is lowercase hex WITHOUT colons: 64 chars for
//! SHA-256 (e.g. `111e9442…7fe1e3` style, as the probe showed). Use
//! [`fingerprint_sha256_hex`] to compute it from DER bytes and
//! [`is_valid_fingerprint`] to check the shape before comparing.

/// Prefix of the xfreerdp argv token carrying a pinned SHA-256 fingerprint.
pub const CERT_ARG_PREFIX: &str = "/cert:fingerprint:sha256:";

/// What to do about the server certificate the peer presented.
///
/// Produced by [`decide`]; consumed by [`cert_arg`]. The RDP tab prompts on
/// `AcceptAndRecord` (TOFU approval) and hard-fails on `ChangedCert` and
/// `NoServerCert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertDecision {
    /// No stored fingerprint for this `(host, port)`: ask the user, and on
    /// approval record `fingerprint` via `rdp_trust_record`.
    AcceptAndRecord { fingerprint: String },
    /// Stored and seen fingerprints match: connect with the token.
    PassStored { fingerprint: String },
    /// Stored and seen fingerprints differ: refuse, never auto-accept.
    ChangedCert { stored: String, seen: String },
    /// The peer presented no certificate to check: abort.
    NoServerCert,
}

/// Map `(stored, seen)` fingerprints to a [`CertDecision`].
///
/// `stored` is the `rdp_trust` row for the session (`None` = first contact);
/// `seen` is the fingerprint of the certificate the server just presented
/// (`None` = the handshake yielded no certificate).
pub fn decide(stored: Option<&str>, seen: Option<&str>) -> CertDecision {
    match (stored, seen) {
        (None, Some(seen)) => CertDecision::AcceptAndRecord {
            fingerprint: seen.to_string(),
        },
        (Some(stored), Some(seen)) if stored == seen => CertDecision::PassStored {
            fingerprint: stored.to_string(),
        },
        (Some(stored), Some(seen)) => CertDecision::ChangedCert {
            stored: stored.to_string(),
            seen: seen.to_string(),
        },
        (_, None) => CertDecision::NoServerCert,
    }
}

/// Map a [`CertDecision`] to the xfreerdp argv token.
///
/// Returns `Some("/cert:fingerprint:sha256:<hex>")` only when a fingerprint
/// was resolved (`AcceptAndRecord` after user approval, or `PassStored`);
/// `None` means the caller must prompt first (`AcceptAndRecord` before
/// approval) or abort (`ChangedCert`, `NoServerCert`).
pub fn cert_arg(decision: &CertDecision) -> Option<String> {
    match decision {
        CertDecision::AcceptAndRecord { fingerprint }
        | CertDecision::PassStored { fingerprint } => {
            Some(format!("{CERT_ARG_PREFIX}{fingerprint}"))
        }
        CertDecision::ChangedCert { .. } | CertDecision::NoServerCert => None,
    }
}

/// SHA-256 over DER bytes, as lowercase hex without colons (64 chars).
pub fn fingerprint_sha256_hex(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// True when `s` has the fingerprint shape: 64 lowercase hex chars, no
/// colons. Rejects empty strings, colon-separated forms (`aa:bb:…`), and
/// uppercase — all of which xfreerdp would refuse in fingerprint mode.
pub fn is_valid_fingerprint(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Sentinel fingerprint for the acquisition probe: a fingerprint-mode
/// connect holding this value can never match a real server certificate
/// (the all-zero SHA-256), so xfreerdp refuses the handshake and — as
/// verified on 3.15 — prints the server's actual thumbprint on stderr
/// while writing nothing to any known-hosts store. The UI flow feeds the
/// parsed value into the TOFU prompt, then reconnects with the real
/// [`CERT_ARG_PREFIX`] token.
pub const PROBE_FINGERPRINT: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The fingerprint the server actually presented, parsed from xfreerdp's
/// stderr (as captured on 3.15 and 3.32). Two real line shapes exist —
///
/// ```text
/// [time] [pid:tid] [ERROR][com.freerdp.crypto] - [fn]: The fingerprint for the host key sent by the remote host is 11:1e:94:…
///     Thumbprint:  11:1e:94:…
/// ```
///
/// Both carry colon-separated hex; the value is normalized (colons
/// stripped, lowercased) and shape-checked before being returned.
///
/// The server controls text that lands on the same stream — its
/// certificate's names are printed (the name-mismatch block quotes the CN
/// *before* the fingerprint line) — so each shape is anchored: the
/// key-line text must be the whole log message (or the bare line), and
/// `Thumbprint:` must start the line. Every match must also agree: two
/// different values mean something besides FreeRDP wrote one, and `None`
/// is returned rather than guessing. `None` too when no line carries a
/// parseable fingerprint (nothing is ever invented). xfreerdp, not this
/// parser, enforces the pin; this only decides what the user is shown and
/// what is recorded. The UI supplies raw stderr lines — the ring's
/// [`RdpProcess::error_digest`](super::process::RdpProcess::error_digest)
/// or live lines.
pub fn parse_seen_fingerprint(lines: &[String]) -> Option<String> {
    let mut found: Option<String> = None;
    for candidate in lines.iter().filter_map(|line| fingerprint_on(line)) {
        match &found {
            None => found = Some(candidate),
            Some(first) if *first == candidate => {}
            Some(_) => return None,
        }
    }
    found
}

/// The fingerprint one stderr line carries in either FreeRDP shape.
fn fingerprint_on(line: &str) -> Option<String> {
    const KEY_MARK: &str = "The fingerprint for the host key sent by the remote host is ";
    const THUMB_MARK: &str = "Thumbprint:";
    let bare = line.trim_start();
    let value = bare
        .strip_prefix(THUMB_MARK)
        .or_else(|| bare.strip_prefix(KEY_MARK))
        .or_else(|| log_message(line)?.strip_prefix(KEY_MARK))?;
    let compact: String = value
        .split_whitespace()
        .next()?
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    is_valid_fingerprint(&compact).then_some(compact)
}

/// The message of a WLog line in FreeRDP's default layout,
/// `[time] [pid:tid] [LEVEL][tag] - [function]: message`. The child's
/// environment is cleared, so no `WLOG_PREFIX` can change the layout.
fn log_message(line: &str) -> Option<&str> {
    let after_tag = &line[line.find(" - [")? + " - [".len()..];
    Some(&after_tag[after_tag.find("]: ")? + "]: ".len()..])
}

#[cfg(test)]
mod tests;
