# SSH

SSH tabs pair a terminal with an SFTP sidebar over one shared,
authenticated connection. This doc covers the connection and its pool,
the terminal channel, keepalives, compression, server uptime,
bandwidth, load and disks, host-key verification,
the authentication chain, tmux persistence, jump hosts and port
forwarding. Where secrets come from
is doc 05; the sidebar is doc 07.

A connection manager that auto-accepts host keys is a MITM hole: that is
a release blocker, not a feature gap.

Code: `src/transport/` — `connection.rs` (`SshConnection`, the russh
handler), `supervisor.rs` (the pool), `ssh.rs` (`SSHTransport`),
`hostkey.rs`, `auth.rs`, `jump.rs`, `forward.rs`, `traffic.rs`,
`host_stats.rs`, `bandwidth.rs`, `load.rs`, `disks.rs`, `remote_script.rs`;
`src/ui/tabs/ssh.rs` (the
SSH connect flow), `src/ui/dialogs/` — `hostkey.rs`, `auth.rs`,
`tunnels/`.

## Connection supervisor

A transport owns one channel, never the SSH session. The session is an
`SshConnection`: one authenticated russh handle that opens session
channels (`SSHTransport`), SFTP subsystem and exec channels (doc 07), and
direct-tcpip channels for jump hosts and local forwards. It is a module
boundary (AGENTS.md).

`ConnectionSupervisor` (one per window) shares one connection per
(host, port, username), kept apart by whether its host key was checked,
by the host keys its session trusts and by whether it offered
compression (`PoolKey`, §Compression): a session under
`strict` or `accept-new` is never handed a connection that a session
under `off` dialed, and a session is never handed one whose key only
another session's set trusts (§Host key verification). Sessions trusting
the same keys — none, usually — share as before. A dial whose session
accepted a changed key is pooled under the set it ended with, key
included.

- consumers hold `Arc`s; a monitor task disconnects and evicts the
  connection once only the pool's own reference is left (checked every
  250 ms);
- a lookup that finds a closed connection (`SshConnection::is_closed`:
  a server disconnect, or a dead link caught by §Keepalives) evicts it and
  dials fresh, even while other consumers still hold it;
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

## Keepalives

Every dial, direct or through a jump host, runs russh keepalives from the
hop's `ssh_options` (`Keepalive::from_options`): after
`keepalive_interval` seconds with nothing from the server the client sends
a probe, and once more than `keepalive_count` probes go unanswered the
session closes. Unset fields default to 15 s and 3, so a link that dies
silently (Wi-Fi gone: no FIN, no RST) is caught within about a minute. An
interval of 0 turns probes off; a count of 0 probes without ever closing.

The closed session ends every channel without an exit status, so each
tab's reader reports `ConnectionLost` and the reconnect policy applies
(doc 03 §Reconnect). A shared connection runs the keepalives of the
session that dialed it.

## Compression

`ssh_options.compression` (the session dialog's **Compress traffic**,
doc 09; `Compression yes` in an ssh_config import, doc 10) decides what
the dial offers (`client_config`). On, it offers `zlib@openssh.com` then
`none`, as OpenSSH's `Compression yes` does; off, only `none`. The
server picks the client's first method it supports, and `none` is
mandatory (RFC 4253), so a server without compression connects
uncompressed and nothing changes on the server. `zlib@openssh.com` starts
only after authentication; plain pre-auth `zlib` is never offered, and
OpenSSH's sshd has refused it since 7.4.

The offer is fixed for a connection's life, so it is part of the pool
key: a compressing session and one that does not never share a
connection, and a changed setting applies to the next dial, while open
tabs keep theirs. One connection carries the terminal, the SFTP sidebar
and the forwards, so they compress together. Each hop of a jumped
connection uses its own session's setting; the jump hop carries the
target's already encrypted stream, which does not compress, so
compression belongs on the target.

### Compression savings

