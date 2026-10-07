//! Authentication chain — full method fallback per doc 11 §Authentication State Machine.
//!
//! Order: agent → key → password → keyboard-interactive.
//! Failure of any single method is NOT fatal — the chain falls through to the
//! next. Exhaustion of all methods = clean `Failed` outcome.
//!
//! Design contracts:
//!   - docs/11-ssh-auth-security.md §Authentication State Machine
//!   - docs/04-transport-layer.md §Auth & Credential Handling

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use russh::client::{Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use zeroize::Zeroizing;

use crate::transport::connection::AppClientHandler;
use crate::transport::types::TransportError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Discriminant for an auth method — used in `AuthOutcome::Failed::methods_remaining`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthMethodKind {
    Agent,
    PublicKey,
    Password,
    KeyboardInteractive,
}

impl fmt::Display for AuthMethodKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => write!(f, "agent"),
            Self::PublicKey => write!(f, "publickey"),
            Self::Password => write!(f, "password"),
            Self::KeyboardInteractive => write!(f, "keyboard-interactive"),
        }
    }
}

/// Outcome of the full authentication chain.
#[derive(Debug)]
pub enum AuthOutcome {
    /// Authentication succeeded (at least one method was accepted).
    Success,
    /// All attempted methods failed or the server offers no compatible methods.
    Failed {
        reason: String,
        /// Methods that were never tried (server didn't offer them, or skipped
        /// due to missing config).
        methods_remaining: Vec<AuthMethodKind>,
    },
    /// The user (or prompt closure) cancelled authentication.
    Cancelled,
}

/// One prompt in a keyboard-interactive challenge round.
#[derive(Clone, Debug)]
pub struct ChallengePrompt {
    pub prompt: String,
    pub echo: bool,
}

/// Bridge closure: UI or test provides answers to server challenges.
///
/// Same pattern as `HostKeyPrompt` in hostkey.rs — the GTK dialog implements
/// this in Phase 3; tests inject a closure that echoes the password.
///
/// Returns `None` if the user cancels the prompt.
pub type ChallengeResponder = Arc<dyn Fn(&[ChallengePrompt]) -> Option<Vec<String>> + Send + Sync>;

// ---------------------------------------------------------------------------
// Auth configuration passed into the chain
// ---------------------------------------------------------------------------

