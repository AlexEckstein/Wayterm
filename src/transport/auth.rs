//! The authentication chain (doc 04 §Authentication):
//! agent → key → password → keyboard-interactive. A failing method falls
//! through to the next; running out of methods is a clean `Failed`.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use russh::client::{Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg};
use zeroize::Zeroizing;

use crate::transport::connection::AppClientHandler;
use crate::transport::types::TransportError;

/// An auth method, as reported in [`AuthOutcome::Failed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthMethodKind {
    Agent,
    PublicKey,
    Password,
    KeyboardInteractive,
}

impl fmt::Display for AuthMethodKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Agent => "agent",
            Self::PublicKey => "publickey",
            Self::Password => "password",
            Self::KeyboardInteractive => "keyboard-interactive",
        })
    }
}

#[derive(Debug)]
pub enum AuthOutcome {
    Success,
    /// Every attempted method failed. `methods_remaining` lists the methods
    /// the chain never attempted (not configured), so a method's absence
    /// proves it was tried and rejected.
    Failed {
        reason: String,
        methods_remaining: Vec<AuthMethodKind>,
    },
    /// The user cancelled a prompt.
    Cancelled,
}

/// One prompt of a keyboard-interactive round.
#[derive(Clone, Debug)]
pub struct ChallengePrompt {
    pub prompt: String,
    pub echo: bool,
}

/// Answers server challenges (the GTK dialog, or a test closure). `None`
/// means the user cancelled. Also used for the one wrong-password re-prompt.
pub type ChallengeResponder = Arc<dyn Fn(&[ChallengePrompt]) -> Option<Vec<String>> + Send + Sync>;

