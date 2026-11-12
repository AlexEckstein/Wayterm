use super::*;

use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;

/// A `MakeWriter` over a shared in-memory buffer.
#[derive(Clone, Default)]
pub(crate) struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    /// Everything written so far, as text.
    pub(crate) fn contents(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Record this thread's events, until the guard drops, in a buffer,
/// through the production subscriber and filter built from `directives` as
/// `WAYTERM_LOG` would give them.
pub(crate) fn capture(directives: Option<&str>) -> (LogBuffer, tracing::subscriber::DefaultGuard) {
    let buffer = LogBuffer::default();
    let (filter, _) = filter_for(directives);
    let guard =
        tracing::subscriber::set_default(subscriber(filter, buffer.clone(), None::<LogBuffer>));
    (buffer, guard)
}

const TARGET: &str = "wayterm::logging::tests";

fn line_with<'a>(output: &'a str, message: &str) -> Option<&'a str> {
    output.lines().find(|line| line.contains(message))
}

#[test]
fn events_are_plain_utc_lines_with_level_target_and_message() {
    let (buffer, guard) = capture(None);
    tracing::info!("an info event");
    tracing::warn!("a warn event");
    tracing::error!("an error event");
    drop(guard);
    let output = buffer.contents();

    for (level, message) in [
        ("INFO", "an info event"),
        ("WARN", "a warn event"),
        ("ERROR", "an error event"),
    ] {
        let line = line_with(&output, message).unwrap_or_else(|| panic!("{message} missing"));
        assert!(line.contains(level), "{line}");
        assert!(line.contains(TARGET), "{line}");
        let stamp = line.split_whitespace().next().unwrap();
        assert!(stamp[..4].bytes().all(|b| b.is_ascii_digit()), "{line}");
        assert!(stamp.ends_with('Z'), "{line}");
    }
    assert!(!output.contains('\x1b'), "{output:?}");
}

#[test]
fn the_default_filter_drops_debug_and_trace() {
    let (buffer, guard) = capture(None);
    tracing::info!("kept at info");
    tracing::debug!("dropped at debug");
    tracing::trace!("dropped at trace");
    drop(guard);
    let output = buffer.contents();

    assert!(output.contains("kept at info"), "{output}");
    assert!(!output.contains("dropped at debug"), "{output}");
    assert!(!output.contains("dropped at trace"), "{output}");
}

#[test]
fn wayterm_log_directives_set_the_filter() {
    let (buffer, guard) = capture(Some("wayterm=debug"));
    tracing::debug!("debug under wayterm=debug");
    tracing::trace!("trace under wayterm=debug");
    drop(guard);
    let output = buffer.contents();
    assert!(output.contains("debug under wayterm=debug"), "{output}");
    assert!(!output.contains("trace under wayterm=debug"), "{output}");

    let (buffer, guard) = capture(Some("warn"));
    tracing::info!("info under warn");
    tracing::warn!("warn under warn");
    drop(guard);
    let output = buffer.contents();
    assert!(!output.contains("info under warn"), "{output}");
    assert!(output.contains("warn under warn"), "{output}");

    assert!(filter_for(Some("  ")).1.is_none());
    let (buffer, guard) = capture(Some("  "));
    tracing::info!("info under blank");
    tracing::debug!("debug under blank");
    drop(guard);
    let output = buffer.contents();
    assert!(output.contains("info under blank"), "{output}");
    assert!(!output.contains("debug under blank"), "{output}");
}

#[test]
fn an_invalid_wayterm_log_value_warns_and_falls_back_to_info() {
    let (filter, warning) = filter_for(Some("wayterm=loud"));
    let warning = warning.expect("wayterm=loud is rejected");
    assert!(warning.contains(LOG_ENV), "{warning}");
    assert!(warning.contains("wayterm=loud"), "{warning}");

    let buffer = LogBuffer::default();
    let subscriber = subscriber(filter, buffer.clone(), None::<LogBuffer>);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("info under the fallback");
        tracing::debug!("debug under the fallback");
    });
    let output = buffer.contents();
    assert!(output.contains("info under the fallback"), "{output}");
    assert!(!output.contains("debug under the fallback"), "{output}");
}

#[test]
fn only_wayterm_targets_are_recorded() {
    let (buffer, guard) = capture(Some("trace"));
    tracing::error!(target: "zbus", "a dependency's event");
    tracing::error!(target: "wayterm::x", "an own event");
    drop(guard);
    let output = buffer.contents();

    assert!(!output.contains("a dependency's event"), "{output}");
    assert!(output.contains("an own event"), "{output}");
}

