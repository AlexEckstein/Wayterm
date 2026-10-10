//! Local shell transport: the user's shell on a `forkpty` pseudo-terminal
//! (doc 04 §Local and serial transports).

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::store::model::SessionConfig;
use crate::transport::types::{
    Transport, TransportError, TransportEvent, TransportState, TransportType,
};

/// Fallback when neither the session nor `$SHELL` names a shell.
const DEFAULT_SHELL: &str = "/bin/sh";
/// How long a hung-up child gets to exit before it is sent `SIGKILL`.
const REAP_KILL_AFTER: Duration = Duration::from_secs(5);
/// Highest descriptor the pre-5.9-kernel fallback closes in the child
/// (`close_range(2)` needs no bound).
const FALLBACK_CLOSE_LIMIT: libc::c_int = 65_536;
const REAP_POLL: Duration = Duration::from_millis(20);

/// The user's shell on a local PTY. The reader task pushes output from the
/// master side; `EIO` there means the child exited, so it is reaped and
/// reported as `SessionEnded`.
pub struct LocalTransport {
    /// The shell as configured; resolved against `$PATH` at connect.
    shell: PathBuf,
    working_dir: Option<PathBuf>,
    master: Option<Arc<AsyncFd<OwnedFd>>>,
    /// The child's pid until someone takes it to reap it (0 = none), so it
    /// is waited for exactly once — a reaped pid may be reused.
    child_pid: Arc<AtomicI32>,
    reader_task: Option<JoinHandle<()>>,
    event_tx: mpsc::Sender<TransportEvent>,
    cancel_token: CancellationToken,
    /// The terminal's measured grid at connect time.
    initial_rows: u32,
    initial_cols: u32,
    /// Shared with the reader task, which ends the connection on `EIO`.
    state: Arc<Mutex<TransportState>>,
}

impl LocalTransport {
    pub fn new(
        config: &SessionConfig,
        event_tx: mpsc::Sender<TransportEvent>,
        initial_rows: u32,
        initial_cols: u32,
    ) -> Self {
        let shell = select_shell(config.local_shell.as_deref(), std::env::var_os("SHELL"));
        let working_dir = config
            .local_working_dir
            .as_deref()
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        Self {
            shell,
            working_dir,
            master: None,
            child_pid: Arc::new(AtomicI32::new(0)),
            reader_task: None,
            event_tx,
            cancel_token: CancellationToken::new(),
            initial_rows,
            initial_cols,
            state: Arc::new(Mutex::new(TransportState::Disconnected)),
        }
    }

    fn set_state(&self, state: TransportState) {
        set_shared_state(&self.state, &self.event_tx, state);
    }

    fn master(&self) -> Result<Arc<AsyncFd<OwnedFd>>, TransportError> {
        self.master.clone().ok_or(TransportError::Disconnected)
    }
}

/// Record `state` and notify, unless nothing changed.
fn set_shared_state(
    shared: &Mutex<TransportState>,
    tx: &mpsc::Sender<TransportEvent>,
    state: TransportState,
) {
    let mut current = shared.lock().unwrap_or_else(|e| e.into_inner());
    if *current != state {
        *current = state;
        current.emit(tx);
    }
}

/// The session's shell, else `$SHELL`, else `/bin/sh`; empty means unset.
fn select_shell(local_shell: Option<&str>, env_shell: Option<OsString>) -> PathBuf {
    if let Some(shell) = local_shell.filter(|s| !s.is_empty()) {
        return PathBuf::from(shell);
    }
    match env_shell.filter(|s| !s.is_empty()) {
        Some(shell) => PathBuf::from(shell),
        None => PathBuf::from(DEFAULT_SHELL),
    }
}

/// A bare name (`zsh`) is looked up in `$PATH`; anything with a slash is
/// taken as is. Done before the fork: the child may only call
/// async-signal-safe functions, so it gets `execve` and a full path.
fn resolve_shell(shell: &Path) -> Result<PathBuf, TransportError> {
    let not_found =
        || TransportError::ConnectionFailed(format!("shell not found: {}", shell.display()));
    if shell.as_os_str().as_bytes().contains(&b'/') {
        return is_executable(shell)
            .then(|| shell.to_path_buf())
            .ok_or_else(not_found);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(shell))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(not_found)
}

