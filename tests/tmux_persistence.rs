//! End-to-end tmux persistence against a real sshd (doc 04 §Tmux Session
//! Persistence, doc 10 Phase 8).
//!
//! Skipped unless the same environment variables as `tests/transport_sshd.rs`
//! are set:
//!   SSH_TEST_HOST, SSH_TEST_PORT, SSH_TEST_USER, SSH_TEST_PASSWORD,
//!   SSH_TEST_KEY_PATH
//!
//! The remote host must have `tmux` installed; the tests probe for it and
//! skip (rather than fail) when it is missing, so the same sshd fixture CI
//! provisions works on hosts without tmux.
//!
//! What is actually proven here:
//!
//! 1. `tmux_enabled` + a session name makes `SSHTransport::connect` exec
//!    `tmux new -A -s wayterm-<name>` instead of a plain shell — the shell
//!    we get back reports `TMUX` set and `tmux display-message -p '#S'`
//!    printing that exact session.
//! 2. Disconnecting the whole transport (closing the channel, dropping the
//!    `SshConnection`) does **not** kill the remote tmux session, and a
//!    reconnect over a brand-new connection re-attaches to it. The proof is
//!    the tmux **session id** (`#{session_id}`, monotonically increasing per
//!    created session) plus a session-scoped environment variable written
//!    before the disconnect: a fresh session would get a new id and no
//!    variable.
//! 3. With tmux disabled, the shell is not inside tmux and the
//!    `wayterm-<name>` session does not exist — the off-path.
//!
//! Tests share one sshd and one tmux server, so the harness runs them with
//! `--test-threads=1`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh::client::{self, Config, Handle};
use tokio::sync::mpsc;
use tokio::time::timeout;

mod app_support {
    pub use wayterm::store::model::{HostKeyPolicy, SessionConfig, SessionType, TMUX_NAME_PREFIX};
    pub use wayterm::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
    pub use wayterm::transport::credentials::{CredentialManager, PromptSource};
    pub use wayterm::transport::hostkey::{HostKeyDecision, HostKeyVerifier};
    pub use wayterm::transport::tab::create_transport;
    pub use wayterm::transport::types::{Transport, TransportEvent, TransportState};
}
use app_support::*;
use wayterm::transport::auth::ChallengePrompt;

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
}

fn read_fixture() -> Option<Fixture> {
    let host = std::env::var("SSH_TEST_HOST").ok()?;
    let port = std::env::var("SSH_TEST_PORT").ok()?.parse().ok()?;
    let user = std::env::var("SSH_TEST_USER").ok()?;
    let password = std::env::var("SSH_TEST_PASSWORD").ok()?;
    Some(Fixture {
        host,
        port,
        user,
        password,
    })
}

fn skip(reason: &str) {
    println!("Skipping tmux persistence test: {reason}");
}

fn accept_all_verifier() -> HostKeyVerifier {
    HostKeyVerifier::new(
        HostKeyPolicy::Off,
        Arc::new(|_| HostKeyDecision::Accept),
        None,
    )
}

async fn connect_authenticated(f: &Fixture) -> Arc<SshConnection> {
    let config = Arc::new(Config::default());
    let addr = SocketAddr::new(
        f.host
            .parse::<IpAddr>()
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        f.port,
    );
    let handler = AppClientHandler {
        host: f.host.clone(),
        port: f.port,
        username: f.user.clone(),
        host_key_verifier: accept_all_verifier(),
        state_tx: None,
    };
    let handle: Handle<AppClientHandler> = client::connect(config, addr, handler)
        .await
        .expect("raw sshd connection failed");
    let connection = SshConnection::new(handle, f.host.clone(), f.port, f.user.clone());

    let password = f.password.clone();
    let creds = CredentialManager::new(vec![Box::new(PromptSource::new(Arc::new(
        move |_session_id, secret_type| {
            (secret_type == "password")
                .then(|| zeroize::Zeroizing::new(password.clone().into_bytes()))
        },
    )))]);
    let password_for_prompt = f.password.clone();
    let responder: wayterm::transport::auth::ChallengeResponder =
        Arc::new(move |prompts: &[ChallengePrompt]| {
            Some(
                prompts
                    .iter()
                    .map(|p| {
                        if p.prompt.to_ascii_lowercase().contains("password") {
                            password_for_prompt.clone()
                        } else {
                            String::new()
                        }
                    })
                    .collect(),
            )
        });

    connection
        .authenticate(
            &AuthMethod::Password(f.password.clone()),
            &creds,
            Some(responder),
        )
        .await
        .expect("authentication failed");
    Arc::new(connection)
}