Each dial counts into a `Traffic` (`traffic.rs`), which
`SshConnection::traffic` hands out: `CountingStream` counts the bytes read
off the socket (or, for a jumped target, off its jump channel), and the
handler adds what each incoming channel data message would have cost
uncompressed — its bytes plus 38 bytes of framing (message fields,
packet length, typical padding and a 16-byte AEAD tag; 4 more for
stderr). Counting starts once authentication succeeds. The saving is
`1 − wire ÷ estimate`, never below 0, so a server that declined
compression reads about 0%. It is `None` for a session that did not
offer compression and until 32 KiB have been measured, when the figure is
still noise.

The status bar shows it for the current tab (doc 03 §Status bar). It
covers the whole connection, so the SFTP sidebar's transfers and the
forwards count into the terminal tab's figure.

## Server figures

While an SSH tab is the one shown and connected (doc 03 §Status bar),
`host_stats::stream_server` runs one long-lived script on one exec
channel of the shared connection: `sh`, with the script on stdin so the
login shell never parses it (the SFTP sidebar's shell probe runs the same
way, doc 07 §Following the shell). A local shell tab runs the same script
under this machine's `sh` (doc 06 §Local shell), and both read its output
through one `host_stats::Parser`. Every line is marked (`wayterm-uptime:`,
`wayterm-load:`, `wayterm-net:`, `wayterm-df:` …), so startup files that
print do not matter, and each is written whole by one `printf`, so the
background `df` jobs' lines never break into another's.

- **Uptime**, first and once: `/proc/uptime` on Linux, else
  `sysctl -n kern.boottime` (macOS and the BSDs) against `date +%s`. The
  page keeps the answer with the instant it arrived, and the bar adds the
  time since. It is read each time the tab is shown, kept while the tab is
  hidden, and cleared on each connect, so a new connection never shows
  the last one's. Without `/proc` the script ends here.
- **Load**, every 2 s: `wayterm-load:<cores> <load averages> <memory>
  <cpu times>`: the `cpuN` line count from `/proc/stat` (counted once),
  the three `/proc/loadavg` figures, `MemTotal`, `MemAvailable`,
  `SwapTotal` and `SwapFree` from `/proc/meminfo`, and the aggregate
  `cpu` line's times. Kernels before 3.14 have no `MemAvailable`; free
  plus buffers plus cache stands in. The tab turns consecutive CPU times
  into a busy share (`CpuMeter`): idle and iowait count as idle, and the
  guest fields are already in user and nice. Used memory is what is not
  available.
- **Bandwidth**, every 2 s after the load: the script finds the default
  route's interface once, in `/proc/net/route` (the `00000000`
  destination), or when there is no IPv4 default, in
  `/proc/net/ipv6_route` (`::/0`, skipping the kernel's unreachable
  defaults on `lo`), then prints `wayterm-net:<interface> <rx bytes> <tx
  bytes>` from `/proc/net/dev`. The counters cover both IP versions. The
  tab turns consecutive samples into bytes per second (`RateMeter`),
  timed by when each sample arrives, and the status bar shows them as
  bits per second in the unit that keeps the figure under 1000
  (`800 bit/s`, `16.4 kbit/s`, `250 Mbit/s`). A counter that went
  backwards (the interface was reset) or a changed interface skips one
  figure. The figure counts everything on the server's interface, not
  just this connection, as MobaXterm's monitor does. Without a default
  route there is no bandwidth line, and the other figures carry on.
