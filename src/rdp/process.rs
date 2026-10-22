//! The xfreerdp subprocess (doc 08 §RDP): argv builder, binary probe, and
//! the child wrapper that feeds the password and watches the exit.
//! Display-free and `Send`; the embedding lives in `ui::tabs::rdp`.
//!
//! Rules the code depends on (verified on FreeRDP 3.15 and 3.32):
//!
//! - **Password on stdin only**, one line, with `/u` on argv and
//!   `/from-stdin:force` (without `/u`, `:force` reads the password first).
//!   A password is cut at its first newline. Stdin stays open for the whole
//!   session: closing it early aborts with `ERRCONNECT_CONNECT_CANCELLED`.
//!   The password is not even an input to [`build_args`].
//! - **`/sec:rdp:off` always:** Standard RDP Security has no TLS, so a
//!   certificate pin would never be checked and the password would reach a
//!   peer that refused TLS.
//! - **FreeRDP 3 only:** the argv uses 3.x toggle syntax, so 2.x is refused
//!   at probe time ([`is_supported_version`]).
//! - Clipboard, fonts and compression are always spelled out (`+x`/`-x`,
//!   on by default upstream); `/audio` and `/printer` only when enabled.
//! - **Connected** is a heuristic: the child lived [`CONNECTED_DELAY`]
//!   with no `ERRCONNECT_*` line on stderr.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// How long the child must stay alive (with no `ERRCONNECT_*` on stderr)
/// before [`RdpProcessEvent::Connected`] is emitted.
pub const CONNECTED_DELAY: Duration = Duration::from_secs(15);
/// Grace between SIGTERM and SIGKILL in [`RdpProcess::terminate`].
pub const TERMINATE_GRACE: Duration = Duration::from_secs(2);
/// How many trailing stderr lines [`ErrorRing`] keeps.
pub const RING_CAPACITY: usize = 200;
/// Substring marking a FreeRDP connection failure on stderr.
pub const CONNECT_ERROR_MARKER: &str = "ERRCONNECT_";
/// Spawn attempts in [`RdpProcess::run`] before a transient `ETXTBSY`
/// becomes a hard error (bounds the worst case at ~100 ms).
const SPAWN_RETRIES: usize = 20;
/// Delay between spawn retries.
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Refuses the legacy RDP security layer (see the module docs).
pub const SEC_RDP_OFF: &str = "/sec:rdp:off";

/// Binary names probed in order: newDistro name first, legacy second.
pub const BINARY_NAMES: [&str; 2] = ["xfreerdp3", "xfreerdp"];

/// Env allowlist for the child: the X display it needs and the cookie
/// file that authorizes it (`XAUTHORITY` — GDM, SDDM and XWayland keep it
/// outside `~/.Xauthority`, so without it the display refuses xfreerdp),
/// `PATH` (it execs helpers), `HOME` (config/cache lookup), and
/// `XDG_RUNTIME_DIR` (the PulseAudio/PipeWire socket `/audio` plays
/// through). Nothing else is propagated — notably no secrets, which never
/// travel in env anyway; these are all paths.
const ALLOWED_ENV: [&str; 5] = ["DISPLAY", "XAUTHORITY", "PATH", "HOME", "XDG_RUNTIME_DIR"];

/// Everything xfreerdp needs except the password (which only ever goes to
/// stdin — see the module docs). `cert_arg` is the [`crate::rdp::cert`]
/// token (`Some("/cert:fingerprint:sha256:<hex>")`), if resolved.
#[derive(Debug, Clone)]
pub struct RdpSpec {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub domain: Option<String>,
    pub width: u32,
    pub height: u32,
    pub depth: Option<u32>,
    pub audio: bool,
    pub clipboard: bool,
    pub fonts: bool,
    pub printer: bool,
    pub compression: bool,
    pub cert_arg: Option<String>,
}