/// A transport over `connection`, optionally with tmux persistence enabled
/// through the real [`create_transport`] factory — the same path the UI
/// takes, so this test covers the dialog's stored fields as well as the
/// exec'd command.
fn ssh_transport(
    config: &SessionConfig,
    connection: Arc<SshConnection>,
) -> (Box<dyn Transport>, mpsc::Receiver<TransportEvent>) {
    let (tx, rx) = mpsc::channel(256);
    let transport =
        create_transport(config, Some(connection), tx, 24, 80).expect("create_transport");
    (transport, rx)
}

/// A plain-shell transport, used for probing the remote host out of band
/// (`tmux has-session`, `command -v tmux`) so assertions never depend on the
/// tmux session's own echo.
async fn probe_transport(f: &Fixture) -> (Box<dyn Transport>, mpsc::Receiver<TransportEvent>) {
    let connection = connect_authenticated(f).await;
    let config = SessionConfig {
        name: "tmux-probe".to_string(),
        session_type: SessionType::SSH,
        ..Default::default()
    };
    let (mut transport, rx) = ssh_transport(&config, connection);
    transport.connect().await.expect("probe connect failed");
    (transport, rx)
}

/// How long [`run`] keeps draining output after the needle appears. An
/// interactive shell echoes the command line before running it, so a needle
/// that is part of the command text is satisfied by the echo; draining until
/// the remote goes quiet captures the actual result too.
const SETTLE: Duration = Duration::from_millis(750);

/// Run `cmd` in `transport` and return its output.
///
/// Returns once `needle` has been seen and the remote has then been quiet
/// for [`SETTLE`]. Callers must therefore pick a needle that appears in the
/// *result*, not in the echoed command line — split the marker into shell
/// string halves when the command has to contain it.
async fn run(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
    cmd: &str,
    needle: &str,
) -> String {
    transport
        .write(format!("{cmd}\n").as_bytes())
        .await
        .expect("write failed");
    timeout(Duration::from_secs(20), async {
        let mut output = String::new();
        let mut needle_seen = false;
        loop {
            let chunk = match tokio::time::timeout(SETTLE, rx.recv()).await {
                Ok(Some(TransportEvent::DataReceived(bytes))) => bytes,
                Ok(Some(TransportEvent::ConnectionLost)) if needle_seen => break,
                Ok(Some(TransportEvent::ConnectionLost)) => {
                    panic!("connection lost while waiting for '{needle}'")
                }
                Ok(Some(_)) => continue,
                // Quiet: either the needle never came or the command is done.
                Ok(None) | Err(_) => break,
            };
            output.push_str(&String::from_utf8_lossy(&chunk));
            if output.contains(needle) {
                needle_seen = true;
            }
        }
        assert!(
            output.contains(needle),
            "timed out waiting for '{needle}' in output of `{cmd}`; got:\n{output}"
        );
        output
    })
    .await
    .unwrap_or_else(|_| panic!("`{cmd}` produced no terminal output within the deadline"))
}

/// Unique per run so a stale session from an earlier run can never satisfy an
/// assertion, and so parallel test binaries cannot collide.
fn unique_suffix(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    format!("p3tmux-{tag}-{nanos:x}")
}

/// Is `tmux` installed on the remote host? Probed over a plain shell so the
/// answer does not depend on the feature under test.
async fn remote_has_tmux(f: &Fixture) -> bool {
    let (mut transport, mut rx) = probe_transport(f).await;
    // The marker is assembled from two quoted halves so the shell's echo of
    // the command line never contains it verbatim: waiting for the token
    // text itself would match the echo and return before the value arrived.
    let output = run(
        &mut transport,
        &mut rx,
        "echo \"WAYTERM_TMUX\"\"PATH=$(command -v tmux || echo none)\"",
        "WAYTERM_TMUXPATH=",
    )
    .await;
    let _ = transport.disconnect().await;
    let resolved = output
        .rsplit_once("WAYTERM_TMUXPATH=")
        .expect("marker present")
        .1
        .split_whitespace()
        .next()
        .unwrap_or_default();
    println!("remote tmux: {resolved}");
    resolved.ends_with("tmux")
}

