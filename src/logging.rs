//! Application diagnostics logging (doc 16): plain lines to stderr and to
//! an owner-only log file under the XDG state directory, filtered by
//! `WAYTERM_LOG`, plus a panic hook that records the panic before the
//! default report.

use std::any::Any;
use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::thread::ThreadId;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::{EnvFilter, LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

/// The env var holding the filter directives.
pub(crate) const LOG_ENV: &str = "WAYTERM_LOG";
const DEFAULT_DIRECTIVES: &str = "info";

static INIT: Once = Once::new();
/// The file writer's worker; dropping it flushes.
static FILE_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);
/// The thread that called init: the GTK main thread.
static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Install the process-wide subscriber and panic hook. Only the first call
/// does anything.
pub fn init() {
    init_with(
        std::io::stderr,
        default_log_path().as_deref(),
        std::env::var(LOG_ENV).ok().as_deref(),
    );
}

/// `init()` with its inputs named, so tests never touch the real stderr,
/// state dir or environment.
fn init_with<C>(console: C, path: Option<&Path>, directives: Option<&str>)
where
    C: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    INIT.call_once(|| install(console, path, directives));
}

fn install<C>(console: C, path: Option<&Path>, directives: Option<&str>)
where
    C: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let (filter, warning) = filter_for(directives);
    if let Some(warning) = warning {
        eprintln!("Wayterm: {warning}");
    }
    let file = open_file_sink(path);
    let (writer, guard) = match file {
        Some((writer, guard)) => (Some(writer), Some(guard)),
        None => (None, None),
    };
    // Not `SubscriberInitExt::init`: that also bridges `log` records into
    // tracing, and dependencies' `log` output (russh's wire-level debug
    // lines) must never reach this log.
    if let Err(err) = tracing::subscriber::set_global_default(subscriber(filter, console, writer)) {
        eprintln!("Wayterm: cannot install the logger: {err}");
        return;
    }
    let has_file = guard.is_some();
    *FILE_GUARD.lock().unwrap_or_else(|e| e.into_inner()) = guard;
    let _ = MAIN_THREAD.set(std::thread::current().id());
    install_panic_hook();
    match path {
        Some(path) if has_file => {
            tracing::info!(log_file = %path.display(), "logging initialized")
        }
        _ => tracing::warn!("logging initialized: stderr only, no log file"),
    }
}

/// `$XDG_STATE_HOME/wayterm/logs/wayterm.log` (default
/// `~/.local/state/wayterm/logs/wayterm.log`).
fn default_log_path() -> Option<PathBuf> {
    log_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// The log path from the two variables: an empty one counts as unset, and
/// without either there is no path.
fn log_path_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let state = match xdg_state_home.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home.filter(|dir| !dir.is_empty())?).join(".local/state"),
    };
    Some(state.join("wayterm/logs/wayterm.log"))
}

/// The filter for `WAYTERM_LOG`'s value: unset or blank means `info`; an
/// invalid value also means `info`, with a warning saying why.
fn filter_for(directives: Option<&str>) -> (EnvFilter, Option<String>) {
    let default = || EnvFilter::new(DEFAULT_DIRECTIVES);
    let Some(value) = directives.filter(|value| !value.trim().is_empty()) else {
        return (default(), None);
    };
    match EnvFilter::builder().parse(value) {
        Ok(filter) => (filter, None),
        Err(err) => (
            default(),
            Some(format!(
                "invalid {LOG_ENV} value {value:?} ({err}); using {DEFAULT_DIRECTIVES:?}"
            )),
        ),
    }
}

/// Open `path` for appending, creating it and its directories: the log
/// names hosts and local paths, so like the session store it is owner-only
/// (directories 0700, file 0600).
fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
}

/// The non-blocking file writer and its worker, or `None` (said once on
/// stderr) when there is no path or it cannot be opened.
fn open_file_sink(path: Option<&Path>) -> Option<(NonBlocking, WorkerGuard)> {
    let Some(path) = path else {
        eprintln!("Wayterm: no state directory (XDG_STATE_HOME or HOME); logging to stderr only");
        return None;
    };
    match open_log_file(path) {
        Ok(file) => Some(tracing_appender::non_blocking(file)),
        Err(err) => {
            eprintln!(
                "Wayterm: cannot open the log file {}: {err}; logging to stderr only",
                path.display()
            );
            None
        }
    }
}

/// The subscriber `init` installs: `filter`, restricted to Wayterm's own
/// targets, writing lines to `console` and, if given, `file`.
fn subscriber<C, F>(
    filter: EnvFilter,
    console: C,
    file: Option<F>,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    C: for<'w> MakeWriter<'w> + Send + Sync + 'static,
    F: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(filter)
        .with(own_targets())
        .with(lines(console))
        .with(file.map(lines))
}

/// Only events from Wayterm's own targets (`wayterm::…`) are recorded,
/// whatever `WAYTERM_LOG` says: dependencies that use `tracing` (zbus,
/// under the keyring) trace whole D-Bus messages, and a Secret Service
/// reply must never reach the log.
fn own_targets() -> Targets {
    Targets::new().with_target("wayterm", LevelFilter::TRACE)
}

/// Plain UTF-8 lines: RFC 3339 UTC timestamp, level, target, message.
fn lines<S, W>(writer: W) -> impl Layer<S> + Send + Sync + 'static
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
}

/// One line describing a panic: where, what it said, and the payload's
/// type.
fn describe_panic(payload: &(dyn Any + Send), location: Option<&Location<'_>>) -> String {
    let location = location.map_or_else(
        || "unknown location".to_owned(),
        |at| format!("{}:{}:{}", at.file(), at.line(), at.column()),
    );
    let (message, kind) = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message, "&str")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        (message.as_str(), "String")
    } else {
        ("Box<dyn Any>", "unknown type")
    };
    format!("panic at {location}: {message} (payload type: {kind})")
}

fn panic_record(info: &std::panic::PanicHookInfo<'_>) -> String {
    describe_panic(info.payload(), info.location())
}

/// Record every panic in the log, then hand it to the previous hook.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            "{}",
            panic_record(info)
        );
        // A panic on the GTK main thread ends the process (it unwinds out
        // of main or aborts at the FFI boundary), and the non-blocking
        // writer would otherwise race the exit and lose this very line.
        if MAIN_THREAD.get() == Some(&thread.id()) {
            flush();
        }
        previous(info);
    }));
}

/// Write out what the file writer holds and stop it: file logging ends
/// here, stderr logging goes on. Calling it again does nothing.
pub(crate) fn flush() {
    let guard = FILE_GUARD.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(guard);
}

#[cfg(test)]
pub(crate) mod tests;