- **Disks**, every 30 s: the script prints the mount table
  (`/proc/mounts`) and starts two background jobs, each running
  `df -P -k` on its group's mount points: one for the network
  filesystems (NFS, SMB/CIFS, 9p, Ceph, Gluster, Lustre and the like) and
  the FUSE ones, which hang the same way when their daemon stops
  (`fuseblk` is a local block device), the other for everything else.
  The type lists live in `disks.rs` and are written into the script.
  Mount points the table escapes (`\040` for a space) are unescaped for
  `df`, and skipped types are never given to it, so an autofs mount point
  is not mounted by asking. A job still running when the next round is
  due, a `df` stuck on a hung mount, which no signal ends, is not started
  again: the round prints `wayterm-disks-late:<group>` instead. So stuck
  `df`s never pile up, and the load, the bandwidth and the other group
  keep updating. `disks::Collector` keeps each group's last finished
  reading and joins them with the latest mount table by mount point; a
  late group's figures stay up, and the disk tooltip says which group is
  not up to date and since when (doc 03 §Status bar). Only real storage
  counts: tmpfs, devtmpfs, ramfs, overlays, squashfs and erofs (snap and
  image mounts, always full), ISO and UDF media, autofs and the desktop's
  FUSE bridges are skipped, and so is a zero-sized filesystem. A mount
  point mounted over keeps only its last mount, and a device mounted at
  several points (bind mounts, btrfs subvolumes) counts once, at its
  shortest mount point that `df` listed. Use % is `df`'s: used over used
  plus available, rounded up. Every dataset of a ZFS pool reports the
  pool's free space as its own, so a pool's datasets (the `zfs` mounts
  whose device starts with the pool's name) are reckoned together: their
  used space summed and the pool's free space counted once, the largest
  any dataset reports, since a quota only lowers it. Each dataset is as
  full as its pool, and the total counts the pool once.

The stream holds the connection only while its channel opens, so it
never keeps a released connection alive. Switching tabs, a drop or a
disconnect drops the receiver; the stream then closes its channel, and
the server's script ends on its next write. A jumped tab samples the
target.

## Host key verification

### Policy

1. Use `~/.ssh/known_hosts`, one store shared with every other SSH client,
   and beside it the session's own set of trusted keys
   (`sessions.host_keys`, doc 09), which only a changed-key acceptance
   adds to.
2. **Unknown host** — neither names it: an OpenSSH-style prompt with the
   SHA-256 fingerprint. Accepting always records the key in known_hosts,
   so the host is never asked about twice. Hashed entries are read, but
   new entries are appended in plain text (`host` or `[host]:port`, then
   the key), even to a hashed file.
3. **Changed key:** known_hosts names the host, or the session trusts keys
   of its own, and none of those is the presented key, whatever the types
   of the keys involved — the man-in-the-middle signal. A warning
   (`dialogs::hostkey::prompt_changed`) shows the key type, fingerprint and
   host:port and asks **Deny** (the default) or **Accept and add to this
   session**. Accepting connects and adds the key to this session's set
   only: known_hosts is never written, so other sessions and other SSH
   clients still see the change, and `ssh-keygen -R` stays the remedy for a
   stale global entry. A key under an `@revoked` entry fails outright,
   whatever the session trusts.
4. The policy is global (`host_key_policy`, seeded `strict`) with a
   per-session override:
   - `strict`: unknown → prompt; changed → prompt (deny, or trust it for
     the session);
   - `accept-new`: unknown → record silently (OpenSSH's
     `StrictHostKeyChecking=accept-new`); changed → prompt, as `strict`;
   - `off`: no verification, for labs only. It skips the question about
     an unknown host and whatever keeps known_hosts from answering; a
     changed or revoked key still fails, without a prompt.

### Implementation

`AppClientHandler::check_server_key` runs during key exchange, before
authentication, and asks `HostKeyVerifier`, which reads the host's entries
itself (`hostkey/known_hosts.rs`) and holds the session's set
(`SessionKeySet`):

| known_hosts and the session's set hold, for the host | Decision |
|---|---|
| the presented key under `@revoked` (known_hosts) | reject, no prompt, every policy |
| the presented key, in either | accept |
| other keys only, in either | changed: `strict`/`accept-new` ask the changed-key question, `off` rejects |
| nothing | apply the policy (unknown host) |

The verifier's prompt (`HostKeyPrompt`) is told which question it is
asked (`HostKeyQuestion::Unknown` or `Changed`), and each has its own
accepting answer: `AcceptAndSave` records known_hosts, and only answers
an unknown host; `AcceptAndSaveSession` adds the key to the session's set,
and only answers a changed key. Any other answer rejects, so a changed key
can never reach known_hosts.

**The session's set.** The SSH connect flow (`src/ui/tabs/ssh.rs`) seeds
each hop's set on the main thread from that hop's row, read fresh from the
store — the tab's copy of the row only when there is no store — so a
redial after an acceptance is not asked again. The set is a shared handle
(`Arc<Mutex<…>>`): the verifier adds to it during key exchange, and once
the dial returns the flow writes what was added back to the row
(`SessionStore::add_session_host_keys`), before anything else: a key
accepted stays accepted even when authentication then fails or the attempt
was cancelled or superseded. The prompt never touches the store, which is
main-thread only. On a jumped connection each hop has its own row's set,
so a key accepted on the jump hop is saved to the jump session and one on
the target to the target.

russh's own `check_known_hosts_path` is not used: it compares the
presented key only with recorded keys of its type, so a known host showing
a key of another type reads as unknown — prompted for under `strict`,
recorded silently under `accept-new` — and whoever sits in the middle picks
the type.

- **Reading** follows OpenSSH. Fields split on spaces and tabs; a host is
  named `host`, or `[host]:port` off port 22, compared without case; the
  host field is a comma-separated list of names, hashed names (`|1|…`),
  `*` and `?` patterns and `!` negations, which exclude. `@revoked` entries
  are honored, `@cert-authority` ones skipped (§Limitations). A missing
  file holds no entries; bytes that are not UTF-8 hide only their own
  line.
- **Key types.** Each dial offers the host-key algorithms of the host's
  recorded keys first — in known_hosts or the session's set
  (`HostKeyVerifier::key_preference`, as OpenSSH orders
  `HostKeyAlgorithms`) — so a server that still holds a recorded key
  presents that one. Without this a host recorded by another client under
  one type would be reported as changed for presenting another.
- **Recording.** A new known_hosts entry (an unknown host only) goes in as
  `host` or `[host]:port` and the key, the name as the session spells it.
  A name that would read in the file as anything but one plain name is
  never recorded and so never accepted, whatever the session trusts:
  blanks, control characters and any of `,*?!|#@[]`, which would make it a
  second line, a list or a pattern vouching for other hosts. A missing
  `~/.ssh` is created with mode 0700. The session's set records the key
  type and the base64 key without padding.
- **Failing closed.** A known_hosts that cannot be read (anything but a
  missing file), no home directory to find it in, a key that cannot be
  encoded, and a key that should be recorded and cannot be all reject,
  even a key the session trusts: none of them makes a host unknown, and
  an unreadable file could revoke the key.

Every rejection is logged with its reason — a changed key not accepted, a
revoked one, or one of the failures above — and never with key material.

Host certificates are rejected. A rejection surfaces as
`TransportError::HostKeyRejected`, never as an auth failure.

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

The challenge dialog (`src/ui/dialogs/auth.rs`) is one UI for password
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
  credentials, host-key policy and trusted keys; host keys are verified on
  both hops, and a changed key accepted on either is saved to that hop's
  session.
- Depth > 1 and self-jump are connect errors
  (`transport::jump::check_jump_chain`), also refused in the session
  dialog.
- The jump hop comes from the supervisor like any connection. The target
  connection is never pooled — the pool key describes a TCP endpoint, not
  a connection backed by a jump channel — and the tab holds only the
  target. The target holds the jump hop's `Arc` for its whole life, so the
  supervisor's monitor releases the bastion only after the last target
  over it is dropped.
- No SFTP task runs over a jumped connection; the sidebar says so instead
  of attaching.

**`ssh -W` fallback.** With `ssh_options.ssh_w_fallback` set, a target
whose direct-tcpip channel the jump hop refuses is reached through
`ssh -W target:port -o BatchMode=yes -- user@jump` as the stream (OpenSSH
never prompts on a tty Wayterm does not own; after `--` a stored user name
is never read as an option). The child carries no secrets, `BW_SESSION`
included (doc 05), and is killed when the connection drops. The stream does not use the jump
hop's connection, so the target does not hold it. Trust note: on that hop
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
- Listeners close when their tab closes or lets go of its connection
  (Disconnect, a lost connection, the shell's exit, a transport that fails
  to connect: doc 03 §Reconnect). Every (re)connect stops the old engine
  and starts a fresh one on the new connection, so forwards come back
  after a reconnect.

### Tunnels dialog

The header bar's **Tunnels…** button opens the current session tab's
Tunnels dialog (`src/ui/dialogs/tunnels.rs`): one row per rule with its
kind, bind, remote, Enabled toggle and live status. While the tab is
connected, toggling Enabled applies to the engine at once; offline, the
dialog still opens for editing and every status reads "no active
connection". Add rule and Remove edit the list; OK validates (ports
1–65535, required fields) and saves the rules to the session row; a help
popover explains each control. The session dialog's SSH group embeds the
same rule editor, so both accept exactly the same input.

