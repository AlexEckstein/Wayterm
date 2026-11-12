# Diagnostics Logging

What Wayterm records about its own run, where the record goes, and how to
turn it up: a log file and stderr, fed by `tracing` events from a starter
set of call sites. It is a diagnostics log for the person running Wayterm,
not a session transcript: terminal contents are never part of it.

Code: `src/logging.rs` (the subsystem, tests in `logging/tests.rs`); the
call sites are listed in §What is logged.

## Where it goes

Every event goes to two places, as one plain UTF-8 line each, without
color codes:

- **stderr**, as the scattered `eprintln!` notes always did;
- **the log file**, `$XDG_STATE_HOME/wayterm/logs/wayterm.log` (default
  `~/.local/state/wayterm/logs/`; an empty `XDG_STATE_HOME` counts as
  unset). The file is appended to, never truncated. Missing directories
  are created with mode 0700 and the file with 0600: the log names hosts
  and local paths, like the session store (doc 09 §Database).

A line holds a UTC timestamp (RFC 3339, ending in `Z`), the level, the
target — the module that logged — and the message with its fields:

```
2026-11-12T09:14:03.512345Z  INFO wayterm::transport::supervisor: connecting host=example.com port=22 session=7
```

## Filter

`WAYTERM_LOG` holds the filter, in `tracing_subscriber::EnvFilter`
directive syntax: a bare level (`debug`), or comma-separated
`target=level` pairs (`wayterm::sftp=debug,warn`). Levels are `error`,
`warn`, `info`, `debug`, `trace`.

- Unset or blank: `info`.
- A value that does not parse (`wayterm=loud`): `info`, and one warning on
  stderr naming the value.
- `filter_for` resolves the value, so the tests drive the function
  production uses.

Whatever the filter says, only Wayterm's own events (targets under
`wayterm`) are recorded. Dependencies that use `tracing` are left out on
purpose: zbus, under the keyring, traces whole D-Bus messages, and a
Secret Service reply must never reach a log (doc 05). For the same reason
the `log` bridge is not installed, so russh's wire-level `log` output is
not captured either.

## What is logged

| Module | Events |
|---|---|
| `logging` | the init result: the log file in use, or that logging is stderr only |
| `app` | start (with the version) and shutdown; the custom-hotkey load report (doc 11 §Persistence); a tray that failed to start (doc 03 §Tray) |
| `store::db` | the store opened, with how long it took; the schema steps done, with their time and whether the seed ran (doc 09 §Database); a store that failed to open; a settings row that could not be read |
| `transport::supervisor` | a dial starting and how it ended (connected, or the connect error); a pooled connection found closed and evicted; the last consumer's release and the disconnect that follows; the supervisor going away (doc 04 §Connection supervisor) |
| `transport::connection` | authentication succeeding, failing (with the chain's reason) or being cancelled; a rejected host key or host certificate; a disconnect |
| `transport::reconnect` | each backoff wait and attempt of a reconnect run, and how the run ended (doc 03 §Reconnect) |
| `sftp::task`, `sftp::transfer` | an SFTP session opened or failing to open; each upload and download starting, finishing (bytes, duration), failing (error, duration) or being cancelled, with its local and remote path (doc 07 §Transfers) |

Connection events carry the host, the port and the session's id: the
`sessions` row id its credentials are looked up under, or `_live_connect`
without a saved session (doc 05 §Session identity). Failures are warnings or
errors with the protocol's own message; routine steps are `info`, and
`debug` adds pool reuse and lost dial races.

**Never logged:** passwords, passphrases, keyboard-interactive answers,
keyring contents, private keys, host-key material, RDP stdin, or anything
else from the credential chain (doc 05); user names; server banners;
terminal input and output. The authentication chain's failure reasons name
the method and never contain a credential (doc 04 §Authentication).

## Lifecycle

`logging::init()` is the module's one public function. `app::run` calls it
first, on the GTK main thread, before the tokio runtime and the window
exist (doc 01 §Threading model). Logging from any thread is then safe:
an event is formatted on the thread that logs, written to stderr there as
an `eprintln!` was, and handed to the file's writer thread through
`tracing_appender::non_blocking`, so no call site waits on the disk,
holds a lock across an `.await`, or needs a channel of its own.

- **Idempotent.** The first call installs the process-wide subscriber and
  the panic hook; later calls do nothing.
- **Never fails startup.** If the log file cannot be created — no
  `XDG_STATE_HOME` or `HOME`, a read-only directory — one line on stderr
  says so and logging goes to stderr only.
- **Flush.** The writer thread's guard lives in a static for the whole
  run. `app::run` logs the shutdown and then drops the guard
  (`logging::flush`), which writes out whatever is still queued before
  the process exits.

## Panic hook

`init` wraps the panic hook that was installed before it. A panic is
logged as one `error` event — its location, message and payload type
(`describe_panic`), with the panicking thread's name — and then the
previous hook runs, so the usual message and backtrace still appear on
stderr.

A panic on the main thread ends the process, by unwinding out of `main` or
by aborting at a GTK callback's FFI boundary, and the writer thread would
race that exit. So for a main-thread panic the hook also flushes the file
before it hands on. A panic on any other thread (a tokio task's is caught
by the runtime) leaves the writer running.

## Testing

`src/logging/tests.rs` (doc 15 §Unit and display-free tests), with no
display and no process-global state except where noted:

- events are captured through the production subscriber over a
  `MakeWriter` on a shared buffer, installed for the test's thread only
  (`capture`): levels, the default filter, `WAYTERM_LOG` values through
  `filter_for`, and the own-targets rule. Other modules' tests use the
  same helper for the events of §What is logged;
- the log path from given `XDG_STATE_HOME` and `HOME` values, never from
  the environment;
- the file sink against a temporary directory: the line written, the
  modes, appending; and the failure paths — a parent that is a file, a
  read-only directory — falling back without a panic;
- `init` twice: this one test installs the process-wide subscriber and
  hook for the test binary, pointed at a buffer and a temporary file;
- `describe_panic` on synthesized payloads. The hook itself is not
  observed: a parallel test runner cannot install and watch a global hook
  safely.

## Limitations

- No rotation, size cap or clean-up: the file grows until the user
  removes it. No log shipping, no journald or syslog target.
- A starter set of call sites (§What is logged). The other diagnostics
  still go to stderr only, as `eprintln!` lines: host-key rejections'
  reasons, the keyring and Bitwarden lookups, port forwards, jump dials,
  the local file task's copies, and most of the UI.
- RDP and VNC are not instrumented.
- Reconnect events carry the attempt number but not the host: the
  reconnect driver does not know what it redials (doc 03 §Reconnect). The
  supervisor's line for the dial that follows names it.
- No log viewer, and no setting: the filter is the environment variable
  only, read once at startup.
- Dependencies' own `tracing` and `log` output is never recorded
  (§Filter), so a D-Bus or SSH library problem shows only through the
  error Wayterm reports for it.
- Only the shutdown path and a main-thread panic flush the file. A
  process that is killed, or that aborts elsewhere, can lose the last
  events still queued for the writer thread; and after a main-thread
  panic has flushed, nothing further reaches the file.
- Under sustained load the non-blocking writer drops events rather than
  block the thread that logs.
- The file's modes are set when it is created; an existing file or
  directory keeps the modes it has.
- The Flatpak's log location is not verified (doc 14).
