# SSH Authentication and Security

Host-key verification, the authentication chain, credential handling, and
the design for tunnels. A connection manager that auto-accepts host keys
is a MITM hole: that is a release blocker, not a feature gap.

## Host Key Verification

### Policy

1. Use `~/.ssh/known_hosts` — one store shared with every other SSH client.
2. **Unknown host:** the OpenSSH-style prompt with the SHA-256 fingerprint.
   Accepting records the key (hashed if the file uses hashing).
3. **Changed key:** hard fail, never a one-click override. The warning
   explains the risk and the `ssh-keygen -R` remedy.
4. Policy is global (`host_key_policy`) with a per-session override:
   - `strict` (default): unknown → prompt; changed → fail;
   - `accept-new`: unknown → record silently; changed → fail;
   - `off`: no verification — lab use only, to be badged in the UI.

### Implementation

`AppClientHandler::check_server_key` (during key exchange, before auth)
asks `HostKeyVerifier`, which maps russh 0.64's `check_known_hosts_path`:

| Result | Meaning | Decision |
|---|---|---|
| `Ok(true)` | known, matching | accept |
| `Err(KeyChanged)` | known, **mismatched** | reject, no prompt |
| `Ok(false)` or another error | unknown (or unreadable file) | apply the policy |

A key the policy says to record but that cannot be written is rejected,
never accepted unrecorded. Host *certificates* are rejected ("not
supported yet"). A rejection surfaces as `TransportError::HostKeyRejected`,
never as an auth failure. The prompt closure hops to the GTK thread while
the dial waits on the blocking pool; a gone main loop means reject.

## Authentication State Machine

### Auth methods

The session stores which method to prefer (`password`, `key`, `agent`,
`keyboard-interactive`, `none`); secrets come from the credential manager
at connect time. Keyboard-interactive is not optional: 2FA/OTP and many
bastions offer nothing else.

### Flow

Order: **agent → key → password → keyboard-interactive**, each tried only
when configured; a failure falls through to the next method.

1. **Agent:** every identity `SSH_AUTH_SOCK` offers.
2. **Key:** loaded from `key_path`; a passphrase is asked for only if the
   key is encrypted. The RSA hash is negotiated (`best_supported_rsa_hash`),
   never hardcoded.
3. **Password:** a wrong password is re-prompted **once** ("Password
   (retry): ") through the challenge responder.
4. **Keyboard-interactive:** each `InfoRequest` round goes to the challenge
   dialog (one field per prompt, masked when `echo` is false), up to 32
   rounds.

The result is `Success`, `Cancelled` (any prompt dismissed — the connect
aborts cleanly, no error screen), or `Failed { reason, methods_remaining }`,
where `methods_remaining` lists the methods never attempted. Unreachable
host, auth failure, and exhausted methods are distinct UI outcomes.
Secrets are `Zeroizing`, never logged, never in SQLite.

## CredentialManager

Lookup walks the sources in priority order, first hit wins:
**keyring → Bitwarden (when enabled) → prompt**. The prompt is always
last: it is the challenge dialog with one masked field, and cancelling it
aborts the connect. Storing walks the same list and uses the first source
that can; a source that cannot store answers `Unavailable` and the walk
moves on (only the keyring stores). The chain is built per connect from
the Settings rows (doc 06 §Credential Storage), so a toggle applies to
the next connect.

- **Availability hint.** When the keyring backend cannot be reached, the
  prompt's instructions line reads "Keyring unavailable — prompting for
  credentials." so a prompt that used to be answered silently is
  explained.
- **Store on save.** The session dialog's **Save password to keyring**
  box stores the typed password under the saved session's id (doc 06).
  A refusal is shown inline; the session save is not undone.
- **Delete on session delete.** Deleting a session deletes its keyring
  entries for every secret type; failures are logged, never blocking.
- **Fallback identity.** The SSH auth chain currently looks secrets up
  under the placeholder session id `_live_connect`, not the session's
  real id, so keyring entries keyed by id are only reached once the real
  id is threaded through. The Bitwarden source is unaffected: it is
  built per session and keys on the session name.

### Credential sources

- **Keyring** — any Secret Service (`org.freedesktop.secrets`) provider:
  GNOME Keyring, KWallet's Secret Service bridge, or KeePassXC with its
  Secret Service integration enabled, which makes a KeePassXC database a
  drop-in store with no KeePass-specific code in Wayterm. Attributes are
  in doc 06 §Credential Storage.
- **Bitwarden** — the `bw` CLI, off by default (`bitwarden.enabled`), the
  binary from `bitwarden.bin` (default `bw`). A session looks up the item
  named `wayterm/<session name>` (exact name, no aliases) with
  `bw get password --raw wayterm/<name> --nointeraction`; trimmed stdout
  is the secret, and it answers whichever secret type the session's
  method asks for. The vault session key is read from `BW_SESSION` in
  Wayterm's environment when the source is built and handed to the child
  through its environment only — never argv, which any local user can
  read in `/proc/<pid>/cmdline`; Wayterm never writes `BW_SESSION`
  itself. Each lookup runs one `bw` (10 s timeout, then killed), nothing
  is cached between connects, and a missing binary, non-zero exit,
  timeout, or empty output is a logged miss that falls through to the
  prompt. The source is read-only: storing answers `Unavailable`. The
  Settings page's status probe runs `bw status --nointeraction` without
  `BW_SESSION` and keeps only the `status` field.

## RDP credentials

Passwords go to xfreerdp on stdin, never argv or the environment; details
and the certificate question are in doc 08.

## Jump hosts and port forwarding (Phase 11)

Both build on `SshConnection::open_direct_tcpip` (doc 04)
(originator `127.0.0.1:0`, OpenSSH `-W` convention).

### Jump hosts

One level of ProxyJump (`sessions.jump_session_id`): connect to the jump
host, open a direct-tcpip channel to the target, and run the target's SSH
session over that stream (as `russh-config` does for ProxyCommand). Each
hop authenticates on its own with its own method/credentials; depth > 1
and self-jump are connect errors (`transport::jump::check_jump_chain`).
The inner connection is never pooled (doc 04 §Connection Supervisor).
SFTP is not available over a jumped session in v1 — the sidebar says
so instead of attaching. Fallback for exotic setups:
`sessions.ssh_options.ssh_w_fallback` runs `ssh -W target:port
[user@]jump` as the stream — the child carries no secrets and is killed
when the connection drops; the inner hop still authenticates through
the stream. Trust note: on the `ssh -W` hop OpenSSH — not Wayterm —
verifies the host key and authenticates (its `known_hosts`, agent and
config); the target hop is still verified and authenticated by Wayterm.

### Port forwarding

Rules live in `sessions.forward_rules` (doc 06). The caller owns a
`transport::forward::ForwardEngine` per live connection: `start(rules)`
serves every enabled local rule (one `TcpListener` each, one
direct-tcpip channel per accepted connection piped with
`copy_bidirectional`), `set_enabled` toggles one rule live, and `stop`
closes listeners and aborts per-connection tasks. Status flows as
`ForwardEvent`s (`Listening` / `Active { conns }` / `LastFailed` /
`Stopped`) for the Tunnels dialog. An empty `bind_addr` means loopback,
never `0.0.0.0`. Listeners close with the connection and return on
reconnect: the reconnect driver stops the old engine and starts a fresh
one on the new connection `Arc`. Dynamic (SOCKS) and remote
forwards (which need `tcpip-forward` handler callbacks) are v2; so are
agent and X11 forwarding.

## Testing Strategy

The transport is testable without a GUI because of the trait split.

1. **Real sshd.** CI provisions the runner's sshd (port 22, throwaway
   `wayterm-test` user, `SSH_TEST_*` in `GITHUB_ENV`) and runs
   `tests/transport_sshd.rs`. Locally, `scripts/run-sshd-tests.sh` starts a
   separate instance on port 2422 from `scripts/sshd-test-config.inc`,
    never touching the system sshd, and also runs `connect_flow`,
    `tmux_persistence`, `reconnect_sshd`, `forward_sshd`, and `jump_sshd`.
2. **Host-key decisions:** unit tests over temp known_hosts files —
   unknown, matching, mismatched, `accept-new`, `off`, hashed entries.
3. **Auth-method matrix:** `tests/auth_matrix.rs` against one sshd per
   mode (`scripts/auth-matrix-sshd.sh`, base config
   `scripts/sshd-auth-matrix-config.inc` plus a per-mode fragment). Local
   and pre-merge only: CI's single all-methods sshd cannot express it, and
   the suite skips there.

   | Mode | `AuthenticationMethods` | Other toggles |
   |---|---|---|
   | password-only | `password` | pubkey **on** (the client must offer its key and be refused, or the fall-through is not proven), kbd-interactive off |
   | kbd-interactive-only | `keyboard-interactive:pam` | needs `UsePAM yes`; password and pubkey off |
   | key-only | `publickey` | password and kbd-interactive off |

   Assertions use `methods_remaining`: a method missing from it was tried
   and rejected. Per mode: the other methods fail, the full chain
   succeeds, a wrong password re-prompts exactly once, and a missing key
   file fails cleanly while the chain continues. An `sshd -t` refusal of a
   mode is reported as a finding, not forced. All three modes passed on
   OpenSSH 10.0p2.
4. **FakeBackend** tests for the pump and bridge (doc 02).
5. **SFTP** against the real sshd: listing, round-trip transfers with
   progress, mkdir/rename/delete, errors after disconnect.
6. **Fuzzing** (later): known_hosts handling and SFTP path joining.

CI also runs fmt, clippy `-D warnings`, the whole test suite, and a weekly
`cargo audit`.

## Limitations (v1)

- Host certificates are rejected; CA support is v2.
- The `off` policy exists for labs and is meant to be loud.
- No agent, X11, remote, or dynamic forwarding; jump depth ≤ 1.

## Version anchors

- **russh 0.64:** `known_hosts::{check,learn}_known_hosts_path` with the
  mapping above; keyboard-interactive is client-driven
  (`authenticate_keyboard_interactive_start`/`_respond`, responses
  `Success | Failure | InfoRequest { prompts: Vec<Prompt { prompt, echo }> }`);
  agent auth via `AgentClient::connect_env` → `request_identities` →
  `authenticate_publickey_with`.
- **russh-sftp 3.0:** `SftpSession::new(channel.into_stream())`;
  `read_dir` is a sync iterator; `remove_file`/`remove_dir`; async
  `File::close`.
- **oo7 0.6:** the Secret Service client behind the keyring source
  (schema and attributes in doc 06 §Credential Storage).
- **FreeRDP ≥ 3.20** for `/from-stdin` (doc 08). **VTE ≥ 0.72** for
  scrollback extraction; SIXEL from 0.62, detected at runtime.

Re-verify these when bumping a dependency; these APIs have moved before.