fn is_executable(path: &Path) -> bool {
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string.
    path.is_file() && unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0
}

/// The child's environment: ours, with the terminal's identity on top.
fn child_env() -> Vec<CString> {
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(key, _)| key != "TERM" && key != "COLORTERM")
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).ok()
        })
        .collect();
    env.push(c"TERM=xterm-256color".to_owned());
    env.push(c"COLORTERM=truecolor".to_owned());
    env
}

fn to_cstring(s: &OsStr) -> Result<CString, TransportError> {
    CString::new(s.as_bytes())
        .map_err(|_| TransportError::ConnectionFailed(format!("NUL in {}", s.to_string_lossy())))
}

fn winsize(rows: u32, cols: u32) -> libc::winsize {
    libc::winsize {
        ws_row: u16::try_from(rows).unwrap_or(u16::MAX),
        ws_col: u16::try_from(cols).unwrap_or(u16::MAX),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Fork `shell` on a new PTY sized `size`, in `cwd` when given. Returns the
/// master fd (non-blocking, close-on-exec) and the child's pid.
fn spawn_shell(
    shell: &Path,
    cwd: Option<&Path>,
    size: libc::winsize,
) -> Result<(OwnedFd, libc::pid_t), TransportError> {
    // Everything the child needs is allocated here: after the fork it may
    // only make async-signal-safe calls.
    let shell_c = to_cstring(shell.as_os_str())?;
    let argv = [shell_c.as_ptr(), std::ptr::null()];
    let env = child_env();
    let envp: Vec<*const libc::c_char> = env
        .iter()
        .map(|entry| entry.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let cwd_c = cwd.map(|dir| to_cstring(dir.as_os_str())).transpose()?;
    // SAFETY: sysconf has no preconditions.
    let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let close_limit = libc::c_int::try_from(open_max)
        .unwrap_or(FALLBACK_CLOSE_LIMIT)
        .clamp(3, FALLBACK_CLOSE_LIMIT);

    let mut master: libc::c_int = -1;
    // SAFETY: the out-pointer is valid; null name/termios are allowed.
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &size) };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        // SAFETY: child side — async-signal-safe calls only, then exec or
        // `_exit`. Signal state is inherited across exec, so put back what
        // the runtime changed (Rust ignores SIGPIPE).
        unsafe {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            for signal in 1..32 {
                libc::signal(signal, libc::SIG_DFL);
            }
            close_inherited_fds(close_limit);
            if let Some(dir) = &cwd_c {
                if libc::chdir(dir.as_ptr()) != 0 {
                    libc::_exit(126);
                }
            }
            libc::execve(shell_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }

    // SAFETY: forkpty returned a fresh master fd that nothing else owns.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let raw = master.as_raw_fd();
    // SAFETY: plain fcntl flag updates on a valid fd.
    let flagged = unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        flags >= 0
            && libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
            && libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) == 0
    };
    if !flagged {
        let err = io::Error::last_os_error();
        drop(master);
        tokio::spawn(reap(pid));
        return Err(err.into());
    }
    Ok((master, pid))
}

/// In the forked child: close every descriptor above stdio, so the shell
/// inherits only its terminal. Without this it also inherited whatever the
/// process held without close-on-exec at fork time — notably another
/// tab's PTY master in the window between `forkpty` and its `FD_CLOEXEC`,
/// which would let that tab's shell read and type into this one, and keep
/// its slave from ever reporting `EIO`. Terminal emulators conventionally
/// do this (VTE does).
///
/// # Safety
///
/// Child side of a fork only: raw syscalls, async-signal-safe.
unsafe fn close_inherited_fds(fallback_limit: libc::c_int) {
    // SAFETY: close_range(3, ~0U, 0) takes plain integers.
    let closed = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            libc::c_long::from(3_u32),
            libc::c_long::from(u32::MAX),
            libc::c_long::from(0_u32),
        )
    };
    if closed == 0 {
        return;
    }
    // Kernels before 5.9 have no close_range: close one by one.
    for fd in 3..fallback_limit {
        // SAFETY: closing a descriptor we may not hold is a harmless EBADF.
        unsafe { libc::close(fd) };
    }
}

