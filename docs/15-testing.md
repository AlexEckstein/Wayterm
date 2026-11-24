# Testing

What is tested, where, and how to run each kind. The transport and most UI
logic are testable without a display because of the trait split (doc 01
§Seams) and the display-free models beside each widget.

## Running the tests

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
scripts/run-sshd-tests.sh          # sshd suites against a throwaway local sshd (needs sudo)
scripts/auth-matrix-sshd.sh        # the auth-method matrix, one sshd per mode
# widget tests; force X11 so a Wayland session cannot capture them
env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test -- --ignored --skip ghostty_display
env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test -- --ignored ghostty_display
```

Suites that need a server or hardware skip themselves (a `println!` and an
early return) when their environment is not set, so `cargo test` is green
on any machine.

These build with the default features, the Ghostty backend among them
(doc 14 §Cargo features). `--no-default-features` on any of them runs the
build without it, which CI's `minimal` job keeps passing.

## CI

`.github/workflows/ci.yml` runs on pushes to main and on every PR:

- **docs:** every relative Markdown link resolves.
- **rust:** `cargo fmt --check`, `cargo clippy --all-targets -D warnings`,
  `cargo test`, the VTE search and Ghostty widget tests under xvfb, then
  a real sshd for
  `tests/transport_sshd.rs`: the runner's own sshd on port 22 with a
  throwaway `wayterm-test` user (random password and a fresh ed25519
  key), exported as `SSH_TEST_*`. Debug info is line-tables-only so the
  GTK test binaries fit the runner's disk. The build has the Ghostty
  backend (§Ghostty backend): `build.rs` builds libghostty-vt as on any
  machine (doc 02 §libghostty-vt pin), and the job caches that
  directory under a key hashed from `build.rs`, so Zig and upstream's
  tarballs are downloaded only after a bump or another change to the
  script. The job fails first if a checksum in `build.rs` is missing
  from doc 02, and a failed run ends by printing the runner's CPU model
  and flags.
- **minimal:** clippy and `cargo test` with `--no-default-features`, the
  build without the Ghostty backend.
- **audit:** a weekly `cargo audit` sweep. It opens one issue per
  advisory (vulnerability, unmaintained, unsound or notice), titled with
  the advisory ID, and skips an ID already in the title of any issue or
  PR, open or closed. The job does not fail on a finding: the issue is
  the alert. It has the workflow's only `issues: write`.
- **dependabot:** opens weekly update PRs for `cargo` (minor/patch
  grouped into one PR, majors separate) and for the `github-actions`
  versions used by the workflows. Every action is pinned to a full
  commit SHA with its version in a trailing comment, so a moved tag
  cannot change what runs; Dependabot updates the SHA and comment
  together, and waits seven days after an action's release before
  proposing it. The Rust toolchain is installed with the runner's own
  `rustup`, not an action.

## Unit and display-free tests

Every module carries its own `tests` (inline or `<module>/tests.rs`).
Highlights:

- **Store:** schema, seed, pragmas, reopening an existing database, CRUD
  and foreign-key behavior; the serde spellings of every
  enum column (doc 09 §Wire formats); `host_keys` written on insert and
  merged by `add_session_host_keys`, never wiped by `update_session`.
- **Host keys:** temp `known_hosts` files — unknown, matching, mismatched,
  `accept-new`, `off`, hashed entries; a key of another type, a revoked
  key, an unreadable file and host names with known_hosts syntax; the
  reader's own cases (tabs, patterns, negation, markers). Every row of the
  decision table with a session key set: a session-trusted key accepted
  without a question, a changed key accepted for the session (known_hosts
  byte-identical) or denied, a session's own keys making an unrecorded
  host changed, `@revoked` and failing closed outranking the set, `off`
  never asking, and session key types offered first.
- **Pool keys:** sessions trusting different keys never share a
  connection, the same keys do, verified and unverified stay apart, and a
  dial that accepted a key is pooled under the grown set
  (`src/transport/supervisor/tests.rs`).
- **Host-key dialogs:** the copy of both questions and each question's
  answers mapped to its own decision (`src/ui/dialogs/hostkey/tests.rs`);
  the connect flow seeding each hop from its row and saving what each hop
  accepted to its own row (`src/ui/tabs/ssh.rs`).
- **Wiring:** `FakeBackend` drives the pump, bridge and drain
  (`src/ui/tabs/terminal_tab/tests.rs`, `tests/local_serial_flow.rs`).
- **Reconnect:** the policy and driver against tokio's paused clock, so the
  backoff curve is asserted in virtual time (`tests/reconnect.rs`).
- **Host-key types:** an in-process russh server holding two host keys is
  asked for the type known_hosts records, or the session trusts, and one
  showing only a key of another type is asked about as a changed key under
  `strict` and `accept-new`. A changed key accepted for the session
  connects and joins its set, a denied one is `HostKeyRejected`, a redial
  with the saved set is not asked again, and another session is never
  handed that connection (`tests/host_key_types.rs`; the two ECDSA cases
  skip without `ssh-keygen`).
- **Keepalives:** an in-process russh server behind a proxy that goes
  silent: keepalives close the connection, and the pool dials fresh while
  the dead one is still held (`tests/keepalive.rs`).
- **Compression:** an in-process russh server that only speaks
  `zlib@openssh.com` echoes data for a compressing session and finds no
  common method for one that does not; a server without zlib takes a
  compressing session uncompressed. The measured saving is over 50% on
  repeated lines, at most 5% when the server declined, and absent for a
  session that never offered compression (`tests/compression.rs`).
- **Jumped sessions:** an in-process russh server playing both hops: a
  target dialed by `dial_with_jump` survives the supervisor's release
  poll, and dropping it releases the bastion (`tests/jump_bastion.rs`).
- **Hotkeys:** registry, conflict rules, the default table's invariants,
  trigger parsing, persistence and JSON layouts; end to end through a real
  SQLite file (`tests/hotkeys.rs`) and the settings page's view model
  (`tests/settings_ui.rs`).
- **Import/export:** every `~/.ssh/config` form (templates, `Match`, the
  `Include` chain with cycles, depth cap and non-regular files,
  `ProxyJump`), position remapping, a JSON export → import roundtrip,
  newer-version refusal, no secret in an export, one fixture per foreign
  format (`src/transfer/tests.rs`).
- **SFTP sidebar, Tunnels dialog, session dialog, host picker, tab state,
  status bar, tray:** their display-free models.
- **Local file task:** listing (links, order), mkdir, rename, delete and
  both copies against a temporary directory, the refusals that keep a
  file from being replaced or emptied and a FIFO from being waited on,
  and shutdown
  (`src/sftp/local/tests.rs`).
- **RDP:** the argv builder (order, toggles, never `/cert:ignore` or
  `/p:`, no password), the certificate decisions and fingerprint parsing,
  the version gate (`src/rdp/*/tests.rs`, `tests/rdp_tab_flow.rs`).
- **Ghostty:** the engine suite
  (`src/terminal/ghostty/engine/tests/`) drives the FFI end to end and
  checks the C struct layouts; a `--no-default-features` run has none of
  it.

### Macros

Display-free: the recorder's pause arithmetic and report filtering, the
notation (an exhaustive roundtrip over all byte pairs), the tap's ordering
and arming, playback order, stop and a dropped terminal (`FakeBackend`),
the editor's `Draft` operations and validation, and the store's CRUD.
Under xvfb: the editor's rows and validation, the panel's
list and toolbar, and a record-then-play roundtrip through a real VTE
terminal, including a non-UTF-8 byte.

## Widget tests

GTK binds to the first thread that initializes it, so every widget check
in the library runs from one ignored test, `ui::gtk_tests::widgets`, under
xvfb, with GLib warnings and criticals made fatal for each check. It
covers the session dialog, Tunnels dialog, session tree (menu, nesting,
drag moves, folder cycles), SFTP sidebar (the header's column menu, a
file opened and its save uploaded through a fake task, a local tab's
sidebar over the real file task), side dock, host
picker, macro editor and panel (record and play through a real VTE),
icons, folder dialog, Settings window, tab focus and close behavior, what
an SSH tab lets go of when its session ends (forwards, transport, SFTP
task), a local-shell tab that connects and closes and whose sidebar
follows its shell, color schemes and the
system theme, the user stylesheet reload, terminal opacity (the rendered
alpha of a VTE terminal and of the window under it, across tab switches),
the VNC view, RDP embedding, the tab bar's "+", the status bar's segments
and tooltips, and that every shortcut
has a window action.

Other ignored display tests: VTE scrollback extraction
(`tests/terminal_polish.rs`), VTE search (`tests/vte_search.rs`) and the
Ghostty widget (`-- --ignored ghostty_display`). The Ghostty widget test
is in the library beside `ui::gtk_tests::widgets` and initializes GTK on
its own thread, so the two cannot share a run: a plain `-- --ignored`
fails one of them, and each runs with the other left out (§Running the
tests).

## sshd suites

`tests/common/mod.rs` is the shared fixture: the suites skip unless
`SSH_TEST_HOST`, `SSH_TEST_PORT`, `SSH_TEST_USER`, `SSH_TEST_PASSWORD`
(and `SSH_TEST_KEY_PATH` for key auth) are set.
`scripts/run-sshd-tests.sh` starts a separate sshd on port 2422 from
`scripts/sshd-test-config.inc` — never touching the system sshd — and runs
them all with `--test-threads=1`:

| Suite | Proves |
|---|---|
| `transport_sshd` | the SSH transport, auth chain and SFTP task: listing (above the home too), round-trip transfers with progress, a refused folder upload, mkdir/rename/delete, errors after disconnect, the server uptime probe, the server bandwidth stream (the one CI runs) |
| `connect_flow` | one shared connection per identity, kept alive by any consumer, released after the last |
| `tmux_persistence` | the tmux command, the same tmux session id and environment surviving a dropped transport *and* connection, the off path, and a program in the pane surviving the auto-reconnect |
| `reconnect_sshd` | the reconnect driver's curve and cancellation against a real redial |
| `forward_sshd` | a local forward end to end (curl-style echo through the tunnel) |
| `jump_sshd` | a target reached over the jump hop's channel, with per-hop auth and the depth/self-jump guards |

The single-sshd rig cannot express network isolation: jump target and jump
host are the same server, and the forwarded service runs on the sshd's own
host. The suite headers say so.

## Auth-method matrix

`tests/auth_matrix.rs` against one sshd per mode
(`scripts/auth-matrix-sshd.sh`, base config
`scripts/sshd-auth-matrix-config.inc` plus a per-mode fragment), via
`SSH_MATRIX_*`. Local and pre-merge only: CI's single all-methods sshd
cannot express it, and the suite skips there.

| Mode | `AuthenticationMethods` | Other toggles |
|---|---|---|
| password-only | `password` | pubkey **on** (the client must offer its key and be refused, or the fall-through is not proven), kbd-interactive off |
| kbd-interactive-only | `keyboard-interactive:pam` | needs `UsePAM yes`; password and pubkey off |
| key-only | `publickey` | password and kbd-interactive off |

Assertions use `methods_remaining`: a method missing from it was tried and
rejected. Per mode: the other methods fail, the full chain succeeds, a
wrong password re-prompts exactly once, and a missing key file fails
cleanly while the chain continues. An `sshd -t` refusal of a mode is
reported as a finding, not forced. All three modes passed on OpenSSH
10.0p2.

## VNC

`tests/common/vnc.rs` is a hand-rolled RFB 3.8 server (both auth paths,
Raw/CopyRect/DesktopSize updates, client-event recording, failure modes)
on loopback, driving the real vnc-rs client and the engine in
`tests/vnc_wire.rs`; deterministic, so it runs in CI. `tests/vnc_live.rs`
runs against a real server when `VNC_TEST_HOST`, `VNC_TEST_PORT` and
`VNC_TEST_PASSWORD` are set (e.g. x11vnc on an Xvfb display).

## RDP

`tests/rdp_tab_flow.rs` covers the display-free state mapping; its live
hook is ignored and gated on `RDP_TEST_HOST`, `RDP_TEST_PORT`,
`RDP_TEST_USER`, `RDP_TEST_PASSWORD` and `RDP_TEST_FINGERPRINT` (an xrdp
server). The embedding is covered by the widget tests.

## Serial

`tests/serial_pty.rs` runs `SerialTransport` against a PTY pair: the
slave is a genuine tty that `serialport` opens by path, the master plays
the far end of the cable. With `SERIAL_TEST_DEVICE` and `SERIAL_TEST_BAUD`
set, the same checks run against real hardware
(`scripts/serial-loopback.sh`); `SERIAL_TEST_PEER` names the second port of
a two-port null-modem loopback, otherwise the device needs a loopback plug.

## Ghostty backend

The backend is in the default build, so the commands under §Running
the tests cover its clippy, its unit tests and its display test. The
benchmark is the rest:

```sh
cargo bench                                 # doc 02 §Benchmark
```

CI runs all but the benchmark (§CI).

## Limitations

- CI runs only `transport_sshd` against its sshd; `connect_flow`,
  `tmux_persistence`, `reconnect_sshd`, `forward_sshd` and `jump_sshd`
  need only that same sshd but run locally only.
- Of the display tests, CI runs only VTE search and the Ghostty widget:
  the widget suite (`ui::gtk_tests::widgets`) and the VTE scrollback test
  (`tests/terminal_polish.rs`) run locally only, as does the benchmark.
- The serial hardware path has not run against real hardware; Tight/ZRLE
  VNC decoding is exercised only against real servers, not the mock.
- No fuzzing yet: `known_hosts` handling and SFTP path joining are the
  first candidates.
