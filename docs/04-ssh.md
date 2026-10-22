# SSH

SSH tabs pair a terminal with an SFTP sidebar over one shared,
authenticated connection. This doc covers the connection and its pool,
the terminal channel, host-key verification, the authentication chain,
tmux persistence, jump hosts and port forwarding. Where secrets come from
is doc 05; the sidebar is doc 07.

A connection manager that auto-accepts host keys is a MITM hole: that is
a release blocker, not a feature gap.

Code: `src/transport/` — `connection.rs` (`SshConnection`, the russh
handler), `supervisor.rs` (the pool), `ssh.rs` (`SSHTransport`),
`hostkey.rs`, `auth.rs`, `jump.rs`, `forward.rs`; `src/ui/tab/ssh.rs` (the
SSH connect flow), `hostkey_dialog.rs`, `auth_dialog.rs`,
`tunnels_dialog/`.

## Connection supervisor

A transport owns one channel, never the SSH session. The session is an
`SshConnection`: one authenticated russh handle that opens session
channels (`SSHTransport`), SFTP subsystem and exec channels (doc 07), and
direct-tcpip channels for jump hosts and local forwards. It is a module
boundary (AGENTS.md).

`ConnectionSupervisor` (one per window) shares one connection per
(host, port, username):

- consumers hold `Arc`s; a monitor task disconnects and evicts the
  connection once only the pool's own reference is left (checked every
  250 ms);
- concurrent dials for one key may race; the loser disconnects its own
  connection and adopts the winner's;
- dialing runs host-key verification and the auth chain. Because prompts
  block until answered, the UI runs the dial on tokio's blocking pool and
  each prompt hops to the GTK main thread; a gone main loop means reject
  or cancel.

So two tabs to the same user@host share one connection, and the SFTP
sidebar never logs in a second time.

## SSHTransport

`connect` opens a session channel, requests an `xterm-256color` PTY with
the terminal's measured grid, then starts either the login shell or tmux
(§Tmux session persistence), and splits the channel: the reader task owns
the read half, `write`/`resize` use the write half. stderr is shown inline
with stdout.

The reader tells an ending apart from a drop: an exit status or signal
followed by the close is `SessionEnded`; a close without one is
`ConnectionLost`. EOF alone ends nothing, since OpenSSH sends it before
the exit status. `disconnect` cancels the reader and drops the channel;
the shared connection stays up for its other consumers.

## Host key verification

### Policy

1. Use `~/.ssh/known_hosts`, one store shared with every other SSH client.
2. **Unknown host:** an OpenSSH-style prompt with the SHA-256 fingerprint.
   Accepting always records the key (hashed if the file uses hashing),
   so the host is never asked about twice.
3. **Changed key:** hard fail, never a one-click override.
4. The policy is global (`host_key_policy`, seeded `strict`) with a
   per-session override:
   - `strict`: unknown → prompt; changed → fail;
   - `accept-new`: unknown → record silently; changed → fail (OpenSSH's
     `StrictHostKeyChecking=accept-new`);
   - `off`: no verification, for labs only.

### Implementation

`AppClientHandler::check_server_key` runs during key exchange, before
authentication, and asks `HostKeyVerifier`, which maps russh's
`check_known_hosts_path`:

| Result | Meaning | Decision |
|---|---|---|
| `Ok(true)` | known, matching | accept |
| `Err(KeyChanged)` | known, **mismatched** | reject, no prompt |
| `Ok(false)` or another error | unknown (or unreadable file) | apply the policy |

A key the policy says to record but that cannot be written is rejected,
never accepted unrecorded. Host certificates are rejected. A rejection
surfaces as `TransportError::HostKeyRejected`, never as an auth failure.

## Authentication

The session row stores which method to use (`password`, `key`, `agent`,
`keyboard-interactive`, `none`; unset means `password`). Secrets come from
the credential chain at connect time (doc 05), never from the row.
Keyboard-interactive matters: 2FA/OTP and many bastions offer nothing
else.

The chain (`authenticate_chain`) runs in a fixed order — **agent → key →
password → keyboard-interactive** — but only the methods the session's
method enables, falling through on failure:

| Stored method | Tried |
|---|---|
| `password` | the looked-up password; a wrong one is re-prompted **once** ("Password (retry): "); then keyboard-interactive |
| `key` | the key at `key_path`; a passphrase is looked up only if the key is encrypted |
| `agent` | every identity `SSH_AUTH_SOCK` offers |
| `keyboard-interactive` | each `InfoRequest` round goes to the challenge dialog, one field per prompt, masked when `echo` is false, up to 32 rounds |
| `none` | nothing: authentication fails |

The RSA signature hash is negotiated (`best_supported_rsa_hash`), never
hardcoded.

The result is `Success`, `Cancelled` (any prompt dismissed: the connect
aborts cleanly, no error screen) or `Failed { reason, methods_remaining }`,
where `methods_remaining` lists the methods never attempted — so a
method's absence proves it was tried and rejected. Unreachable host, host
key rejected, auth failure and cancel are distinct outcomes in the UI.
Secrets are `Zeroizing`, never logged, never in SQLite; failure reasons
name the method and never contain a credential.

The challenge dialog (`src/ui/auth_dialog.rs`) is one UI for password
prompts, key passphrases and keyboard-interactive rounds; answers are
never logged or trimmed.

## Tmux session persistence

A per-session toggle (`sessions.tmux_enabled`). When on, `SSHTransport`
runs `tmux new -A -s wayterm-<name>` instead of a shell. `-A` attaches
when the session exists, so a dropped connection reconnects to the same
live session and its programs keep running. The `wayterm-` prefix keeps
clear of the user's own sessions, and the name is shell-quoted (`;` is
legal in tmux names).

