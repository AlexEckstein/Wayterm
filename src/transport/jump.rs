//! Jump hosts, one level deep (doc 11 §Jump hosts).
//!
//! A session with `jump_session_id` dials by opening a direct-tcpip channel
//! on the jump hop's [`SshConnection`] to the target and running the
//! target's SSH session over that stream (`russh-config` ProxyCommand
//! style). Each hop authenticates on its own; host-key verification runs on
//! the inner hop through the same [`AppClientHandler`] path, never skipped.
//!
//! Shape: [`dial_with_jump`] is the config-level entry the connect flow
//! will call (it resolves both hops from their [`SessionConfig`] rows,
//! checks depth, dials the jump hop through the supervisor, then hands the
//! live jump connection to [`dial_via_jump`]). [`dial_via_jump`] takes an
//! already-connected jump [`SshConnection`], which keeps it testable
//! against a single sshd and keeps the supervisor pool out of the inner
//! (stream-backed) connection — the inner connection is intentionally NOT
//! pooled, so the caller owns the returned [`SshConnection`] outright.

use std::process::Stdio;
use std::sync::Arc;

use russh::client::{self, Config};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::store::model::{AuthMethod as StoredMethod, SessionConfig};
use crate::transport::auth::ChallengeResponder;
use crate::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
use crate::transport::credentials::CredentialManager;
use crate::transport::hostkey::HostKeyVerifier;
use crate::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use crate::transport::types::TransportError;

/// Per-hop secrets and prompts. Each hop authenticates with its own
/// session's method and credentials — the jump hop's auth is the jump
/// session's, the inner's is the target's.
pub struct HopInputs<'a> {
    pub creds: &'a CredentialManager,
    pub verifier: HostKeyVerifier,
    pub responder: Option<ChallengeResponder>,
}

/// Everything [`dial_via_jump`] needs about the target hop.
pub struct JumpTarget<'a> {
    pub params: ConnectParams,
    pub method: AuthMethod,
    pub creds: &'a CredentialManager,
    pub verifier: HostKeyVerifier,
    pub responder: Option<ChallengeResponder>,
    /// `sessions.ssh_options.ssh_w_fallback`: when the jump hop rejects
    /// direct-tcpip, spawn `ssh -W` as the stream instead.
    pub ssh_w_fallback: bool,
}

/// Depth and self-jump checks (doc 11: depth ≤ 1). Pure so the connect flow
/// and tests can call it without dialing anything.
pub fn check_jump_chain(
    jump_config: &SessionConfig,
    target_config: &SessionConfig,
) -> Result<(), TransportError> {
    if jump_config.jump_session_id.is_some() {
        return Err(TransportError::ConnectionFailed(format!(
            "jump depth > 1 is not supported: jump session '{}' itself jumps \
             (v1 allows one hop, doc 11)",
            jump_config.name,
        )));
    }
    if target_config.id == jump_config.id {
        return Err(TransportError::ConnectionFailed(format!(
            "session '{}' cannot jump through itself",
            target_config.name,
        )));
    }
    Ok(())
}

/// Where a hop dials: host, port (default 22), username. A row missing
/// either is a connect error, never a dial to the wrong place.
pub fn hop_params(config: &SessionConfig) -> Result<ConnectParams, TransportError> {
    let host = config
        .host
        .clone()
        .filter(|h| !h.trim().is_empty())
        .ok_or_else(|| {
            TransportError::ConnectionFailed(format!(
                "session '{}' has no host configured",
                config.name
            ))
        })?;
    let username = config
        .username
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            TransportError::ConnectionFailed(format!(
                "session '{}' has no username configured",
                config.name
            ))
        })?;
    Ok(ConnectParams {
        host,
        port: config.port.unwrap_or(22),
        username,
    })
}

/// Map the stored method onto the auth chain's input. Mirrors
/// `ui::tab::transport_auth_method`: secrets are never on the row, the
/// chain asks the credential manager at connect time.
pub fn hop_auth_method(config: &SessionConfig) -> AuthMethod {
    match config.auth_method.clone().unwrap_or(StoredMethod::Password) {
        StoredMethod::Password => AuthMethod::Password(String::new()),
        StoredMethod::Key => AuthMethod::Key {
            path: config.key_path.clone().unwrap_or_default(),
            passphrase: None,
        },
        StoredMethod::Agent => AuthMethod::Agent,
        StoredMethod::KeyboardInteractive => AuthMethod::KeyboardInteractive,
        StoredMethod::None => AuthMethod::None,
    }
}