/// What the chain may try; absent fields skip their method.
pub struct AuthConfig {
    pub key_path: Option<String>,
    pub key_passphrase: Option<Zeroizing<Vec<u8>>>,
    pub password: Option<Zeroizing<Vec<u8>>>,
    pub try_agent: bool,
    pub try_keyboard_interactive: bool,
    pub challenge_responder: Option<ChallengeResponder>,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = |set: bool| if set { "[REDACTED]" } else { "None" };
        f.debug_struct("AuthConfig")
            .field("key_path", &self.key_path)
            .field("key_passphrase", &redacted(self.key_passphrase.is_some()))
            .field("password", &redacted(self.password.is_some()))
            .field("try_agent", &self.try_agent)
            .field("try_keyboard_interactive", &self.try_keyboard_interactive)
            .field(
                "challenge_responder",
                &self.challenge_responder.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

/// Run the chain (doc 04 §Authentication). Failure reasons name the method and never
/// contain a credential.
pub async fn authenticate_chain(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    config: &AuthConfig,
) -> Result<AuthOutcome, TransportError> {
    let mut not_tried = Vec::new();
    let mut last_reason = String::new();

    if config.try_agent {
        match try_agent_auth(handle, username).await {
            Ok(true) => return Ok(AuthOutcome::Success),
            Ok(false) => last_reason = "agent: no identity accepted by server".into(),
            Err(reason) => last_reason = format!("agent: {reason}"),
        }
    } else {
        not_tried.push(AuthMethodKind::Agent);
    }

    if let Some(key_path) = &config.key_path {
        match try_key_auth(handle, username, key_path, config.key_passphrase.as_ref()).await {
            Ok(true) => return Ok(AuthOutcome::Success),
            Ok(false) => last_reason = "publickey: server rejected key".into(),
            Err(reason) => last_reason = format!("publickey: {reason}"),
        }
    } else {
        not_tried.push(AuthMethodKind::PublicKey);
    }

    if let Some(password) = &config.password {
        match try_password_auth(handle, username, password).await {
            Ok(true) => return Ok(AuthOutcome::Success),
            Err(reason) => last_reason = format!("password: {reason}"),
            // Wrong password: re-prompt exactly once (doc 04 §Authentication).
            Ok(false) => match &config.challenge_responder {
                None => last_reason = "password: rejected, no re-prompt available".into(),
                Some(responder) => {
                    let retry = [ChallengePrompt {
                        prompt: "Password (retry): ".into(),
                        echo: false,
                    }];
                    let Some(answers) = responder(&retry) else {
                        return Ok(AuthOutcome::Cancelled);
                    };
                    if let Some(answer) = answers.into_iter().next() {
                        let answer = Zeroizing::new(answer.into_bytes());
                        match try_password_auth(handle, username, &answer).await {
                            Ok(true) => return Ok(AuthOutcome::Success),
                            Ok(false) => last_reason = "password: rejected after retry".into(),
                            Err(reason) => last_reason = format!("password retry: {reason}"),
                        }
                    }
                }
            },
        }
    } else {
        not_tried.push(AuthMethodKind::Password);
    }

    match (config.try_keyboard_interactive, &config.challenge_responder) {
        (true, Some(responder)) => {
            match try_keyboard_interactive_auth(handle, username, responder).await {
                Ok(true) => return Ok(AuthOutcome::Success),
                Ok(false) => last_reason = "keyboard-interactive: server rejected responses".into(),
                Err(KbdAuthError::Cancelled) => return Ok(AuthOutcome::Cancelled),
                Err(KbdAuthError::Failed(reason)) => {
                    last_reason = format!("keyboard-interactive: {reason}")
                }
            }
        }
        (true, None) => {
            not_tried.push(AuthMethodKind::KeyboardInteractive);
            last_reason = "keyboard-interactive: no challenge responder configured".into();
        }
        (false, _) => not_tried.push(AuthMethodKind::KeyboardInteractive),
    }

    if last_reason.is_empty() {
        last_reason = "no authentication methods available".into();
    }
    Ok(AuthOutcome::Failed {
        reason: last_reason,
        methods_remaining: not_tried,
    })
}

/// A cancelled prompt aborts the whole chain; a failure falls through.
enum KbdAuthError {
    Cancelled,
    Failed(String),
}

/// The server's preferred RSA hash, or `None` for non-RSA keys — never a
/// hardcoded algorithm (doc 04).
async fn rsa_hash(handle: &Handle<AppClientHandler>) -> Result<Option<HashAlg>, String> {
    handle
        .best_supported_rsa_hash()
        .await
        .map(Option::flatten)
        .map_err(|e| format!("best_supported_rsa_hash: {e}"))
}

/// Try each identity `SSH_AUTH_SOCK` offers; `Ok(false)` if all are rejected.
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
    for identity in identities {
        // Certificate identities are not supported.
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        let hash_alg = rsa_hash(handle).await?;
        if let Ok(result) = handle
            .authenticate_publickey_with(username, key, hash_alg, &mut agent)
            .await
        {
            if result.success() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn try_key_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    key_path: &str,
    passphrase: Option<&Zeroizing<Vec<u8>>>,
) -> Result<bool, String> {
    let passphrase = passphrase.map(|p| Zeroizing::new(String::from_utf8_lossy(p).into_owned()));
    let key = load_secret_key(
        Path::new(key_path),
        passphrase.as_deref().map(String::as_str),
    )
    .map_err(|e| format!("load key {key_path}: {e}"))?;
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash(handle).await?);
    let result = handle
        .authenticate_publickey(username, key)
        .await
        .map_err(|e| format!("publickey auth: {e}"))?;
    Ok(result.success())
}

async fn try_password_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    password: &Zeroizing<Vec<u8>>,
) -> Result<bool, String> {
    let result = handle
        .authenticate_password(username, String::from_utf8_lossy(password))
        .await
        .map_err(|e| format!("password auth: {e}"))?;
    Ok(result.success())
}

/// Answer `InfoRequest` rounds until the server decides (russh 0.64 drives
/// this from the client side), bounded against a misbehaving server.
async fn try_keyboard_interactive_auth(
    handle: &mut Handle<AppClientHandler>,
    username: &str,
    responder: &ChallengeResponder,
) -> Result<bool, KbdAuthError> {
    const MAX_ROUNDS: usize = 32;
    let mut response = handle
        .authenticate_keyboard_interactive_start(username, None::<String>)
        .await
        .map_err(|e| KbdAuthError::Failed(format!("kbd-interactive start: {e}")))?;
    for _ in 0..MAX_ROUNDS {
        let prompts = match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => prompts,
        };
        let prompts: Vec<ChallengePrompt> = prompts
            .into_iter()
            .map(|p| ChallengePrompt {
                prompt: p.prompt,
                echo: p.echo,
            })
            .collect();
        // russh cannot abort a round mid-flow, so stop answering instead.
        let answers = responder(&prompts).ok_or(KbdAuthError::Cancelled)?;
        response = handle
            .authenticate_keyboard_interactive_respond(answers)
            .await
            .map_err(|e| KbdAuthError::Failed(format!("kbd-interactive respond: {e}")))?;
    }
    Err(KbdAuthError::Failed("too many challenge rounds".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_kinds_display_their_wire_names() {
        let names: Vec<String> = [
            AuthMethodKind::Agent,
            AuthMethodKind::PublicKey,
            AuthMethodKind::Password,
            AuthMethodKind::KeyboardInteractive,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            names,
            ["agent", "publickey", "password", "keyboard-interactive"]
        );
    }

    #[test]
    fn auth_config_debug_redacts_secrets() {
        let config = AuthConfig {
            key_path: Some("/tmp/key".into()),
            key_passphrase: Some(Zeroizing::new(b"supersecret".to_vec())),
            password: Some(Zeroizing::new(b"hunter2".to_vec())),
            try_agent: true,
            try_keyboard_interactive: true,
            challenge_responder: Some(Arc::new(|_| None)),
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(!debug.contains("supersecret") && !debug.contains("hunter2"));
    }
}