#[test]
fn the_log_path_follows_xdg_state_home_then_home() {
    let os = |s: &str| Some(OsString::from(s));
    assert_eq!(
        log_path_from(os("/state"), os("/home/u")),
        Some(PathBuf::from("/state/wayterm/logs/wayterm.log"))
    );
    assert_eq!(
        log_path_from(None, os("/home/u")),
        Some(PathBuf::from(
            "/home/u/.local/state/wayterm/logs/wayterm.log"
        ))
    );
    assert_eq!(
        log_path_from(os(""), os("/home/u")),
        Some(PathBuf::from(
            "/home/u/.local/state/wayterm/logs/wayterm.log"
        ))
    );
    assert_eq!(log_path_from(None, None), None);
    assert_eq!(log_path_from(os(""), os("")), None);
}

fn log_once_to_file(path: &Path, message: &str) {
    let (writer, worker) = open_file_sink(Some(path)).expect("the log file opens");
    let subscriber = subscriber(filter_for(None).0, LogBuffer::default(), Some(writer));
    tracing::subscriber::with_default(subscriber, || tracing::info!("{message}"));
    // Dropping the worker writes out what it holds.
    drop(worker);
}

#[test]
fn the_log_file_is_private_and_appended_to() {
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/wayterm/logs/wayterm.log");

    log_once_to_file(&path, "the first run");
    let contents = std::fs::read_to_string(&path).unwrap();
    let line = line_with(&contents, "the first run").expect("the line is in the file");
    assert!(line.contains("INFO"), "{line}");
    assert!(line.contains(TARGET), "{line}");
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert_eq!(mode(&path), 0o600);

    log_once_to_file(&path, "the second run");
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("the first run"), "{contents}");
    assert!(contents.contains("the second run"), "{contents}");
}

#[test]
fn an_unopenable_log_file_falls_back_to_stderr_only() {
    let dir = tempfile::tempdir().unwrap();

    let blocker = dir.path().join("not-a-dir");
    std::fs::File::create(&blocker)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let under_file = blocker.join("logs/wayterm.log");
    assert!(open_log_file(&under_file).is_err());
    assert!(open_file_sink(Some(&under_file)).is_none());

    // Root ignores directory modes. A file this process created is owned
    // by its effective uid, which reads it without `unsafe` libc calls.
    if std::fs::metadata(&blocker).unwrap().uid() != 0 {
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let under_locked = locked.join("wayterm/logs/wayterm.log");
        let opened = open_log_file(&under_locked).is_err();
        let sink = open_file_sink(Some(&under_locked)).is_none();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(opened);
        assert!(sink);
    }

    assert!(open_file_sink(None).is_none());

    let buffer = LogBuffer::default();
    let subscriber = subscriber(filter_for(None).0, buffer.clone(), None::<LogBuffer>);
    tracing::subscriber::with_default(subscriber, || tracing::info!("console only"));
    assert!(buffer.contents().contains("console only"));
}

#[test]
fn init_installs_once_and_later_calls_change_nothing() {
    // This installs the process-global subscriber and panic hook for the
    // test binary, on purpose, pointed at a buffer and a temp file; the
    // first call wins, so the closing `init()` never touches the real
    // state directory.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a/wayterm.log");
    let b = dir.path().join("b/wayterm.log");
    let console = LogBuffer::default();

    init_with(console.clone(), Some(&a), None);
    init_with(LogBuffer::default(), Some(&b), Some("trace"));
    init();

    assert!(a.exists());
    assert!(!b.exists());
    let output = console.contents();
    assert_eq!(
        output
            .lines()
            .filter(|line| line.contains("logging initialized"))
            .count(),
        1,
        "{output}"
    );
}

#[test]
fn a_panic_is_described_by_location_payload_type_and_message() {
    let here = Location::caller();
    let text: Box<dyn Any + Send> = Box::new("a str message");
    let owned: Box<dyn Any + Send> = Box::new(String::from("a String message"));
    let other: Box<dyn Any + Send> = Box::new(42_u32);

    let record = describe_panic(text.as_ref(), Some(here));
    assert!(record.contains("a str message"), "{record}");
    assert!(record.contains(file!()), "{record}");
    assert!(record.contains(&format!(":{}:", here.line())), "{record}");
    assert!(record.contains("(payload type: &str)"), "{record}");

    let record = describe_panic(owned.as_ref(), Some(here));
    assert!(record.contains("a String message"), "{record}");
    assert!(record.contains("(payload type: String)"), "{record}");

    let record = describe_panic(other.as_ref(), None);
    assert!(record.contains("unknown location"), "{record}");
    assert!(record.contains("unknown type"), "{record}");
}