/// Config-level jump dial for the connect flow: check the chain, dial the
/// jump hop through the supervisor with the jump session's own params/auth,
/// then dial the target over it. Cancellation from either hop's auth
/// propagates as [`TransportError::Cancelled`].
///
/// The returned connection is NOT pooled: it is backed by a jump channel,
/// not a TCP endpoint the pool key could describe.
pub async fn dial_with_jump(
    supervisor: &ConnectionSupervisor,
    jump_config: &SessionConfig,
    jump_auth: HopInputs<'_>,
    target_config: &SessionConfig,
    target_auth: HopInputs<'_>,
) -> Result<Arc<SshConnection>, TransportError> {
    check_jump_chain(jump_config, target_config)?;
    let jump_params = hop_params(jump_config)?;
    let jump_method = hop_auth_method(jump_config);
    let jump_conn = supervisor
        .get_or_connect(
            &jump_params,
            &jump_method,
            jump_auth.creds,
            jump_auth.verifier,
            jump_auth.responder,
        )
        .await?;
    let target = JumpTarget {
        params: hop_params(target_config)?,
        method: hop_auth_method(target_config),
        creds: target_auth.creds,
        verifier: target_auth.verifier,
        responder: target_auth.responder,
        ssh_w_fallback: target_config
            .ssh_options
            .as_ref()
            .is_some_and(|o| o.ssh_w_fallback),
    };
    let inner = dial_via_jump(&jump_conn, &jump_params, target).await?;
    Ok(Arc::new(inner))
}

/// Dial the target over an already-connected jump hop: open direct-tcpip
/// to the target, run the target's SSH session over the channel stream
/// with the SAME dial semantics as a direct dial (host-key verification +
/// auth chain via the supervisor's shared helpers), and return the
/// target's connection.
///
/// When the direct-tcpip open fails and `target.ssh_w_fallback` is set,
/// spawn `ssh -W` as the stream instead (the child is transport only —
/// the inner hop still authenticates through the stream). The fallback
/// triggers on [`TransportError::ConnectionFailed`] as well as
/// [`TransportError::ChannelError`]: `open_direct_tcpip` maps russh errors
/// through `TransportError::from_russh` (always `ConnectionFailed`), so
/// matching `ChannelError` alone would leave the fallback dead code.
pub async fn dial_via_jump(
    jump_conn: &Arc<SshConnection>,
    jump: &ConnectParams,
    target: JumpTarget<'_>,
) -> Result<SshConnection, TransportError> {
    let direct = jump_conn
        .open_direct_tcpip(&target.params.host, target.params.port)
        .await;
    match direct {
        Ok(channel) => connect_inner(channel.into_stream(), &target).await,
        Err(err @ (TransportError::ChannelError(_) | TransportError::ConnectionFailed(_)))
            if target.ssh_w_fallback =>
        {
            eprintln!("Wayterm: jump: direct-tcpip rejected ({err}); falling back to ssh -W");
            let stream = SshWStream::spawn(jump, &target.params.host, target.params.port).await?;
            connect_inner(stream, &target).await
        }
        Err(err) => Err(err),
    }
}

/// Run the target's SSH session over `stream`: full host-key verification
/// (never skipped) plus the standard auth chain, mirroring `dial_ssh`.
async fn connect_inner<S>(
    stream: S,
    target: &JumpTarget<'_>,
) -> Result<SshConnection, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let config = Arc::new(Config::default());
    let handler = AppClientHandler::new(
        target.params.host.clone(),
        target.params.port,
        target.params.username.clone(),
        target.verifier.clone(),
        None,
    );
    let handle = client::connect_stream(config, stream, handler)
        .await
        .map_err(|err| crate::transport::supervisor::connect_error(&target.params.host, err))?;
    crate::transport::supervisor::wrap_and_authenticate(
        handle,
        &target.params,
        &target.method,
        target.creds,
        target.responder.clone(),
    )
    .await
}

/// `ssh -W host:port [user@]host` argv. No secrets ever appear here: the
/// child is only the transport, auth happens through the stream.
/// `BatchMode=yes` keeps OpenSSH from ever prompting on a tty Wayterm does
/// not own (doc 11).
pub(crate) fn ssh_w_argv(jump: &ConnectParams, target_host: &str, target_port: u16) -> Vec<String> {
    let mut argv = vec![
        "-W".to_string(),
        format!("{target_host}:{target_port}"),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
    ];
    if jump.port != 22 {
        argv.push("-p".to_string());
        argv.push(jump.port.to_string());
    }
    argv.push(format!("{}@{}", jump.username, jump.host));
    argv
}

/// An `ssh -W` child as a byte stream: stdin is the write half, stdout the
/// read half. Lifecycle: `kill_on_drop` ties the child to the stream, and
/// the stream is owned by the russh session — when the connection drops,
/// the stream drops and the child is killed. Auth material never touches
/// the child (see [`ssh_w_argv`]).
struct SshWStream {
    stdin: ChildStdin,
    stdout: ChildStdout,
    child: Child,
}

impl std::fmt::Debug for SshWStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshWStream")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl SshWStream {
    async fn spawn(
        jump: &ConnectParams,
        target_host: &str,
        target_port: u16,
    ) -> Result<Self, TransportError> {
        let mut child = Command::new("ssh")
            .args(ssh_w_argv(jump, target_host, target_port))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                TransportError::ConnectionFailed(format!("ssh -W fallback failed to spawn: {err}"))
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            TransportError::ConnectionFailed("ssh -W fallback: no stdin pipe".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            TransportError::ConnectionFailed("ssh -W fallback: no stdout pipe".to_string())
        })?;
        Ok(Self {
            stdin,
            stdout,
            child,
        })
    }
}

impl AsyncRead for SshWStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshWStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