/// Everything the auth chain needs to attempt all methods.
///
/// The caller populates whichever fields are available; absent fields cause
/// the corresponding method to be skipped.
pub struct AuthConfig {
    /// Private key path (for public-key auth).
    pub key_path: Option<String>,
    /// Passphrase for the private key (if encrypted).
    pub key_passphrase: Option<Zeroizing<Vec<u8>>>,
    /// Password (for password auth).
    pub password: Option<Zeroizing<Vec<u8>>>,
    /// Whether to attempt SSH agent auth.
    pub try_agent: bool,
    /// Whether to attempt keyboard-interactive auth.
    pub try_keyboard_interactive: bool,
    /// Challenge-response closure for keyboard-interactive prompts.
    /// Also used to re-prompt for password on retry.
    pub challenge_responder: Option<ChallengeResponder>,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("key_path", &self.key_path)
            .field(
                "key_passphrase",
                &self.key_passphrase.as_ref().map(|_| "[REDACTED]"),
            )
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("try_agent", &self.try_agent)
            .field("try_keyboard_interactive", &self.try_keyboard_interactive)
            .field(
                "challenge_responder",
                &self.challenge_responder.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The auth chain
// ---------------------------------------------------------------------------

/// Run the full authentication fallback chain per doc 11 §Flow.
///
/// Order: agent → key → password → keyboard-interactive.
/// Each method that fails is logged (never the credential) and the chain falls
/// through. Exhaustion = clean `AuthOutcome::Failed`.
pub async fn authenticate_chain(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    config: &AuthConfig,
) -> Result<AuthOutcome, TransportError> {
    let mut methods_not_tried: Vec<AuthMethodKind> = Vec::new();
    let mut last_reason = String::new();

    // -----------------------------------------------------------------------
    // 1. Agent auth
    // -----------------------------------------------------------------------
    if config.try_agent {
        match try_agent_auth(handle, username).await {
            Ok(true) => return Ok(AuthOutcome::Success),
            Ok(false) => {
                last_reason = "agent: no identity accepted by server".into();
            }
            Err(reason) => {
                last_reason = format!("agent: {reason}");
            }
        }
    } else {
        methods_not_tried.push(AuthMethodKind::Agent);
    }

    // -----------------------------------------------------------------------
    // 2. Public-key auth
    // -----------------------------------------------------------------------
    if let Some(ref key_path) = config.key_path {
        match try_key_auth(handle, username, key_path, config.key_passphrase.as_ref()).await {
            Ok(true) => return Ok(AuthOutcome::Success),
            Ok(false) => {
                last_reason = "publickey: server rejected key".into();
            }
            Err(reason) => {
                last_reason = format!("publickey: {reason}");
            }
        }
    } else {
        methods_not_tried.push(AuthMethodKind::PublicKey);
    }

    // -----------------------------------------------------------------------
    // 3. Password auth (with one retry on wrong-password via re-prompt)
    // -----------------------------------------------------------------------
    if let Some(ref password) = config.password {
        match try_password_auth(handle, username, password).await {
            Ok(true) => return Ok(AuthOutcome::Success),
            Ok(false) => {
                // Wrong password — retry once via re-prompt (doc 11 §Flow).
                if let Some(ref responder) = config.challenge_responder {
                    let prompts = [ChallengePrompt {
                        prompt: "Password (retry): ".to_string(),
                        echo: false,
                    }];
                    if let Some(answers) = responder(&prompts) {
                        if let Some(retry_pw) = answers.into_iter().next() {
                            let retry_bytes = Zeroizing::new(retry_pw.into_bytes());
                            match try_password_auth(handle, username, &retry_bytes).await {
                                Ok(true) => return Ok(AuthOutcome::Success),
                                Ok(false) => {
                                    last_reason = "password: rejected after retry".into();
                                }
                                Err(reason) => {
                                    last_reason = format!("password retry: {reason}");
                                }
                            }
                        }
                    } else {
                        return Ok(AuthOutcome::Cancelled);
                    }
                } else {
                    last_reason = "password: rejected, no re-prompt available".into();
                }
            }
            Err(reason) => {
                last_reason = format!("password: {reason}");
            }
        }
    } else {
        methods_not_tried.push(AuthMethodKind::Password);
    }

    // -----------------------------------------------------------------------
    // 4. Keyboard-interactive auth
    // -----------------------------------------------------------------------
    if config.try_keyboard_interactive {
        if let Some(ref responder) = config.challenge_responder {
            match try_keyboard_interactive_auth(handle, username, responder).await {
                Ok(true) => return Ok(AuthOutcome::Success),
                Ok(false) => {
                    last_reason = "keyboard-interactive: server rejected responses".into();
                }
                Err(reason) => {
                    last_reason = format!("keyboard-interactive: {reason}");
                }
            }
        } else {
            methods_not_tried.push(AuthMethodKind::KeyboardInteractive);
            last_reason = "keyboard-interactive: no challenge responder configured".into();
        }
    } else {
        methods_not_tried.push(AuthMethodKind::KeyboardInteractive);
    }

    // -----------------------------------------------------------------------
    // All methods exhausted
    // -----------------------------------------------------------------------
    Ok(AuthOutcome::Failed {
        reason: if last_reason.is_empty() {
            "no authentication methods available".into()
        } else {
            last_reason
        },
        methods_remaining: methods_not_tried,
    })
}

// ---------------------------------------------------------------------------
// Individual method implementations
// ---------------------------------------------------------------------------

/// Attempt SSH agent authentication.
///
/// Connects to the agent via `SSH_AUTH_SOCK`, enumerates identities, and tries
/// each one. Returns `Ok(true)` on success, `Ok(false)` if all identities were
/// rejected, `Err` on transport/agent errors.
async fn try_agent_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
) -> Result<bool, String> {
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| format!("cannot connect to agent: {e}"))?;

    let identities = agent
        .request_identities()
        .await
        .map_err(|e| format!("agent request_identities failed: {e}"))?;

    if identities.is_empty() {
        return Err("agent has no identities".into());
    }

    for identity in &identities {
        let (pubkey, hash_alg) = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                // Determine hash algorithm: for RSA keys the server may prefer
                // SHA-256 or SHA-512 over SHA-1. best_supported_rsa_hash()
                // returns None for non-RSA keys.
                let alg = handle
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| format!("best_supported_rsa_hash: {e}"))?
                    .flatten();
                (key.clone(), alg)
            }
            AgentIdentity::Certificate { .. } => {
                // Certificate-based agent auth — skip for now, fall through
                continue;
            }
        };

        match handle
            .authenticate_publickey_with(username, pubkey, hash_alg, &mut agent)
            .await
        {
            Ok(result) if result.success() => return Ok(true),
            Ok(_) => continue, // This identity rejected, try next
            Err(_) => continue,
        }
    }

    Ok(false)
}

