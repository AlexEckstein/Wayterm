//! xfreerdp subprocess wrapper (doc 08 §RDP). Display-free and tokio-side:
//! no GTK types, no X11 types — everything here is `Send`. The embedding
//! (reparenting into a container widget) is lane P7-2's job; this module
//! only builds the argv, finds the binary, spawns the child, feeds it the
//! password, and watches it exit.
//!
//! ## Credentials via stdin (probed on FreeRDP 3.15, Debian 13, xrdp)
//!
//! - The username travels on argv (`/u:<user>` — not a secret); the
//!   password travels on stdin as exactly one line with
//!   `/from-stdin:force`. Passing `/u` avoids the ordering trap: without
//!   it, `:force` reads the password *first* and the username second.
//! - Lines are bare (no quoting; `\r` or `\n` ends a line), so a password
//!   cannot contain a newline — [`RdpProcess::run`] truncates at the first
//!   one rather than sending a second line.
//! - Write the line and KEEP the pipe open until the session is connected:
//!   closing stdin early aborts the connect with
//!   `ERRCONNECT_CONNECT_CANCELLED`. The writer side therefore holds the
//!   `ChildStdin` handle for the whole session; only the in-memory copy of
//!   the password is dropped (zeroized) right after the write.
//! - `/from-stdin` exists in every FreeRDP since 2.0.0, but was buggy
//!   around 3.5–3.17 (fixed in 3.20). Tested on 3.15: it works there, but
//!   treat other versions in that range with suspicion. `FREERDP_ASKPASS`
//!   is not used (environment-based, 3.10+ only).
//!
//! ## Argv contract
//!
//! [`build_args`] emits, in order: `/v:<host>:<port>`, `/u:<username>`,
//! `/d:<domain>` (iff set), `/w:`/​`/h:`, `/bpp:` (iff set),
//! `/from-stdin:force` and `/sec:rdp:off` (always), the feature toggles,
//! `/parent-window:<xid>` (iff embedded — note the flag name:
//! `/parent-window:`, NOT `/parent-window-id:`, as verified on 3.15), and
//! the certificate token (iff resolved). The password never appears in
//! argv — it is not even an input to the builder, so the compiler enforces
//! what the tests document.
//!
//! `/sec:rdp:off` keeps the certificate pin meaningful. Legacy "Standard
//! RDP Security" has no TLS handshake, so `/cert:fingerprint:` is never
//! consulted: a peer that answers the negotiation with
//! `SSL_NOT_ALLOWED_BY_SERVER` gets a session — and the auto-logon password
//! — despite a pin that cannot match (verified on FreeRDP 3.32 against a
//! `freerdp-shadow-cli3 /sec:rdp` server). With the flag the client refuses
//! with `ERRCONNECT_SECURITY_NEGO_CONNECT_FAILED`; TLS and NLA servers are
//! unaffected. The `:off` toggle syntax is FreeRDP 3; 2.x spelled it
//! `-sec-rdp`, which 3.x rejects.
//!
//! Toggle forms follow FreeRDP's own `cmdline.h`/man pages (bool options
//! accept `+`/`-`): clipboard, fonts and compression default ON upstream,
//! so they are always spelled out (`+clipboard`/`-clipboard`,
//! `+fonts`/`-fonts`, `+compression`/`-compression`); audio redirection
//! (`/audio`, alias of `/sound`) and printer (`/printer`) default OFF, so
//! they are passed only when enabled (their `-` forms are not clean bool
//! toggles — both take optional values).
//!
//! ## Liveness heuristic
//!
//! xfreerdp has no machine-readable "connected" signal on stderr, so
//! [`RdpProcess::run`] emits `Connected` once when the child has lived
//! [`CONNECTED_DELAY`] (15 s) after spawn without exiting AND stderr
//! carries no `ERRCONNECT_*` line. Live validation of this heuristic
//! against the rig is P7-2/wave-close work, not this lane's.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
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
    args.push(
        if spec.clipboard {
            "+clipboard"
        } else {
            "-clipboard"
        }
        .to_string(),
    );
    args.push(if spec.fonts { "+fonts" } else { "-fonts" }.to_string());
    if spec.printer {
        args.push("/printer".to_string());
    }
    if spec.audio {
        args.push("/audio".to_string());
    }
    args.push(
        if spec.compression {
            "+compression"
        } else {
            "-compression"
        }
        .to_string(),
    );
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

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &std::path::Path) -> bool {
    true
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

/// Minimal child environment: [`ALLOWED_ENV`] entries copied from our own
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
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        RdpProcessEvent::Ended {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        RdpProcessEvent::Ended {
            code: status.code(),
            signal: None,
        }
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
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // New process group: terminate() signals the group, so orphaned
            // grandchildren of our child die with it — and we can never
            // signal anything outside our own session.
            cmd.as_std_mut().process_group(0);
        }
        // Exec can transiently fail with ETXTBSY while the binary is being
        // replaced on disk (a package update swapping xfreerdp mid-connect
        // — or a freshly-written helper racing concurrent forks). Retry a
        // few times; a persistent failure still surfaces as an error.
        let mut child: Option<Child> = None;
        for attempt in 0..SPAWN_RETRIES {
            match cmd.spawn() {
                Ok(c) => {
                    child = Some(c);
                    break;
                }
                Err(e)
                    if e.kind() == io::ErrorKind::ExecutableFileBusy
                        && attempt + 1 < SPAWN_RETRIES =>
                {
                    tokio::time::sleep(SPAWN_RETRY_DELAY).await;
                }
                Err(e) => return Err(e),
            }
        }
        let mut child = match child {
            Some(c) => c,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::ExecutableFileBusy,
                    "xfreerdp binary busy (ETXTBSY): still being replaced?",
                ));
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
        signal_group(child, Signal::Term);
        if tokio::time::timeout(self.terminate_grace, child.wait())
            .await
            .is_err()
        {
            signal_group(child, Signal::Kill);
            let _ = child.wait().await;
        }
        self.stdin.take();
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Signal the child's whole process group (created at spawn, so this can
/// never reach anything outside our own session). A reaped child has no
/// pid and is left alone.
fn signal_group(child: &mut Child, signal: Signal) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let signal = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: plain kill(2); the negative pid names the group we made.
        unsafe {
            libc::kill(-(pid as libc::pid_t), signal);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        let _ = child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn full_spec() -> RdpSpec {
        RdpSpec {
            host: "rdp.example.lan".to_string(),
            port: 3389,
            username: "ada".to_string(),
            domain: Some("CORP".to_string()),
            width: 1920,
            height: 1080,
            depth: Some(32),
            audio: true,
            clipboard: true,
            fonts: true,
            printer: true,
            compression: true,
            cert_arg: Some(format!(
                "{}4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b",
                crate::rdp::cert::CERT_ARG_PREFIX
            )),
        }
    }

    #[test]
    fn argv_matches_the_contract() {
        let args = build_args(&full_spec(), Some(0x2a00007));
        assert_eq!(
            args,
            vec![
                "/v:rdp.example.lan:3389",
                "/u:ada",
                "/d:CORP",
                "/w:1920",
                "/h:1080",
                "/bpp:32",
                "/from-stdin:force",
                "/sec:rdp:off",
                "+clipboard",
                "+fonts",
                "/printer",
                "/audio",
                "+compression",
                "/parent-window:44040199",
                "/cert:fingerprint:sha256:4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b",
            ]
        );
    }

    #[test]
    fn unset_optionals_disappear_and_off_toggles_spell_out() {
        let mut spec = full_spec();
        spec.domain = None;
        spec.depth = None;
        spec.audio = false;
        spec.clipboard = false;
        spec.fonts = false;
        spec.printer = false;
        spec.compression = false;
        spec.cert_arg = None;
        let args = build_args(&spec, None);
        assert_eq!(
            args,
            vec![
                "/v:rdp.example.lan:3389",
                "/u:ada",
                "/w:1920",
                "/h:1080",
                "/from-stdin:force",
                "/sec:rdp:off",
                "-clipboard",
                "-fonts",
                "-compression",
            ]
        );
    }

    #[test]
    fn legacy_rdp_security_is_always_refused() {
        // Without it a peer can skip TLS — and the pin — entirely.
        let mut spec = full_spec();
        for cert in [None, spec.cert_arg.clone()] {
            spec.cert_arg = cert;
            for xid in [None, Some(1)] {
                let args = build_args(&spec, xid);
                assert_eq!(args.iter().filter(|a| *a == SEC_RDP_OFF).count(), 1);
            }
        }
    }

    #[test]
    fn parent_window_present_iff_embedded() {
        let spec = full_spec();
        assert!(build_args(&spec, Some(99))
            .iter()
            .any(|a| a == "/parent-window:99"));
        assert!(build_args(&spec, None)
            .iter()
            .all(|a| !a.starts_with("/parent-window")));
        // The verified flag name is /parent-window:, never -id suffixed.
        assert!(build_args(&spec, Some(99))
            .iter()
            .all(|a| !a.contains("parent-window-id")));
    }

    #[test]
    fn cert_ignore_never_appears() {
        // Mutation check: if anyone swaps the fingerprint token for
        // /cert:ignore (or appends it), this goes red. The builder is the
        // last gate — only fingerprint-prefixed tokens pass; a hostile
        // cert_arg is dropped, never forwarded.
        for cert in [
            None,
            Some("/cert:fingerprint:sha256:abcd".to_string()),
            Some("/cert:ignore".to_string()),
        ] {
            let mut spec = full_spec();
            spec.cert_arg = cert;
            let args = build_args(&spec, Some(1));
            assert!(
                args.iter().all(|a| a != "/cert:ignore"),
                "argv must never carry /cert:ignore: {args:?}"
            );
        }
        // The hostile token is dropped entirely, not smuggled elsewhere.
        let mut spec = full_spec();
        spec.cert_arg = Some("/cert:ignore".to_string());
        let args = build_args(&spec, Some(1));
        assert!(args.iter().all(|a| !a.contains("ignore")));
    }

    #[test]
    fn password_has_no_place_in_argv() {
        // The builder does not even take a password: the compiler enforces
        // what this test documents. Pin the FreeRDP password flag too, so a
        // future `/p:` smuggling goes red.
        let args = build_args(&full_spec(), Some(7));
        assert!(args.iter().all(|a| !a.starts_with("/p:")));
        assert!(!args.iter().any(|a| a.contains("s3cr3t")));
    }

    #[test]
    fn version_parser_reads_parenthesized_triples() {
        assert_eq!(
            parse_version("This is FreeRDP version 3.15.0 (3.15.0)"),
            Some("3.15.0".to_string())
        );
        assert_eq!(
            parse_version("prefix (3.15.1) suffix"),
            Some("3.15.1".to_string())
        );
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("empty () parens"), None);
        assert_eq!(parse_version("(abc) not numeric"), None);
        assert_eq!(parse_version("unbalanced (3.15.0"), None);
    }

    #[test]
    fn version_parser_falls_back_to_bare_version_and_ignores_hashes() {
        // Real `xfreerdp --version` shape: the triple is bare, the parens
        // hold a git revision that must NOT parse as a version.
        assert_eq!(
            parse_version("This is FreeRDP version 3.15.1 (2681e62)"),
            Some("3.15.1".to_string())
        );
        assert_eq!(parse_version("(2681e62) only a hash"), None);
        // A bare triple with no keyword is not a version report.
        assert_eq!(parse_version("just 3.15.1 floating"), None);
    }

    #[test]
    fn ring_keeps_the_last_200_lines_in_order() {
        let mut ring = ErrorRing::new();
        assert!(ring.is_empty());
        for i in 0..(RING_CAPACITY + 50) {
            ring.push(format!("line {i}"));
        }
        assert_eq!(ring.len(), RING_CAPACITY);
        let digest = ring.digest();
        assert_eq!(digest.len(), RING_CAPACITY);
        assert_eq!(digest[0], "line 50");
        assert_eq!(
            digest[RING_CAPACITY - 1],
            format!("line {}", RING_CAPACITY + 49)
        );
    }

    #[test]
    fn connect_error_marker_detection() {
        let mut ring = ErrorRing::new();
        ring.push("a benign line".to_string());
        assert!(!ring.has_connect_error());
        ring.push("[WARN] ERRCONNECT_CONNECT_CANCELLED needlessly? no".to_string());
        assert!(ring.has_connect_error());
    }

    #[test]
    fn exit_mapping_separates_code_from_signal() {
        use std::os::unix::process::ExitStatusExt;
        let ok = std::process::ExitStatus::from_raw(0);
        assert_eq!(
            exit_event(&ok),
            RdpProcessEvent::Ended {
                code: Some(0),
                signal: None
            }
        );
        let failed = std::process::ExitStatus::from_raw(3 << 8);
        assert_eq!(
            exit_event(&failed),
            RdpProcessEvent::Ended {
                code: Some(3),
                signal: None
            }
        );
        let killed = std::process::ExitStatus::from_raw(9);
        assert_eq!(
            exit_event(&killed),
            RdpProcessEvent::Ended {
                code: None,
                signal: Some(9)
            }
        );
    }

    #[test]
    fn minimal_env_is_allowlisted() {
        for (key, _) in minimal_env() {
            assert!(ALLOWED_ENV.contains(&key.as_str()));
        }
        // The display needs its authorization cookie to admit the child.
        assert!(ALLOWED_ENV.contains(&"XAUTHORITY"));
    }

    #[test]
    fn find_binary_prefers_xfreerdp3_and_skips_nonexec() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("xfreerdp");
        std::fs::write(&legacy, "#!/bin/sh\necho hi\n").unwrap();
        // Not executable yet: invisible to the search.
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find_binary_in(Some(path.as_os_str())), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            find_binary_in(Some(path.as_os_str())),
            Some("xfreerdp".to_string())
        );

        let modern = dir.path().join("xfreerdp3");
        std::fs::write(&modern, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&modern, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            find_binary_in(Some(path.as_os_str())),
            Some("xfreerdp3".to_string())
        );
        assert_eq!(find_binary_in(None), None);
    }

    #[test]
    fn probe_binary_reports_a_sane_shape_when_present() {
        // xfreerdp may not be installed: only pin the output SHAPE.
        if let Some((name, version)) = probe_binary() {
            assert!(["xfreerdp3", "xfreerdp"].contains(&name.as_str()));
            assert!(!version.is_empty());
        }
    }

    /// Write an executable sh script; returns its path.
    fn fake_binary(dir: &tempfile::TempDir, body: &str) -> String {
        let path = dir.path().join("fake-xfreerdp");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.to_str().unwrap().to_string()
    }

    fn test_spec() -> RdpSpec {
        RdpSpec {
            host: "127.0.0.1".to_string(),
            port: 13389,
            username: "tester".to_string(),
            domain: None,
            width: 1024,
            height: 768,
            depth: None,
            audio: false,
            clipboard: true,
            fonts: true,
            printer: false,
            compression: true,
            cert_arg: None,
        }
    }

    /// Stop a shared, live session the way the tab does: abort the runner
    /// (which holds the lock for the whole session), then terminate. Fails
    /// if the child outlived the call or it took anywhere near the fake's
    /// natural lifetime.
    async fn abort_and_terminate(
        proc: &std::sync::Arc<tokio::sync::Mutex<RdpProcess>>,
        runner: tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let started = std::time::Instant::now();
        runner.abort();
        let mut proc = tokio::time::timeout(Duration::from_secs(5), proc.lock())
            .await
            .expect("the aborted runner releases the lock");
        proc.terminate().await;
        let reaped = proc.child.as_mut().expect("spawned").try_wait().unwrap();
        assert!(reaped.is_some(), "terminate reaps the child");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn run_emits_connected_and_terminate_kills_the_live_child() {
        let dir = tempfile::tempdir().unwrap();
        // Reads ONE stdin line (the password), notes stderr, then idles.
        let bin = fake_binary(&dir, "read -r line\necho fake-ready >&2\nsleep 30");
        let proc = RdpProcess::new(bin.clone(), &test_spec(), None)
            .with_connected_delay(Duration::from_millis(100))
            .with_terminate_grace(Duration::from_secs(2));
        assert_eq!(proc.binary(), bin);
        assert_eq!(proc.argv()[0], "/v:127.0.0.1:13389");
        let proc = std::sync::Arc::new(tokio::sync::Mutex::new(proc));
        let (tx, mut rx) = mpsc::channel(8);
        let password = Zeroizing::new(b"not-a-real-secret".to_vec());
        let runner = {
            let proc = std::sync::Arc::clone(&proc);
            tokio::spawn(async move { proc.lock().await.run(&password, tx).await })
        };
        assert_eq!(rx.recv().await, Some(RdpProcessEvent::Connected));
        abort_and_terminate(&proc, runner).await;
        assert_eq!(rx.recv().await, None, "the aborted runner sends nothing");
    }

    #[tokio::test]
    async fn run_against_failing_child_ends_without_connected() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(
            &dir,
            "read -r line\necho ERRCONNECT_LOGON_FAILURE >&2\nexit 1",
        );
        let mut proc =
            RdpProcess::new(bin, &test_spec(), None).with_connected_delay(Duration::from_secs(60));
        let (tx, mut rx) = mpsc::channel(8);
        let password = Zeroizing::new(b"wrong".to_vec());
        proc.run(&password, tx).await.unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(RdpProcessEvent::Ended { code: Some(1), .. })
        ));
        assert_eq!(rx.recv().await, None, "Ended is always last");
        assert!(proc
            .error_digest()
            .iter()
            .any(|l| l.contains("ERRCONNECT_LOGON_FAILURE")));
    }

    #[tokio::test]
    async fn spawn_retries_past_transient_text_file_busy() {
        // Holding the binary open for writing makes every exec of it fail
        // with ETXTBSY; releasing it from a timer lets the retry loop in
        // run() succeed. Guards the fix for the flake seen under parallel
        // gate runs (spawn racing concurrent forks failed with
        // `ExecutableFileBusy`).
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(&dir, "read -r line\necho hi >&2\nexit 3");
        let held = std::fs::OpenOptions::new().write(true).open(&bin).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(held);
        });
        let mut proc =
            RdpProcess::new(bin, &test_spec(), None).with_connected_delay(Duration::from_secs(60));
        let (tx, mut rx) = mpsc::channel(8);
        let password = Zeroizing::new(b"pw".to_vec());
        proc.run(&password, tx).await.unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(RdpProcessEvent::Ended { code: Some(3), .. })
        ));
        assert_eq!(rx.recv().await, None, "Ended is always last");
    }

    #[tokio::test]
    async fn child_sees_minimal_env_and_never_the_password() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("env-dump");
        let bin = fake_binary(
            &dir,
            &format!(
                "read -r line\nenv | sort > {}\nsleep 30",
                dump.to_str().unwrap()
            ),
        );
        let proc = RdpProcess::new(bin, &test_spec(), None)
            .with_connected_delay(Duration::from_millis(50))
            .with_terminate_grace(Duration::from_secs(2));
        let (tx, mut rx) = mpsc::channel(8);
        let secret = "s3cr3t-that-must-not-leak";
        let password = Zeroizing::new(secret.as_bytes().to_vec());
        let proc = std::sync::Arc::new(tokio::sync::Mutex::new(proc));
        let runner = {
            let proc = std::sync::Arc::clone(&proc);
            tokio::spawn(async move { proc.lock().await.run(&password, tx).await })
        };
        assert_eq!(rx.recv().await, Some(RdpProcessEvent::Connected));
        abort_and_terminate(&proc, runner).await;
        let dumped = std::fs::read_to_string(&dump).unwrap();
        // The fake binary is a shebang script, so its interpreter adds
        // PWD/SHLVL/_ itself — those are not leaks. Everything else the
        // child sees must come from the allowlist.
        const INTERPRETER_ADDED: [&str; 3] = ["PWD", "SHLVL", "_"];
        assert!(!dumped.contains(secret), "password leaked into env");
        for (key, _) in std::env::vars() {
            if ALLOWED_ENV.contains(&key.as_str()) || INTERPRETER_ADDED.contains(&key.as_str()) {
                continue;
            }
            assert!(
                dumped
                    .lines()
                    .all(|l| l.split('=').next() != Some(key.as_str())),
                "parent env leaked into child: {key}"
            );
        }
        for line in dumped.lines() {
            let key = line.split('=').next().unwrap_or("");
            assert!(
                ALLOWED_ENV.contains(&key) || INTERPRETER_ADDED.contains(&key),
                "unexpected env key in child: {key}"
            );
        }
    }
}