`sessions.tmux_session_name` stores the bare suffix (the transport adds
the prefix); NULL means the session's own name. The session dialog shows
the full remote name, as `tmux list-sessions` prints it, strips one
leading `wayterm-` on save, and follows the Name field until the user
edits it. tmux applies to SSH only; local and serial sessions ignore the
flag.

Two tabs attaching to one tmux name would mirror each other, so only the
first tab of a session uses tmux: duplicates connect with a plain shell,
and a notice says so ("tmux disabled for this tab") unless the user ticked
its "Do not show me again" (`duplicate_tmux_notice`). Restored tabs follow
the same rule without the notice.

Disconnecting or closing a tab never kills the tmux session: persistence
is the point, so it is left for the user to end. tmux scrollback persists
on the remote host; that data at rest is out of scope.

## Jump hosts

One level of ProxyJump (`sessions.jump_session_id`, chosen in the session
dialog's SSH group): connect to the jump host, open a direct-tcpip channel
to the target (originator `127.0.0.1:0`, the OpenSSH `-W` convention),
and run the target's SSH session over that stream.

- Each hop authenticates as its own session, with its own method,
  credentials and host-key policy; host keys are verified on both hops.
- Depth > 1 and self-jump are connect errors
  (`transport::jump::check_jump_chain`), also refused in the session
  dialog.
- The jump hop comes from the supervisor like any connection. The target
  connection is never pooled — the pool key describes a TCP endpoint, not
  a connection backed by a jump channel — and holds its bastion's `Arc`
  for its whole life, so the supervisor cannot evict the bastion under a
  live target.
- No SFTP task runs over a jumped connection; the sidebar says so instead
  of attaching.

**`ssh -W` fallback.** With `ssh_options.ssh_w_fallback` set, a target
whose direct-tcpip channel the jump hop refuses is reached through
`ssh -W target:port [user@]jump -o BatchMode=yes` as the stream (OpenSSH
never prompts on a tty Wayterm does not own). The child carries no secrets
and is killed when the connection drops. Trust note: on that hop
OpenSSH — not Wayterm — verifies the host key and authenticates (its
`known_hosts`, agent and config); the target hop is still verified and
authenticated by Wayterm.

## Port forwarding

Rules live in `sessions.forward_rules` (doc 09 §Schema): kind, bind
address and port, remote host and port, enabled. An empty bind address
means loopback, never `0.0.0.0`.

The SSH tab's connect flow owns one `ForwardEngine` per live connection
(`src/transport/forward.rs`):

- `start(rules)` serves every enabled Local rule: one `TcpListener` each;
  every accepted connection opens one direct-tcpip channel to the rule's
  remote and pipes both directions with `copy_bidirectional`. A refused
  remote closes only that socket; the listener keeps serving.
- `set_enabled` toggles one rule live; `stop` closes listeners and aborts
  the per-connection tasks.
- Status flows as `ForwardEvent`s — `Listening`, `Active { conns }`,
  `LastFailed { error }`, `Stopped` — one per transition.
- Listeners close with their tab. Every (re)connect stops the old engine
  and starts a fresh one on the new connection, so forwards come back
  after a reconnect.

### Tunnels dialog

The header bar's **Tunnels…** button opens the current session tab's
Tunnels dialog (`src/ui/tunnels_dialog.rs`): one row per rule with its
kind, bind, remote, Enabled toggle and live status. While the tab is
connected, toggling Enabled applies to the engine at once; offline, the
dialog still opens for editing and every status reads "no active
connection". Add rule and Remove edit the list; OK validates (ports
1–65535, required fields) and saves the rules to the session row; a help
popover explains each control. The session dialog's SSH group embeds the
same rule editor, so both accept exactly the same input.

## Version anchors

- **russh 0.64:** `known_hosts::{check,learn}_known_hosts_path` with the
  mapping above; keyboard-interactive is client-driven
  (`authenticate_keyboard_interactive_start`/`_respond`, responses
  `Success | Failure | InfoRequest { prompts }`); agent auth via
  `AgentClient::connect_env` → `request_identities` →
  `authenticate_publickey_with`.
- **russh-sftp 3.0:** doc 07.

Re-verify these when bumping a dependency; these APIs have moved before.

## Limitations

- **The changed-key warning is never shown.** A changed key is rejected
  without a prompt, as it must be, but the tab shows the rejection as a
  plain connection error; the warning dialog that explains the risk and
  the `ssh-keygen -R` remedy (`hostkey_dialog::prompt_changed`) exists
  but nothing calls it.
- Host certificates are rejected; there is no CA support.
- The `off` policy has no badge in the UI.
- `ssh_options.compression`, `keepalive_interval` and `keepalive_count`
  are stored but not applied. The session dialog edits no `ssh_options`
  field, so `ssh_w_fallback` can only be set by a JSON import (doc 10).
- Only Local forwards: Remote (`ssh -R`) and Dynamic (SOCKS, `ssh -D`)
  rules can be stored and are shown marked, but never start. No agent or
  X11 forwarding.
- Jump depth is one hop. Jumped connections are not deduplicated (two tabs
  to the same target through the same bastion open two target
  connections), and have no SFTP sidebar.
- Tunnels exist only while a tab is open; there are no tunnel-only
  sessions.
- Plain (non-tmux) shells on one shared connection cannot be told apart
  by the SFTP sidebar's directory probe (doc 07 §Limitations).