/// Take the pid so it is reaped once, by whoever gets there first.
fn take_pid(pid: &AtomicI32) -> Option<libc::pid_t> {
    Some(pid.swap(0, Ordering::SeqCst)).filter(|&pid| pid > 0)
}

/// Wait for `pid` without blocking a thread. A child that ignores the
/// hangup gets `SIGKILL` after [`REAP_KILL_AFTER`], so no zombie remains.
async fn reap(pid: libc::pid_t) {
    let kill_at = Instant::now() + REAP_KILL_AFTER;
    let mut killed = false;
    loop {
        let mut status = 0;
        // SAFETY: waitpid on our own child with a valid status pointer.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return;
        }
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return; // ECHILD: already reaped.
        }
        if !killed && Instant::now() >= kill_at {
            // SAFETY: signalling our own, not yet reaped, child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            killed = true;
        }
        tokio::time::sleep(REAP_POLL).await;
    }
}

/// The only read path: push every chunk from the master as `DataReceived`
/// until the child's side closes (`EIO`), then reap the child and report:
/// `SessionEnded` when the shell exited, `ConnectionLost` after a read error.
async fn read_loop(
    master: Arc<AsyncFd<OwnedFd>>,
    child_pid: Arc<AtomicI32>,
    state: Arc<Mutex<TransportState>>,
    event_tx: mpsc::Sender<TransportEvent>,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; 8192];
    let exited = loop {
        let guard = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            guard = master.readable() => guard,
        };
        let mut guard = match guard {
            Ok(guard) => guard,
            Err(e) => {
                let _ = event_tx.try_send(TransportEvent::Error(e.to_string()));
                break false;
            }
        };
        let read = guard.try_io(|fd| {
            // SAFETY: reading into a buffer we own, of the length we pass.
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match read {
            Err(_would_block) => continue,
            Ok(Ok(0)) => break true,
            Ok(Ok(n)) => {
                let event = TransportEvent::DataReceived(buf[..n].to_vec());
                if event_tx.send(event).await.is_err() {
                    return; // tab closed
                }
            }
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
            // EIO: every slave fd is closed — the shell exited.
            Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => break true,
            Ok(Err(e)) => {
                let _ = event_tx.try_send(TransportEvent::Error(e.to_string()));
                break false;
            }
        }
    };
    // Reap in a task of its own: `disconnect` aborting this reader must
    // not leave a zombie behind.
    if let Some(pid) = take_pid(&child_pid) {
        let _ = tokio::spawn(reap(pid)).await;
    }
    let event = if exited {
        TransportEvent::SessionEnded
    } else {
        TransportEvent::ConnectionLost
    };
    let _ = event_tx.send(event).await;
    set_shared_state(&state, &event_tx, TransportState::Disconnected);
}

