//! Shared helpers for the suites that run against a real sshd. They skip
//! unless `SSH_TEST_HOST`, `SSH_TEST_PORT`, `SSH_TEST_USER` and
//! `SSH_TEST_PASSWORD` (plus `SSH_TEST_KEY_PATH` for key auth) are set;
//! `scripts/run-sshd-tests.sh` provisions a throwaway sshd and sets them.

// Each suite uses a different subset.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Config, Handle};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use zeroize::Zeroizing;

use wayterm::sftp::{SftpClient, SftpCommand};
use wayterm::store::model::HostKeyPolicy;
use wayterm::transport::auth::{ChallengePrompt, ChallengeResponder};
use wayterm::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
use wayterm::transport::credentials::{CredentialManager, PromptSource};
use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
use wayterm::transport::supervisor::HopAuth;
use wayterm::transport::types::TransportEvent;

pub struct Fixture {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub key_path: Option<String>,
}

/// The fixture, or `None` after printing the skip notice
/// (`scripts/run-sshd-tests.sh` greps for its wording).
pub fn fixture() -> Option<Fixture> {
    let read = || {
        Some(Fixture {
            host: std::env::var("SSH_TEST_HOST").ok()?,
            port: std::env::var("SSH_TEST_PORT").ok()?.parse().ok()?,
            user: std::env::var("SSH_TEST_USER").ok()?,
            password: std::env::var("SSH_TEST_PASSWORD").ok()?,
            key_path: std::env::var("SSH_TEST_KEY_PATH").ok(),
        })
    };
    let fixture = read();
    if fixture.is_none() {
        println!(
            "Skipping sshd integration test: set SSH_TEST_HOST, SSH_TEST_PORT, \
             SSH_TEST_USER, SSH_TEST_PASSWORD, and SSH_TEST_KEY_PATH to run it."
        );
    }
    fixture
}

pub fn accept_all_verifier() -> HostKeyVerifier {
    HostKeyVerifier::new(
        HostKeyPolicy::Off,
        Arc::new(|_| HostKeyDecision::Accept),
        None,
    )
}

/// Credentials that answer the password lookup (and nothing else).
pub fn password_creds(password: &str) -> CredentialManager {
    let password = password.to_string();
    CredentialManager::new(vec![Box::new(PromptSource::new(Arc::new(
        move |_, secret_type| {
            (secret_type == "password").then(|| Zeroizing::new(password.clone().into_bytes()))
        },
    )))])
}

/// Answers password-shaped prompts (including the chain's retry) with
/// `password`, anything else with an empty string.
pub fn password_responder(password: &str) -> ChallengeResponder {
    let password = password.to_string();
    Arc::new(move |prompts: &[ChallengePrompt]| {
        Some(
            prompts
                .iter()
                .map(|p| {
                    if p.prompt.to_ascii_lowercase().contains("password") {
                        password.clone()
                    } else {
                        String::new()
                    }
                })
                .collect(),
        )
    })
}

/// Password auth with `password`, for the lookup and the chain's retry
/// prompt alike, accepting any host key.
pub fn password_auth(password: &str) -> HopAuth {
    HopAuth {
        method: AuthMethod::Password(String::new()),
        creds: password_creds(password),
        verifier: accept_all_verifier(),
        responder: Some(password_responder(password)),
    }
}

/// No method, no credentials, no prompts: for guards that fire before any
/// dial.
pub fn no_auth() -> HopAuth {
    HopAuth {
        method: AuthMethod::None,
        creds: CredentialManager::new(vec![]),
        verifier: accept_all_verifier(),
        responder: None,
    }
}

/// Answers every prompt with `answer` (keyboard-interactive via PAM).
pub fn answer_all(answer: &str) -> ChallengeResponder {
    let answer = answer.to_string();
    Arc::new(move |prompts: &[ChallengePrompt]| Some(vec![answer.clone(); prompts.len()]))
}

/// A raw, unauthenticated session that accepts any host key.
pub async fn connect_raw_to(host: &str, port: u16) -> Handle<AppClientHandler> {
    let addr = SocketAddr::new(
        host.parse().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        port,
    );
    let handler = AppClientHandler::new(host.into(), port, accept_all_verifier());
    client::connect(Arc::new(Config::default()), addr, handler)
        .await
        .expect("raw sshd connection failed")
}

pub async fn connect_raw(f: &Fixture) -> Handle<AppClientHandler> {
    connect_raw_to(&f.host, f.port).await
}

/// An authenticated connection using `method`.
pub async fn connect_with(
    f: &Fixture,
    method: AuthMethod,
    creds: CredentialManager,
    responder: ChallengeResponder,
) -> Arc<SshConnection> {
    let handle = connect_raw(f).await;
    let connection = SshConnection::new(handle, f.host.clone(), f.port, f.user.clone());
    connection
        .authenticate(&method, &creds, Some(responder))
        .await
        .expect("authentication failed");
    Arc::new(connection)
}

/// An authenticated connection using the fixture's password.
pub async fn connect_password(f: &Fixture) -> Arc<SshConnection> {
    connect_with(
        f,
        AuthMethod::Password(String::new()),
        password_creds(&f.password),
        password_responder(&f.password),
    )
    .await
}

/// Read transport output until `needle` shows up (10 s at most).
pub async fn collect_echo(rx: &mut mpsc::Receiver<TransportEvent>, needle: &str) -> String {
    let mut output = String::new();
    let found = timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(TransportEvent::DataReceived(bytes)) => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                    if output.contains(needle) {
                        return;
                    }
                }
                Some(TransportEvent::ConnectionLost) => {
                    panic!("connection lost before seeing '{needle}'")
                }
                Some(_) => {}
                None => panic!("event channel closed before seeing '{needle}'"),
            }
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("timed out waiting for '{needle}'"));
    output
}

/// Send an SFTP command and wait for its reply.
pub async fn sftp_cmd<T>(
    client: &SftpClient,
    make: impl FnOnce(oneshot::Sender<Result<T, String>>) -> SftpCommand,
) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    client
        .request(make(tx))
        .await
        .expect("sftp task queue closed");
    timeout(Duration::from_secs(30), rx)
        .await
        .expect("sftp command timed out")
        .expect("sftp task ended before replying")
}

/// Unique per run, so leftovers of an earlier run never match.
pub fn unique_tag() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    )
}
