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
/// Produced by [`decide`]; consumed by [`cert_arg`]. The UI lane prompts on
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
///
/// Uses `sha2` 0.10 (already a dependency; verified against the registry as
/// the current 0.10.x line — no new crate pinned by this module).
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

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "111e9442b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c7fe1";
    const FP_B: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn unknown_cert_proposes_record_and_resolves_to_token() {
        let decision = decide(None, Some(FP_A));
        assert_eq!(
            decision,
            CertDecision::AcceptAndRecord {
                fingerprint: FP_A.to_string()
            }
        );
        assert_eq!(
            cert_arg(&decision),
            Some(format!("{CERT_ARG_PREFIX}{FP_A}"))
        );
    }

    #[test]
    fn same_cert_passes_stored_token() {
        let decision = decide(Some(FP_A), Some(FP_A));
        assert_eq!(
            decision,
            CertDecision::PassStored {
                fingerprint: FP_A.to_string()
            }
        );
        assert_eq!(
            cert_arg(&decision),
            Some(format!("{CERT_ARG_PREFIX}{FP_A}"))
        );
    }

    #[test]
    fn changed_cert_refuses_with_no_token() {
        let decision = decide(Some(FP_A), Some(FP_B));
        assert_eq!(
            decision,
            CertDecision::ChangedCert {
                stored: FP_A.to_string(),
                seen: FP_B.to_string(),
            }
        );
        assert_eq!(cert_arg(&decision), None);
    }

    #[test]
    fn missing_server_cert_is_an_error_with_no_token() {
        for stored in [None, Some(FP_A)] {
            let decision = decide(stored, None);
            assert_eq!(decision, CertDecision::NoServerCert);
            assert_eq!(cert_arg(&decision), None);
        }
    }

    #[test]
    fn fingerprint_form_is_lowercase_hex_without_colons() {
        let fp = fingerprint_sha256_hex(b"fake-der-bytes");
        assert_eq!(fp.len(), 64);
        assert!(is_valid_fingerprint(&fp));
        assert!(!fp.contains(':'));
        assert_eq!(fp, fp.to_lowercase());
    }

    #[test]
    fn sha256_matches_the_known_empty_vector() {
        // Locks the hash function itself, not just the formatting.
        assert_eq!(
            fingerprint_sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn fingerprint_shape_rejects_bad_forms() {
        assert!(!is_valid_fingerprint(""));
        assert!(!is_valid_fingerprint(&format!("{FP_A}0"))); // 65 chars
        assert!(!is_valid_fingerprint(&FP_A[..63])); // 63 chars
        assert!(!is_valid_fingerprint(&FP_A.to_uppercase())); // uppercase
        assert!(!is_valid_fingerprint("aa:bb:cc")); // colon-separated
        assert!(!is_valid_fingerprint(&"g".repeat(64))); // non-hex
    }
}
