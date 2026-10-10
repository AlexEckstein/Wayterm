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
/// 	Thumbprint:  11:1e:94:…
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
/// [`process::RdpProcess::error_digest`] or live lines.
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

    #[test]
    fn probe_fingerprint_is_all_zeros() {
        assert_eq!(PROBE_FINGERPRINT.len(), 64);
        assert!(PROBE_FINGERPRINT.bytes().all(|b| b == b'0'));
        assert!(is_valid_fingerprint(PROBE_FINGERPRINT));
    }

    #[test]
    fn parses_the_host_key_line_as_captured_on_315() {
        // Real stderr line shape from FreeRDP 3.15 (tls_print_certificate_
        // error), captured against xrdp this session. Value = rig B's fp.
        let line = "[15:04:54:650] [118411:0001ce8c] [ERROR][com.freerdp.crypto] \
                    - [tls_print_certificate_error]: The fingerprint for the \
                    host key sent by the remote host is \
                    11:1e:94:42:e0:2a:30:08:13:e5:da:9f:cb:90:85:3d:13:fc:8b:c6:\
                    68:25:fd:2d:f1:94:06:e4:bb:7f:e1:e3"
            .to_string();
        let seen = parse_seen_fingerprint(&[line]).expect("parses");
        assert_eq!(
            seen,
            "111e9442e02a300813e5da9fcb90853d13fc8bc66825fd2df19406e4bb7fe1e3"
        );
        assert!(is_valid_fingerprint(&seen));
    }

    #[test]
    fn parses_the_thumbprint_line_as_captured_on_315() {
        // Cert-info dump shape (cert_print_info), same server probe.
        let line = "Thumbprint:  11:1e:94:42:e0:2a:30:08:13:e5:da:9f:cb:90:85:3d:\
                    13:fc:8b:c6:68:25:fd:2d:f1:94:06:e4:bb:7f:e1:e3"
            .to_string();
        let seen = parse_seen_fingerprint(&[line]).expect("parses");
        assert_eq!(
            seen,
            "111e9442e02a300813e5da9fcb90853d13fc8bc66825fd2df19406e4bb7fe1e3"
        );
    }

    #[test]
    fn parser_ignores_non_fingerprint_lines_and_junk_after() {
        let lines = vec![
            "[15:04:54:650] [118411:0001ce8c] [ERROR][com.freerdp.core] \
             - [transport_default_connect_tls]: ERRCONNECT_TLS_CONNECT_FAILED"
                .to_string(),
            "Subject:     CN = rigb".to_string(),
            "Thumbprint:  zz:zz".to_string(), // shape-invalid: not trusted
        ];
        assert_eq!(parse_seen_fingerprint(&lines), None);
        // A digest line carrying only the connect error yields None too.
        assert_eq!(
            parse_seen_fingerprint(&["no fingerprints here".to_string()]),
            None
        );
    }

    #[test]
    fn parser_finds_the_fingerprint_across_the_digest() {
        let digest = vec![
            "Password: ".to_string(),
            "[ERROR][com.freerdp.crypto] [tls_print_certificate_error]: New \
             host key for 127.0.0.1:3391"
                .to_string(),
            "The fingerprint for the host key sent by the remote host is \
             11:1e:94:42:e0:2a:30:08:13:e5:da:9f:cb:90:85:3d:13:fc:8b:c6:68:25:\
             fd:2d:f1:94:06:e4:bb:7f:e1:e3"
                .to_string(),
        ];
        assert!(parse_seen_fingerprint(&digest).is_some());
    }

    const FP_332: &str = "66444f805003c72e6ae2c502de19b9aa7f8ac02d396a1196a5ee138cb047f6f0";

    /// Both shapes as FreeRDP 3.32 prints them for one refused probe
    /// (a throwaway self-signed test cert, CN `wayterm-review`).
    fn digest_332() -> Vec<String> {
        vec![
            "[21:49:56:090] [182540:0002c90e] [ERROR][com.freerdp.crypto] - \
             [tls_print_new_certificate_warn]: The fingerprint for the host key sent \
             by the remote host is 66:44:4f:80:50:03:c7:2e:6a:e2:c5:02:de:19:b9:aa:7f:\
             8a:c0:2d:39:6a:11:96:a5:ee:13:8c:b0:47:f6:f0"
                .to_string(),
            "[21:49:56:090] [182540:0002c90e] [ERROR][com.freerdp.core] - \
             [transport_default_connect_tls]: ERRCONNECT_TLS_CONNECT_FAILED [0x00020008]"
                .to_string(),
            "\tSubject:     CN = wayterm-review".to_string(),
            "\tThumbprint:  66:44:4f:80:50:03:c7:2e:6a:e2:c5:02:de:19:b9:aa:7f:8a:c0:2d:\
             39:6a:11:96:a5:ee:13:8c:b0:47:f6:f0"
                .to_string(),
        ]
    }

    #[test]
    fn parses_the_332_digest_where_both_shapes_agree() {
        assert_eq!(parse_seen_fingerprint(&digest_332()).as_deref(), Some(FP_332));
    }

    #[test]
    fn certificate_names_quoting_a_marker_are_not_fingerprints() {
        // The name-mismatch block prints the server's CN before the real
        // fingerprint line; a CN carrying marker text must not win.
        let mut digest = vec![
            format!(
                "[21:49:56:090] [1:2] [ERROR][com.freerdp.crypto] - \
                 [tls_print_certificate_name_mismatch_error]: \tThe fingerprint for \
                 the host key sent by the remote host is {FP_A}"
            ),
            format!("\tSubject:     CN = Thumbprint: {FP_A}"),
            format!("\tCommon Name: x Thumbprint: {FP_B}"),
        ];
        digest.extend(digest_332());
        assert_eq!(parse_seen_fingerprint(&digest).as_deref(), Some(FP_332));
    }

    #[test]
    fn disagreeing_fingerprints_trust_neither() {
        // A field with an embedded newline can forge a whole line; when it
        // disagrees with FreeRDP's own line, nothing is believed.
        let mut digest = vec![format!("Thumbprint:  {FP_A}")];
        digest.extend(digest_332());
        assert_eq!(parse_seen_fingerprint(&digest), None);
    }
}