/// A `SessionConfig` with tmux persistence on, as the dialog would store it
/// for a session named `name` with the default `wayterm-<name>` remote name.
fn tmux_session_config(name: &str, suffix: &str) -> SessionConfig {
    SessionConfig {
        name: name.to_string(),
        session_type: SessionType::SSH,
        host: Some("127.0.0.1".to_string()),
        port: Some(22),
        username: Some("wayterm-test".to_string()),
        tmux_enabled: true,
        // Stored bare: `SSHTransport` prepends `wayterm-`.
        tmux_session_name: Some(suffix.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn tmux_session_survives_disconnect_and_reconnect_attaches_to_it() {
    let Some(f) = read_fixture() else {
        skip("set SSH_TEST_HOST, SSH_TEST_PORT, SSH_TEST_USER, SSH_TEST_PASSWORD, and SSH_TEST_KEY_PATH");
        return;
    };
    if !remote_has_tmux(&f).await {
        skip("tmux is not installed on the remote host");
        return;
    }

    let suffix = unique_suffix("persist");
    let remote_name = format!("{}{}", TMUX_NAME_PREFIX, suffix);
    let token = format!("wayterm-token-{suffix}");
    let config = tmux_session_config("tmux persist", &suffix);

    // --- First connect: the transport execs `tmux new -A -s wayterm-…` ----
    let first_session_id;
    {
        let connection = connect_authenticated(&f).await;
        let (mut transport, mut rx) = ssh_transport(&config, connection);
        transport.connect().await.expect("connect failed");

        // Inside tmux: TMUX is set, and the shell reports our session name.
        let output = run(
            &mut transport,
            &mut rx,
            "echo \"WAYTERM_TMUX=[${TMUX:-none}]\"",
            "WAYTERM_TMUX=",
        )
        .await;
        assert!(
            !output.contains("WAYTERM_TMUX=[none]"),
            "the shell must be inside a tmux session, got: {output}"
        );

        let output = run(
            &mut transport,
            &mut rx,
            "tmux display-message -p '#{session_name}'",
            &remote_name,
        )
        .await;
        assert!(
            output.contains(&remote_name),
            "display-message must print {remote_name}, got: {output}"
        );

        // Session id is the identity proof: a *newly created* session gets a
        // fresh one. Session-scoped environment is the second proof.
        first_session_id = session_id(&mut transport, &mut rx).await;
        assert!(
            first_session_id.starts_with('$'),
            "session id should look like $N, got {first_session_id}"
        );

        set_session_env(&mut transport, &mut rx, &remote_name, &token).await;

        transport.disconnect().await.expect("disconnect failed");
        assert_eq!(transport.state(), TransportState::Disconnected);
        drop(transport);
    }
    // The `SshConnection` (and the whole TCP session) is gone with the block.

    // --- The tmux session outlived the transport --------------------------
    {
        let (mut transport, mut rx) = probe_transport(&f).await;
        assert!(
            run_ok(
                &mut transport,
                &mut rx,
                &format!("tmux has-session -t {remote_name}")
            )
            .await,
            "{remote_name} must survive the disconnect"
        );
        let _ = transport.disconnect().await;
    }

    // --- Reconnect over a brand-new connection: `-A` re-attaches ---------
    {
        let connection = connect_authenticated(&f).await;
        let (mut transport, mut rx) = ssh_transport(&config, connection);
        transport.connect().await.expect("reconnect failed");

        let reattached_id = session_id(&mut transport, &mut rx).await;
        assert_eq!(
            reattached_id, first_session_id,
            "reconnect must re-attach to the persisted session, not create a new one"
        );

        let output = run(
            &mut transport,
            &mut rx,
            &format!("tmux show-environment -t {remote_name} WAYTERM_PROBE"),
            &token,
        )
        .await;
        assert!(
            output.contains(&token),
            "session state written before the disconnect must still be there, got: {output}"
        );

        transport.disconnect().await.expect("disconnect failed");
    }

    // --- Cleanup: leave no session behind on the shared test host --------
    let (mut transport, mut rx) = probe_transport(&f).await;
    run(
        &mut transport,
        &mut rx,
        &format!("tmux kill-session -t {remote_name} 2>/dev/null; echo WAYTERM_CLEANED"),
        "WAYTERM_CLEANED",
    )
    .await;
    let _ = transport.disconnect().await;
}

/// Run `cmd` on the remote and report whether it succeeded, as a `bool`.
///
/// Used for probes whose answer is the exit status (`tmux has-session`), where
/// there is no output to wait for. The marker is assembled from two quoted
/// halves so the shell's echo of the command never matches it; [`run`] drains
/// until the remote goes quiet, so the real result is always in `output`.
async fn run_ok(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
    cmd: &str,
) -> bool {
    let output = run(
        transport,
        rx,
        &format!("{cmd} && echo \"WAYTERM_\"\"TRUE\" || echo \"WAYTERM_\"\"FALSE\""),
        "WAYTERM_",
    )
    .await;
    // The echoed command line can only produce the first marker; the real
    // verdict is the last one the shell printed.
    output
        .rsplit("WAYTERM_")
        .next()
        .map(|tail| tail.starts_with("TRUE"))
        .unwrap_or(false)
}

/// Read `#{session_id}` out of the *current* tmux session.
async fn session_id(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
) -> String {
    let output = run(
        transport,
        rx,
        "echo \"WAYTERM_SID=$(tmux display-message -p '#{session_id}')\"",
        "WAYTERM_SID=",
    )
    .await;
    output
        .rsplit_once("WAYTERM_SID=")
        .expect("marker present")
        .1
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Store `token` in the current tmux session's environment — state that
/// exists only inside that one tmux session object.
async fn set_session_env(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
    remote_name: &str,
    token: &str,
) {
    let output = run(
        transport,
        rx,
        &format!(
            "tmux set-environment -t {remote_name} WAYTERM_PROBE {token} && echo WAYTERM_SET_OK"
        ),
        "WAYTERM_SET_OK",
    )
    .await;
    assert!(
        output.contains("WAYTERM_SET_OK"),
        "set-environment failed: {output}"
    );
}

#[tokio::test]
async fn tmux_disabled_leaves_the_shell_outside_tmux() {
    let Some(f) = read_fixture() else {
        skip("set SSH_TEST_HOST, SSH_TEST_PORT, SSH_TEST_USER, SSH_TEST_PASSWORD, and SSH_TEST_KEY_PATH");
        return;
    };
    if !remote_has_tmux(&f).await {
        skip("tmux is not installed on the remote host");
        return;
    }

    let suffix = unique_suffix("off");
    let remote_name = format!("{}{}", TMUX_NAME_PREFIX, suffix);
    // Deliberately enabled in the stored row but with no tmux switch turned
    // on by the caller: create_transport only calls set_tmux_enabled when
    // tmux_session_suffix() yields a name, and a disabled row yields None.
    let mut config = tmux_session_config("tmux off", &suffix);
    config.tmux_enabled = false;

    let connection = connect_authenticated(&f).await;
    let (mut transport, mut rx) = ssh_transport(&config, connection);
    transport.connect().await.expect("connect failed");

    let output = run(
        &mut transport,
        &mut rx,
        "echo \"WAYTERM_TMUX=[${TMUX:-none}]\"",
        "WAYTERM_TMUX=",
    )
    .await;
    assert!(
        output.contains("WAYTERM_TMUX=[none]"),
        "with tmux disabled the shell must be a plain shell, got: {output}"
    );

    // …and no wayterm- session was created for it.
    let (mut probe, mut probe_rx) = probe_transport(&f).await;
    assert!(
        !run_ok(
            &mut probe,
            &mut probe_rx,
            &format!("tmux has-session -t {remote_name}")
        )
        .await,
        "no tmux session should exist for a session with the toggle off"
    );

    transport.disconnect().await.expect("disconnect failed");
    let _ = probe.disconnect().await;
}

/// A serial session with the tmux flag on stores it, but no transport is
/// created for it at all — doc 04 scopes the feature to SSH.
#[tokio::test]
async fn tmux_flag_on_a_serial_session_creates_no_transport() {
    let config = SessionConfig {
        name: "console".to_string(),
        session_type: SessionType::Serial,
        serial_device: Some("/dev/ttyUSB0".to_string()),
        tmux_enabled: true,
        tmux_session_name: Some("console".to_string()),
        ..Default::default()
    };

    let (tx, _rx) = mpsc::channel::<TransportEvent>(1);
    let result = create_transport(&config, None, tx, 24, 80);
    assert!(
        result.is_err(),
        "serial sessions have no byte-stream transport (doc 10 Phase 5), so the \
         stored tmux flag is inert"
    );
}
