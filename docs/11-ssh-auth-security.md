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

Lookup goes keyring → prompt, first hit wins; storing (when the user opts
in) goes to the first source that can. The prompt is the challenge dialog
with one masked field; cancelling aborts the connect. Keyring attributes
are in doc 06 §Credential Storage. Status: the keyring source is a stub
(always misses), so secrets are prompted for every connect. When it is
wired: offer to delete a session's keyring entries when the session is
deleted.

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
The inner connection is never pooled. Fallback for exotic setups:
`sessions.ssh_options.ssh_w_fallback` runs `ssh -W target:port
[user@]jump` as the stream — the child carries no secrets and is killed
when the connection drops; the inner hop still authenticates through the
stream.

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
- **FreeRDP ≥ 3.20** for `/from-stdin` (doc 08). **VTE ≥ 0.72** for
  scrollback extraction; SIXEL from 0.62, detected at runtime.

Re-verify these when bumping a dependency; these APIs have moved before.