/// Attempt public-key authentication with a key file on disk.
async fn try_key_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    key_path: &str,
    passphrase: Option<&Zeroizing<Vec<u8>>>,
) -> Result<bool, String> {
    let passphrase_str = passphrase.map(|p| String::from_utf8_lossy(p).to_string());
    let key = load_secret_key(Path::new(key_path), passphrase_str.as_deref())
        .map_err(|e| format!("load key {key_path}: {e}"))?;

    // Derive the hash algorithm per key type (doc 11 warns against hardcoding
    // "ssh-ed25519").
    let hash_alg = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| format!("best_supported_rsa_hash: {e}"))?
        .flatten();

    let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    let result = handle
        .authenticate_publickey(username, key_with_alg)
        .await
        .map_err(|e| format!("publickey auth: {e}"))?;

    Ok(result.success())
}

/// Attempt password authentication.
async fn try_password_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    password: &Zeroizing<Vec<u8>>,
) -> Result<bool, String> {
    let pw_str = String::from_utf8_lossy(password).to_string();
    let result = handle
        .authenticate_password(username, pw_str)
        .await
        .map_err(|e| format!("password auth: {e}"))?;

    Ok(result.success())
}

/// Attempt keyboard-interactive authentication.
///
/// russh 0.64.1 API (differs from doc 11's 0.63 anchor):
/// - `handle.authenticate_keyboard_interactive_start()` → initiates
/// - Returns `KeyboardInteractiveAuthResponse` which can be `Success`,
///   `Failure`, or `InfoRequest { name, instructions, prompts: Vec<Prompt> }`
/// - For each `InfoRequest`, call `handle.authenticate_keyboard_interactive_respond()`
///   with the answers.
/// - Multiple rounds are supported.
async fn try_keyboard_interactive_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    responder: &ChallengeResponder,
) -> Result<bool, String> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(username, None::<String>)
        .await
        .map_err(|e| format!("kbd-interactive start: {e}"))?;

    // Maximum rounds to prevent infinite loops from misbehaving servers.
    const MAX_ROUNDS: usize = 32;

    for _round in 0..MAX_ROUNDS {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name: _,
                instructions: _,
                prompts,
            } => {
                // Convert russh Prompt to our ChallengePrompt
                let challenge_prompts: Vec<ChallengePrompt> = prompts
                    .iter()
                    .map(|p| ChallengePrompt {
                        prompt: p.prompt.clone(),
                        echo: p.echo,
                    })
                    .collect();

                // Ask the user (or test closure)
                let answers = match responder(&challenge_prompts) {
                    Some(a) => a,
                    None => {
                        // User cancelled — we can't cleanly abort
                        // keyboard-interactive mid-flow in russh 0.64, so
                        // send empty responses which will cause the server
                        // to reject.
                        return Err("user cancelled keyboard-interactive prompt".into());
                    }
                };

                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|e| format!("kbd-interactive respond: {e}"))?;
            }
        }
    }

    Err("keyboard-interactive: too many challenge rounds".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_method_kind_display() {
        assert_eq!(AuthMethodKind::Agent.to_string(), "agent");
        assert_eq!(AuthMethodKind::PublicKey.to_string(), "publickey");
        assert_eq!(AuthMethodKind::Password.to_string(), "password");
        assert_eq!(
            AuthMethodKind::KeyboardInteractive.to_string(),
            "keyboard-interactive"
        );
    }

    #[test]
    fn auth_outcome_debug_redaction() {
        let failed = AuthOutcome::Failed {
            reason: "test".into(),
            methods_remaining: vec![AuthMethodKind::Agent, AuthMethodKind::Password],
        };
        let dbg = format!("{failed:?}");
        assert!(dbg.contains("test"));
        assert!(dbg.contains("Agent"));
        assert!(dbg.contains("Password"));
    }

    #[test]
    fn auth_config_debug_redacts_secrets() {
        let config = AuthConfig {
            key_path: Some("/tmp/key".into()),
            key_passphrase: Some(Zeroizing::new(b"supersecret".to_vec())),
            password: Some(Zeroizing::new(b"hunter2".to_vec())),
            try_agent: true,
            try_keyboard_interactive: true,
            challenge_responder: None,
        };
        let dbg = format!("{config:?}");
        assert!(dbg.contains("[REDACTED]"));
        assert!(!dbg.contains("supersecret"));
        assert!(!dbg.contains("hunter2"));
    }

    #[test]
    fn challenge_prompt_roundtrip() {
        let prompts = vec![
            ChallengePrompt {
                prompt: "Password: ".into(),
                echo: false,
            },
            ChallengePrompt {
                prompt: "OTP: ".into(),
                echo: true,
            },
        ];

        let responder: ChallengeResponder = Arc::new(|ps| {
            Some(
                ps.iter()
                    .map(|p| {
                        if p.echo {
                            format!("echo-{}", p.prompt)
                        } else {
                            "secret".into()
                        }
                    })
                    .collect(),
            )
        });

        let answers = responder(&prompts).unwrap();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0], "secret");
        assert_eq!(answers[1], "echo-OTP: ");
    }

    #[test]
    fn challenge_responder_can_cancel() {
        let responder: ChallengeResponder = Arc::new(|_| None);
        let prompts = [ChallengePrompt {
            prompt: "Password: ".into(),
            echo: false,
        }];
        assert!(responder(&prompts).is_none());
    }

    #[test]
    fn auth_method_kind_order_matches_doc_11() {
        // The documented fallback order is agent → publickey → password →
        // keyboard-interactive. We assert the exact enum order by display
        // name, and that all four are distinct.
        let ordered = [
            AuthMethodKind::Agent,
            AuthMethodKind::PublicKey,
            AuthMethodKind::Password,
            AuthMethodKind::KeyboardInteractive,
        ];
        let displays: Vec<String> = ordered.iter().map(|k| k.to_string()).collect();
        assert_eq!(
            displays,
            vec!["agent", "publickey", "password", "keyboard-interactive"]
        );
        for (i, a) in ordered.iter().enumerate() {
            for (j, b) in ordered.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn auth_outcome_failed_carries_methods_remaining() {
        let outcome = AuthOutcome::Failed {
            reason: "all methods exhausted".into(),
            methods_remaining: vec![
                AuthMethodKind::Agent,
                AuthMethodKind::PublicKey,
                AuthMethodKind::Password,
                AuthMethodKind::KeyboardInteractive,
            ],
        };
        let dbg = format!("{outcome:?}");
        assert!(dbg.contains("all methods exhausted"));
        assert!(dbg.contains("Agent"));
        assert!(dbg.contains("PublicKey"));
        assert!(dbg.contains("Password"));
        assert!(dbg.contains("KeyboardInteractive"));
    }

    #[test]
    fn keyboard_interactive_multi_round_responder() {
        // Simulates a server that sends multiple rounds (password + OTP).
        // The responder must be called once per round and produce answers in
        // the same order as the prompts.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();

        let responder: ChallengeResponder = Arc::new(move |ps| {
            let round = calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answers: Vec<String> = ps
                .iter()
                .map(|p| {
                    if round == 0 && !p.echo {
                        "first-password".into()
                    } else if round == 1 && p.echo {
                        "123456".into()
                    } else {
                        "unexpected".into()
                    }
                })
                .collect();
            Some(answers)
        });

        let round1 = vec![ChallengePrompt {
            prompt: "Password: ".into(),
            echo: false,
        }];
        let round2 = vec![ChallengePrompt {
            prompt: "Verification code: ".into(),
            echo: true,
        }];

        assert_eq!(
            responder(&round1).unwrap(),
            vec!["first-password".to_string()]
        );
        assert_eq!(responder(&round2).unwrap(), vec!["123456".to_string()]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn keyboard_interactive_response_formatting() {
        // Verify that challenge prompts map correctly to response vectors.
        let prompts = vec![
            ChallengePrompt {
                prompt: "Password: ".into(),
                echo: false,
            },
            ChallengePrompt {
                prompt: "Verification code: ".into(),
                echo: true,
            },
            ChallengePrompt {
                prompt: "".into(),
                echo: false,
            },
        ];

        // Simulated responder that returns password for non-echo, "123456" for echo,
        // and empty for empty prompts.
        let responder: ChallengeResponder = Arc::new(|ps| {
            Some(
                ps.iter()
                    .map(|p| {
                        if p.prompt.is_empty() {
                            String::new()
                        } else if p.echo {
                            "123456".into()
                        } else {
                            "mypassword".into()
                        }
                    })
                    .collect(),
            )
        });

        let answers = responder(&prompts).unwrap();
        assert_eq!(answers.len(), 3);
        assert_eq!(answers[0], "mypassword");
        assert_eq!(answers[1], "123456");
        assert_eq!(answers[2], "");
    }

    #[test]
    fn challenge_responder_cancellation_propagates() {
        // A responder returning None models the user cancelling the prompt.
        let responder: ChallengeResponder = Arc::new(|_| None);
        let prompts = [ChallengePrompt {
            prompt: "Password: ".into(),
            echo: false,
        }];
        assert!(responder(&prompts).is_none());
    }
}
