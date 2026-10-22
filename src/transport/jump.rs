//! Jump hosts, one level deep (doc 04 §Jump hosts).
//!
//! A session with `jump_session_id` dials by opening a direct-tcpip channel
//! on the jump hop's [`SshConnection`] to the target and running the
//! target's SSH session over that stream (`russh-config` ProxyCommand
//! style). Each hop authenticates on its own; host-key verification runs on
//! the inner hop through the same [`AppClientHandler`] path, never skipped.
//!
//! [`dial_with_jump`] is the connect flow's entry: it resolves both hops
//! from their rows, checks depth, dials the jump hop through the
//! supervisor, and hands it to [`dial_via_jump`], which runs the target
//! over the live jump connection (testable against one sshd). The inner
//! connection is never pooled: the caller owns it outright.

use std::process::Stdio;
use std::sync::Arc;

use russh::client::{self, Config};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::store::model::{AuthMethod as StoredMethod, SessionConfig};
use crate::transport::connection::{AppClientHandler, AuthMethod, SshConnection};
use crate::transport::supervisor::{
    authenticated, connect_error, ConnectParams, ConnectionSupervisor, HopAuth,
};
use crate::transport::types::TransportError;

/// Depth and self-jump checks (doc 04: depth ≤ 1). Pure so the connect flow
/// and tests can call it without dialing anything.
pub fn check_jump_chain(
    jump_config: &SessionConfig,
    target_config: &SessionConfig,
) -> Result<(), TransportError> {
    if jump_config.jump_session_id.is_some() {
        return Err(TransportError::ConnectionFailed(format!(
            "jump depth > 1 is not supported: jump session '{}' itself jumps \
             (v1 allows one hop, doc 04)",
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

/// Map the stored method onto the auth chain's input, for any hop, jumped
/// or not. Secrets are never on the row: the chain asks the credential
/// manager at connect time.
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
/// jump hop through the supervisor with `jump_auth`, then dial the target
/// over it with `target_auth` — each hop authenticating as its own session
/// (doc 04). Cancellation from either hop's auth propagates as
/// [`TransportError::Cancelled`].
///
/// The returned connection is NOT pooled: it is backed by a jump channel,
/// not a TCP endpoint the pool key could describe.
pub async fn dial_with_jump(
    supervisor: &ConnectionSupervisor,
    jump_config: &SessionConfig,
    jump_auth: &HopAuth,
    target_config: &SessionConfig,
    target_auth: &HopAuth,
) -> Result<Arc<SshConnection>, TransportError> {
    check_jump_chain(jump_config, target_config)?;
    let jump = hop_params(jump_config)?;
    let jump_conn = supervisor.get_or_connect(&jump, jump_auth).await?;
    let target = hop_params(target_config)?;
    let ssh_w_fallback = target_config
        .ssh_options
        .as_ref()
        .is_some_and(|o| o.ssh_w_fallback);
    let inner = dial_via_jump(&jump_conn, &jump, &target, target_auth, ssh_w_fallback).await?;
    Ok(Arc::new(inner))
}

/// Dial `target` over an already-connected jump hop: open direct-tcpip to
/// it, run its SSH session over the channel stream with the SAME dial
/// semantics as a direct dial (host-key verification + auth chain via the
/// supervisor's shared helpers), and return the target's connection.
///
/// When the direct-tcpip open fails and `ssh_w_fallback` is set
/// (`sessions.ssh_options.ssh_w_fallback`), spawn `ssh -W` as the stream
/// instead (the child is transport only — the inner hop still
/// authenticates through the stream). The fallback triggers on
/// [`TransportError::ConnectionFailed`] as well as
/// [`TransportError::ChannelError`]: `open_direct_tcpip` maps russh errors
/// through `TransportError::from_russh` (always `ConnectionFailed`), so
/// matching `ChannelError` alone would leave the fallback dead code.
pub async fn dial_via_jump(
    jump_conn: &Arc<SshConnection>,
    jump: &ConnectParams,
    target: &ConnectParams,
    auth: &HopAuth,
    ssh_w_fallback: bool,
) -> Result<SshConnection, TransportError> {
    match jump_conn.open_direct_tcpip(&target.host, target.port).await {
        Ok(channel) => connect_inner(channel.into_stream(), target, auth).await,
        Err(err @ (TransportError::ChannelError(_) | TransportError::ConnectionFailed(_)))
            if ssh_w_fallback =>
        {
            eprintln!("Wayterm: jump: direct-tcpip rejected ({err}); falling back to ssh -W");
            let stream = SshWStream::spawn(jump, &target.host, target.port).await?;
            connect_inner(stream, target, auth).await
        }
        Err(err) => Err(err),
    }
}

/// Run the target's SSH session over `stream`: full host-key verification
/// (never skipped) plus the standard auth chain, as a direct dial does.
async fn connect_inner<S>(
    stream: S,
    target: &ConnectParams,
    auth: &HopAuth,
) -> Result<SshConnection, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let handler = AppClientHandler::new(target.host.clone(), target.port, auth.verifier.clone());
    let handle = client::connect_stream(Arc::new(Config::default()), stream, handler)
        .await
        .map_err(|err| connect_error(&target.host, err))?;
    authenticated(handle, target, auth).await
}

/// `ssh -W host:port [user@]host` argv. No secrets ever appear here: the
/// child is only the transport, auth happens through the stream.
/// `BatchMode=yes` keeps OpenSSH from ever prompting on a tty Wayterm does
/// not own (doc 04).
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
    /// Write half: bytes to the target.
    stdin: ChildStdin,
    /// Read half: bytes from the target.
    stdout: ChildStdout,
    /// Killed when the stream drops.
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
    /// Run `ssh -W target:port [user@]jump`, piped.
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