## Version anchors

- **russh 0.64:** keyboard-interactive is client-driven
  (`authenticate_keyboard_interactive_start`/`_respond`, responses
  `Success | Failure | InfoRequest { prompts }`); agent auth via
  `AgentClient::connect_env` → `request_identities` →
  `authenticate_publickey_with`.
- **russh 0.65:** `known_hosts::learn_known_hosts_path` appends an entry
  (lookups are Wayterm's own, §Host key verification), and
  `Preferred::DEFAULT.key` is the host-key algorithm list that
  `key_preference` reorders. `Preferred::compression` is the offer, from
  `russh::compression::{ZLIB_LEGACY, NONE}`; the `flate2` default
  feature provides zlib, and `zlib@openssh.com` waits for authentication
  (`Compression::is_deferred`).
- **russh-sftp 3.0:** doc 07.

Re-verify these when bumping a dependency; these APIs have moved before.

## Limitations

- A session's trusted keys can only be added to, at the changed-key
  prompt (or by a JSON import, doc 10): there is no UI to list or remove
  them, and known_hosts entries are never copied into them. The set
  belongs to the session, not to a host: editing the session's host or
  port keeps it, and a key the new host does not present makes it read as
  changed rather than unknown.
- A denied changed key shows on the tab as a plain host-key rejection.
- Host certificates are rejected; there is no CA support, so
  `@cert-authority` lines in known_hosts are skipped.
- A host that replaced its key with one of another type is reported as
  changed, as in OpenSSH: each session that connects to it asks once and
  then trusts the new key, while known_hosts keeps the stale entry until
  it is removed (`ssh-keygen -R`). Entries for a host's IP address are not
  consulted unless the session names the address, and new entries are
  never hashed.
- The `off` policy has no badge in the UI.
- russh reports neither the negotiated compression nor its byte counts,
  so the saving is an estimate (§Compression savings): framing is
  modelled, not read, so a MAC longer than 16 bytes (`hmac-*-etm`) reads
  low. Incoming traffic only, since russh has no hook for outgoing channel
  data, so uploads are not measured; incoming packets other than channel
  data (keepalive replies, window adjusts, rekeys) count against the
  saving.
- The session dialog edits only `ssh_options.compression`, so keepalives
  can only be set by an import and `ssh_w_fallback` only by a JSON import
  (doc 10).
- A redial or new tab that starts before keepalives catch a dead link is
  handed that connection and waits on it; once keepalives close it, the
  attempt fails and the next one dials fresh.
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
- Server figures need a POSIX `sh`: a Windows OpenSSH server, or a
  restricted shell that refuses exec channels, shows none. The uptime
  also comes from `sysctl kern.boottime` (macOS, the BSDs); only the Linux
  path has run against a real server, and the `sysctl` path is tested
  against a stand-in `sysctl`. Everything else reads `/proc` and needs
  `grep`, `sed` and `df`, so only Linux servers show it. Showing a tab
  again starts a new script, so its uptime returns after a round trip and
  its CPU and bandwidth figures after one interval.
- When the server has default routes on several interfaces, the first
  listed is measured, IPv4 before IPv6, which may not be the one this
  connection uses. The rate includes this connection's own traffic, the
  sampling's few bytes every 2 s among it.
- A ZFS pool's used space counts only its mounted datasets: snapshots,
  unmounted datasets and reservations are left out, so the pool reads
  emptier than `zpool list` says. When every dataset has a quota, the
  pool's free space reads as the largest quota's room. LVM thin pools and
  other overcommitted storage are not recognised.
- Filesystems are grouped by type: a network filesystem of a type not in
  the list counts as local, so a hung one stalls the local group too, and
  one hung mount stalls its whole group. A `df` stuck on a hung mount
  stays on the host until the mount answers, one per group each time the
  tab is shown while it hangs. A group never read since the tab was shown
  shows no filesystems until its `df` finishes.
- The shown SSH tab holds one exec channel for its figures, on top of its
  shell and SFTP, which counts against the server's `MaxSessions` (10 by
  default).