/// Build the xfreerdp argv for `spec` (see the module docs for the
/// contract). `parent_xid` is the container's X11 window id when embedding;
/// `None` opens a top-level window.
pub fn build_args(spec: &RdpSpec, parent_xid: Option<u32>) -> Vec<String> {
    let mut args = vec![
        format!("/v:{}:{}", spec.host, spec.port),
        format!("/u:{}", spec.username),
    ];
    if let Some(domain) = &spec.domain {
        args.push(format!("/d:{domain}"));
    }
    args.push(format!("/w:{}", spec.width));
    args.push(format!("/h:{}", spec.height));
    if let Some(depth) = spec.depth {
        args.push(format!("/bpp:{depth}"));
    }
    args.push("/from-stdin:force".to_string());
    // Never negotiate down to Standard RDP Security: it skips TLS, and with
    // it the fingerprint pin (module docs).
    args.push(SEC_RDP_OFF.to_string());
    args.push(toggle(spec.clipboard, "clipboard"));
    args.push(toggle(spec.fonts, "fonts"));
    if spec.printer {
        args.push("/printer".to_string());
    }
    if spec.audio {
        args.push("/audio".to_string());
    }
    args.push(toggle(spec.compression, "compression"));
    if let Some(xid) = parent_xid {
        args.push(format!("/parent-window:{xid}"));
    }
    // Last gate before spawn: only a fingerprint token is ever passed.
    // Anything else (notably `/cert:ignore`, which the trust model forbids)
    // is dropped, never forwarded.
    if let Some(cert_arg) = &spec.cert_arg {
        if cert_arg.starts_with(crate::rdp::cert::CERT_ARG_PREFIX) {
            args.push(cert_arg.clone());
        }
    }
    args
}

/// A bool option spelled out either way: `+name` or `-name`.
fn toggle(on: bool, name: &str) -> String {
    format!("{}{name}", if on { '+' } else { '-' })
}

/// The oldest FreeRDP major version the argv contract speaks (module docs).
pub const MIN_FREERDP_MAJOR: u32 = 3;

/// True when `version` (dotted digits, as [`parse_version`] returns them)
/// is FreeRDP [`MIN_FREERDP_MAJOR`] or later. An unreadable version is
/// unsupported: without evidence that the argv will be understood, it is
/// not run.
pub fn is_supported_version(version: &str) -> bool {
    version
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= MIN_FREERDP_MAJOR)
}

/// Parse `<bin> --version` output. Accepts the parenthesized dotted triple
/// first (e.g. `(3.15.0)`), falling back to a bare `version X.Y[.Z]`
/// (`This is FreeRDP version 3.15.1 (2681e62)` carries the triple outside
/// the parens, which hold a git revision). Returns the version digits only.
pub fn parse_version(output: &str) -> Option<String> {
    parse_paren_version(output).or_else(|| parse_bare_version(output))
}