#[async_trait]
impl Transport for LocalTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        if self.master.is_some() {
            return Err(TransportError::ChannelError("already connected".into()));
        }
        self.set_state(TransportState::Connecting);

        let spawned = resolve_shell(&self.shell).and_then(|shell| {
            if let Some(dir) = self.working_dir.as_deref().filter(|dir| !dir.is_dir()) {
                return Err(TransportError::ConnectionFailed(format!(
                    "working directory not found: {}",
                    dir.display()
                )));
            }
            let size = winsize(self.initial_rows, self.initial_cols);
            spawn_shell(&shell, self.working_dir.as_deref(), size)
        });
        let (master, pid) = match spawned {
            Ok(spawned) => spawned,
            Err(e) => {
                self.set_state(TransportState::Disconnected);
                return Err(e);
            }
        };
        self.child_pid.store(pid, Ordering::SeqCst);
        let master = match AsyncFd::new(master) {
            Ok(master) => Arc::new(master),
            Err(e) => {
                if let Some(pid) = take_pid(&self.child_pid) {
                    tokio::spawn(reap(pid));
                }
                self.set_state(TransportState::Disconnected);
                return Err(e.into());
            }
        };

        self.cancel_token = CancellationToken::new();
        self.reader_task = Some(tokio::spawn(read_loop(
            master.clone(),
            self.child_pid.clone(),
            self.state.clone(),
            self.event_tx.clone(),
            self.cancel_token.clone(),
        )));
        self.master = Some(master);
        self.set_state(TransportState::Connected);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.cancel_token.cancel();
        if let Some(task) = self.reader_task.take() {
            task.abort();
            // Wait so the reader's handle on the master is gone too.
            let _ = task.await;
        }
        // Closing the master hangs up the PTY: the shell gets SIGHUP.
        self.master = None;
        if let Some(pid) = take_pid(&self.child_pid) {
            tokio::spawn(reap(pid));
        }
        self.set_state(TransportState::Disconnected);
        Ok(())
    }

    fn state(&self) -> TransportState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let master = self.master()?;
        let mut rest = data;
        while !rest.is_empty() {
            let mut guard = master.writable().await?;
            let written = guard.try_io(|fd| {
                // SAFETY: writing from a slice we borrow, of its length.
                let n = unsafe { libc::write(fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match written {
                Err(_would_block) => continue,
                Ok(Ok(n)) => rest = &rest[n..],
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// `TIOCSWINSZ` on the master; the kernel sends `SIGWINCH` to the
    /// child's foreground process group.
    async fn resize(&mut self, rows: u32, cols: u32) -> Result<(), TransportError> {
        let master = self.master()?;
        let size = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ reads one `winsize` from a valid pointer.
        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// Ignored: tmux persistence keeps a remote shell alive across dropped
    /// connections, and a local shell has no connection to drop.
    fn set_tmux_enabled(&mut self, _enabled: bool, _session_name: String) {}

    fn transport_type(&self) -> TransportType {
        TransportType::LocalPTY
    }

    fn description(&self) -> String {
        self.shell.display().to_string()
    }
}

/// A transport dropped without `disconnect` (the tab's close timed out)
/// still hangs up its shell and reaps it.
impl Drop for LocalTransport {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        if let (Some(pid), Ok(runtime)) = (
            take_pid(&self.child_pid),
            tokio::runtime::Handle::try_current(),
        ) {
            runtime.spawn(reap(pid));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(config: &SessionConfig) -> (LocalTransport, mpsc::Receiver<TransportEvent>) {
        let (tx, rx) = mpsc::channel(256);
        (LocalTransport::new(config, tx, 24, 80), rx)
    }

    fn sh_config() -> SessionConfig {
        SessionConfig {
            local_shell: Some("/bin/sh".into()),
            ..SessionConfig::default()
        }
    }

    /// Collect output until it contains `needle`.
    async fn output_until(rx: &mut mpsc::Receiver<TransportEvent>, needle: &str) -> String {
        let mut seen = String::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !seen.contains(needle) {
                if let Some(TransportEvent::DataReceived(data)) = rx.recv().await {
                    seen.push_str(&String::from_utf8_lossy(&data));
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {needle:?} in output: {seen:?}"));
        seen
    }

    #[test]
    fn shell_selection_prefers_session_then_env_then_bin_sh() {
        let env = Some(OsString::from("/usr/bin/zsh"));
        assert_eq!(
            select_shell(Some("/bin/bash"), env.clone()),
            Path::new("/bin/bash")
        );
        assert_eq!(select_shell(None, env.clone()), Path::new("/usr/bin/zsh"));
        assert_eq!(select_shell(Some(""), env), Path::new("/usr/bin/zsh"));
        assert_eq!(
            select_shell(None, Some(OsString::new())),
            Path::new("/bin/sh")
        );
        assert_eq!(select_shell(None, None), Path::new("/bin/sh"));
    }

    #[test]
    fn bare_shell_names_resolve_through_path() {
        assert!(resolve_shell(Path::new("sh")).unwrap().is_absolute());
        assert!(matches!(
            resolve_shell(Path::new("/nonexistent/shell")),
            Err(TransportError::ConnectionFailed(_))
        ));
    }

    #[test]
    fn child_env_declares_the_terminal() {
        let env = child_env();
        let has = |entry: &str| env.iter().any(|e| e.as_bytes() == entry.as_bytes());
        assert!(has("TERM=xterm-256color"));
        assert!(has("COLORTERM=truecolor"));
        assert_eq!(
            env.iter()
                .filter(|e| e.as_bytes().starts_with(b"TERM="))
                .count(),
            1
        );
    }

    #[test]
    fn identity_is_the_shell() {
        let (t, _rx) = transport(&sh_config());
        assert_eq!(t.description(), "/bin/sh");
        assert!(matches!(t.transport_type(), TransportType::LocalPTY));
        assert_eq!(t.state(), TransportState::Disconnected);
    }

    #[tokio::test]
    async fn working_dir_becomes_the_shells_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let config = SessionConfig {
            local_working_dir: Some(dir.path().to_string_lossy().into_owned()),
            ..sh_config()
        };
        let (mut t, mut rx) = transport(&config);
        t.connect().await.unwrap();
        // The echoed command line also shows the markers, around `$(…)`.
        t.write(b"echo \"cwd=$(pwd -P).\"\n").await.unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        output_until(&mut rx, &format!("cwd={}.", canonical.display())).await;
        t.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn the_shell_inherits_no_descriptors_beyond_its_terminal() {
        // A descriptor without close-on-exec, at a number nothing else
        // uses: exactly what a racing fork or a careless library leaves.
        const PLANTED: libc::c_int = 777;
        // SAFETY: duplicating our own stderr onto an unused number.
        assert_eq!(unsafe { libc::dup2(2, PLANTED) }, PLANTED);
        let (mut t, mut rx) = transport(&sh_config());
        t.connect().await.unwrap();
        // The marker is assembled by the shell, so the echoed command line
        // cannot satisfy the wait by itself.
        t.write(b"echo \"fd777=$(test -e /proc/$$/fd/777 && echo open || echo closed).\"\n")
            .await
            .unwrap();
        let seen = output_until(&mut rx, "fd777=closed.").await;
        t.disconnect().await.unwrap();
        // SAFETY: closing the descriptor planted above.
        unsafe { libc::close(PLANTED) };
        assert!(!seen.contains("fd777=open."), "{seen:?}");
    }

    #[tokio::test]
    async fn a_missing_working_dir_fails_the_connect() {
        let config = SessionConfig {
            local_working_dir: Some("/nonexistent/wayterm-dir".into()),
            ..sh_config()
        };
        let (mut t, _rx) = transport(&config);
        assert!(matches!(
            t.connect().await,
            Err(TransportError::ConnectionFailed(_))
        ));
        assert_eq!(t.state(), TransportState::Disconnected);
    }

    #[tokio::test]
    async fn state_moves_through_connecting_connected_disconnected() {
        let (mut t, mut rx) = transport(&sh_config());
        t.connect().await.unwrap();
        assert_eq!(t.state(), TransportState::Connected);
        assert!(matches!(
            t.connect().await,
            Err(TransportError::ChannelError(_))
        ));
        t.disconnect().await.unwrap();
        assert_eq!(t.state(), TransportState::Disconnected);

        let mut states = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TransportEvent::StateChanged(state) = event {
                states.push(state);
            }
        }
        assert_eq!(
            states,
            [
                TransportState::Connecting,
                TransportState::Connected,
                TransportState::Disconnected
            ]
        );
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_ends_io() {
        let (mut t, _rx) = transport(&sh_config());
        t.disconnect().await.unwrap();
        t.connect().await.unwrap();
        t.disconnect().await.unwrap();
        t.disconnect().await.unwrap();
        assert_eq!(t.state(), TransportState::Disconnected);
        assert!(matches!(
            t.write(b"x").await,
            Err(TransportError::Disconnected)
        ));
        assert!(matches!(
            t.resize(30, 100).await,
            Err(TransportError::Disconnected)
        ));
    }
}