fn is_dotted_version(candidate: &str) -> bool {
    let parts: Vec<&str> = candidate.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// First parenthesized dotted triple in `output`.
fn parse_paren_version(output: &str) -> Option<String> {
    let mut rest = output;
    while let Some(open) = rest.find('(') {
        let after = &rest[open + 1..];
        if let Some(close) = after.find(')') {
            let candidate = &after[..close];
            if is_dotted_version(candidate) {
                return Some(candidate.to_string());
            }
            rest = &after[close + 1..];
        } else {
            break;
        }
    }
    None
}

/// First `version <triple>` in `output` (case-insensitive on the keyword).
fn parse_bare_version(output: &str) -> Option<String> {
    let lowered = output.to_lowercase();
    let mut search = lowered.as_str();
    while let Some(at) = search.find("version") {
        let after = &search[at + "version".len()..];
        if let Some(token) = after
            .split_whitespace()
            .next()
            // The token comes from the lowercased copy, but version digits
            // are case-invariant so the value is exact.
            .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.'))
        {
            if is_dotted_version(token) {
                return Some(token.to_string());
            }
        }
        search = after;
    }
    None
}

/// Search `path_env` (a `PATH`-formatted value) for [`BINARY_NAMES`],
/// newest name first. Factored out of [`probe_binary`] so tests can pass a
/// fake `PATH` without touching the process environment.
pub fn find_binary_in(path_env: Option<&OsStr>) -> Option<String> {
    let path_env = path_env?;
    for name in BINARY_NAMES {
        for dir in std::env::split_paths(path_env) {
            let candidate = dir.join(name);
            if candidate.is_file() && is_executable(&candidate) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Probe `xfreerdp3`, then `xfreerdp`, on `PATH`; run `<bin> --version` and
/// return `(binary_name, version)`. Blocking (one process spawn): call once
/// at startup, preferably on a blocking thread — never per frame.
///
/// Both stdout and stderr are scanned: `--version` has printed to either
/// across FreeRDP releases.
pub fn probe_binary() -> Option<(String, String)> {
    let name = find_binary_in(std::env::var_os("PATH").as_deref())?;
    let output = std::process::Command::new(&name)
        .arg("--version")
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    parse_version(&text).map(|version| (name, version))
}

/// Minimal child environment: `ALLOWED_ENV` entries copied from our own
/// env when present. Everything else is dropped (`env_clear` at spawn).
pub fn minimal_env() -> Vec<(String, String)> {
    ALLOWED_ENV
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| ((*key).to_string(), v)))
        .collect()
}

/// What the running child reports. `Connected` fires at most once (see the
/// liveness heuristic in the module docs); `Ended` is always last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdpProcessEvent {
    Connected,
    Ended {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Map a wait status to the terminal [`RdpProcessEvent::Ended`]. Pure so
/// the exit mapping is testable without spawning anything.
pub fn exit_event(status: &std::process::ExitStatus) -> RdpProcessEvent {
    RdpProcessEvent::Ended {
        code: status.code(),
        signal: status.signal(),
    }
}

/// The last [`RING_CAPACITY`] stderr lines. Never carries the password —
/// the password is written only to stdin, never to any logged stream.
#[derive(Debug, Default)]
pub struct ErrorRing {
    lines: VecDeque<String>,
}

impl ErrorRing {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, line: String) {
        if self.lines.len() >= RING_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Oldest-first snapshot of the retained lines.
    pub fn digest(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    /// True once any retained line carries a FreeRDP connection error.
    pub fn has_connect_error(&self) -> bool {
        self.lines.iter().any(|l| l.contains(CONNECT_ERROR_MARKER))
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Owns one xfreerdp child: its pid (so the UI can only ever kill its own
/// session), its still-open stdin, and its stderr ring. Tokio-side only.
///
/// [`run`](Self::run) borrows `&mut self` for the whole session, so when
/// the wrapper is shared behind an `Arc<tokio::sync::Mutex<_>>` the runner
/// task holds the lock until the child exits. To stop a live session,
/// abort the runner task first — dropping the `run` future leaves the child
/// in `self` — then [`terminate`](Self::terminate) signals and reaps it.
/// Dropping the wrapper SIGKILLs the child (`kill_on_drop`) as a backstop.
pub struct RdpProcess {
    binary: String,
    argv: Vec<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    ring: ErrorRing,
    connected_delay: Duration,
    terminate_grace: Duration,
}

impl RdpProcess {
    /// Build (not spawn) the wrapper: argv comes from [`build_args`].
    /// `binary` is usually [`probe_binary`]'s name; tests pass a fake.
    pub fn new(binary: impl Into<String>, spec: &RdpSpec, parent_xid: Option<u32>) -> Self {
        let binary = binary.into();
        let argv = build_args(spec, parent_xid);
        Self {
            binary,
            argv,
            child: None,
            stdin: None,
            ring: ErrorRing::new(),
            connected_delay: CONNECTED_DELAY,
            terminate_grace: TERMINATE_GRACE,
        }
    }

    /// Override [`CONNECTED_DELAY`] (test hook; also useful for fast rigs).
    pub fn with_connected_delay(mut self, delay: Duration) -> Self {
        self.connected_delay = delay;
        self
    }

    /// Override [`TERMINATE_GRACE`] (test hook).
    pub fn with_terminate_grace(mut self, grace: Duration) -> Self {
        self.terminate_grace = grace;
        self
    }

    pub fn binary(&self) -> &str {
        &self.binary
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Oldest-first snapshot of the retained stderr lines.
    pub fn error_digest(&self) -> Vec<String> {
        self.ring.digest()
    }

    /// Spawn the child, feed it the password as ONE stdin line, then watch
    /// it until exit, emitting [`RdpProcessEvent`]s. The in-memory password
    /// copy is zeroized right after the write; the pipe itself stays open
    /// for the whole session (closing early aborts the connect — see the
    /// module docs).
    pub async fn run(
        &mut self,
        password: &Zeroizing<Vec<u8>>,
        events: mpsc::Sender<RdpProcessEvent>,
    ) -> io::Result<()> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(&self.argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(minimal_env())
            .kill_on_drop(true)
            // New process group: terminate() signals the group, so orphaned
            // grandchildren of our child die with it — and we can never
            // signal anything outside our own session.
            .process_group(0);
        // Exec can transiently fail with ETXTBSY while the binary is being
        // replaced on disk (a package update swapping xfreerdp mid-connect
        // — or a freshly-written helper racing concurrent forks). Retry a
        // few times; a persistent failure still surfaces as an error.
        let mut attempts = 1;
        let mut child = loop {
            match cmd.spawn() {
                Ok(child) => break child,
                Err(e)
                    if e.kind() == io::ErrorKind::ExecutableFileBusy
                        && attempts < SPAWN_RETRIES =>
                {
                    attempts += 1;
                    tokio::time::sleep(SPAWN_RETRY_DELAY).await;
                }
                Err(e) => return Err(e),
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "xfreerdp stdin not piped"))?;
        self.stdin = Some(stdin);

        // ONE line: truncate at the first CR/LF (a password cannot contain
        // a newline — the rest is not a second credential line, it is cut),
        // then LF. The Zeroizing line is dropped (zeroized) after the write;
        // the pipe handle stays open above.
        let end = password
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .unwrap_or(password.len());
        let mut line = Zeroizing::new(Vec::with_capacity(end + 1));
        line.extend_from_slice(&password[..end]);
        line.push(b'\n');
        let write_result = async {
            let stdin = self.stdin.as_mut().expect("stdin just stored");
            stdin.write_all(&line).await?;
            stdin.flush().await
        }
        .await;
        drop(line);
        if let Err(e) = write_result {
            // The child is already gone or refusing input: stop it and
            // report, rather than waiting out a hung session.
            self.stdin.take();
            let _ = child.kill().await;
            return Err(e);
        }

        let stderr: ChildStderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "xfreerdp stderr not piped")
        })?;
        self.child = Some(child);
        let mut lines = BufReader::new(stderr).lines();
        let deadline = tokio::time::sleep(self.connected_delay);
        tokio::pin!(deadline);
        let mut timer_done = false;
        let mut stderr_done = false;
        let mut connected = false;

        loop {
            let child = self.child.as_mut().expect("child just stored");
            tokio::select! {
                line = lines.next_line(), if !stderr_done => {
                    match line {
                        Ok(Some(text)) => self.ring.push(text),
                        // EOF or read error: stop polling stderr, keep
                        // waiting on the timer and the child below.
                        Ok(None) | Err(_) => stderr_done = true,
                    }
                }
                _ = &mut deadline, if !timer_done => {
                    timer_done = true;
                }
                status = child.wait() => {
                    let status = status?;
                    // A fast-exiting child can beat the stderr arm: the
                    // pipe still holds unread lines when wait() returns.
                    // Drain them into the ring (EOF ends it) before
                    // reporting the exit, so error digests are complete.
                    while let Ok(line) = lines.next_line().await {
                        match line {
                            Some(text) => self.ring.push(text),
                            None => break,
                        }
                    }
                    let _ = events.send(exit_event(&status)).await;
                    break;
                }
            }
            if timer_done && !connected {
                connected = true;
                let alive = self
                    .child
                    .as_mut()
                    .expect("child just stored")
                    .try_wait()?
                    .is_none();
                if alive && !self.ring.has_connect_error() {
                    let _ = events.send(RdpProcessEvent::Connected).await;
                }
            }
        }
        self.stdin.take();
        Ok(())
    }

    /// SIGTERM the child's process group, then SIGKILL after the grace
    /// period. Only ever signals our own child's group (see spawn). Needs
    /// the wrapper's lock, which a live [`run`](Self::run) holds: abort the
    /// runner task first (see the type docs).
    pub async fn terminate(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        // Already reaped (or never spawned): nothing to signal.
        match child.try_wait() {
            Ok(None) => {}
            _ => return,
        }
        signal_group(child, libc::SIGTERM);
        if tokio::time::timeout(self.terminate_grace, child.wait())
            .await
            .is_err()
        {
            signal_group(child, libc::SIGKILL);
            let _ = child.wait().await;
        }
        self.stdin.take();
    }
}

/// Signal the child's whole process group (created at spawn, so this can
/// never reach anything outside our own session). A reaped child has no
/// pid and is left alone.
fn signal_group(child: &Child, signal: libc::c_int) {
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2); the negative pid names the group we made.
        unsafe {
            libc::kill(-(pid as libc::pid_t), signal);
        }
    }
}

#[cfg(test)]
mod tests;
