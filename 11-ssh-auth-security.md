# SSH Authentication & Security

## Problem

Doc 04 sketched the transport plumbing but left the security-critical paths
undefined. This doc specifies them concretely: host key verification, the full
authentication state machine (including keyboard-interactive), credential
handling, and how tunnels / jump hosts fit the architecture. Phase 2 is not
done until the items marked **[Phase 2]** here are implemented — a connection
manager that auto-accepts host keys is a MITM hole and will be treated as a
release blocker.

Verified against russh **0.63.x** and FreeRDP **3.x** (see "Version anchors"
at the end).

## Host Key Verification **[Phase 2]**

### Policy

1. **Reuse `~/.ssh/known_hosts`** — one store, standard semantics, and
   interop with every other SSH client the user runs. No separate
   per-app known-hosts DB in v1.
2. **First connection (unknown host):** show the OpenSSH-style prompt with
   the server key fingerprint (SHA-256, Base64, like `ssh-keygen -lf`).
   Accept-and-record via `learn_known_hosts_path`, which writes entries in
   the user's existing format (hashed `|1|…` if their file uses hashed
   hosts).
3. **Changed key:** hard fail with a scary dialog — this is the MITM
   signal. Offer "view fingerprint" and instructions for removing the old
   entry, but never a one-click override.
4. **Global setting `host_key_policy`** (also per-session override):
   - `strict` (default) — unknown → prompt; changed → fail.
   - `accept-new` — unknown → auto-accept and record (like OpenSSH's
     `StrictHostKeyChecking=accept-new`); changed → fail.
   - `off` — no verification. Exposed only for lab/testing; any session
     using it shows a persistent warning badge in its tab.

### Implementation

russh delegates verification to the client `Handler`; implement
`check_server_key(&mut self, server_public_key: &PublicKeyOrCertificate)`.
Two functions from the russh `keys::known_hosts` module do the work:

- `check_known_hosts_path(host, port, key, path)` — `Ok(true)` = known and
  matches; `Ok(false)` = known and MISMATCHED; `Err(_)` = unknown host.
- `learn_known_hosts_path(host, port, key, path)` — appends the key.

```rust
pub enum HostKeyDecision { Accept, AcceptAndSave, Reject }

pub struct HostKeyFacts {
    pub host: String,
    pub port: u16,
    pub key_type: String,             // "ssh-ed25519", "ssh-rsa", …
    pub fingerprint_sha256: String,  // "SHA256:AnOThErFp..."
}

pub struct HostKeyVerifier {
    policy: HostKeyPolicy,            // strict | accept-new | off
    prompt: Arc<dyn Fn(&HostKeyFacts) -> HostKeyDecision + Send + Sync>,
}
```

`HostKeyVerifier::verify()` maps `check_known_hosts_path` results to
decisions: match → accept; mismatch → fail (no prompt); unknown → apply
policy (prompt under `strict`, auto-save under `accept-new`, auto-accept
under `off`). The prompt closure bridges to the GTK dialog via a oneshot
channel — `check_server_key` is async, so the transport task simply
suspends until the dialog answers. Do NOT block the reactor.

**Watch out:** `check_server_key` fires during key exchange, before
authentication. Surface its failures as `TransportError::HostKeyRejected`,
not as auth failures. If the server presents an SSH *certificate*
(`PublicKeyOrCertificate::Certificate` variant), v1 rejects it with a clear
message ("certificate-based host auth not supported yet") rather than
silently accepting.

## Authentication State Machine **[Phase 2]**

### Auth methods

`AuthMethod` (docs 04/06) gains a variant:

```rust
pub enum AuthMethod {
    Password(Zeroizing<Vec<u8>>),
    Key { path: String, passphrase: Option<Zeroizing<Vec<u8>>> },
    Agent,
    KeyboardInteractive,
    None,
}
```

`keyboard-interactive` is NOT optional — enterprises, 2FA/OTP vendors, and
plenty of bastion setups only offer it. MobaXterm and PuTTY both support
it. russh routes the server's prompts through `client::Handler`
callbacks (`KeyboardInteractiveAuthResponse`; see the
`client_exec_interactive.rs` example in the russh repo for wiring). Design
the CredentialManager prompt as a generic **challenge-response dialog**
(array of {prompt, echo-on/off} pairs) rather than a password box — the
same UI serves password fallback, key passphrase prompts, and
keyboard-interactive rounds.

### Flow

```rust
// Conceptual order; per-session preference, default:
// agent → key → password → keyboard-interactive
async fn authenticate(session: &mut Handle, creds: &CredentialSet) -> Result<AuthOutcome> {
    // 1. Agent (if configured). Failure is NOT fatal — fall through.
    // 2. Private key (if configured). Passphrase prompt via challenge dialog.
    // 3. Password (if available). Failure is NOT fatal — server may
    //    offer keyboard-interactive next.
    // 4. Keyboard-interactive: server sends prompts → render → user answers
    //    → possibly multiple rounds.
    // → TransportState::Connected, or AuthFailed(reason, methods_remaining)
}
```

Rules:

- Wrong password = retry, not an error screen. Distinct UI for "host
  unreachable" vs "auth failed" vs "no remaining auth methods".
- Zeroize after use (`Zeroizing` everywhere). Never log secrets. Never
  store secret material in SQLite (doc 06 governs).
- **Key hash algorithms:** the doc-04 sketch hardcodes `"ssh-ed25519"` in
  `PrivateKeyWithHashAlg::new(key, "ssh-ed25519")` — wrong for RSA keys.
  Derive the hash algorithm per key type instead of hardcoding.
- **Agent path:** doc 04 referenced `client::AgentClient::connect_env()`
  and `authenticate_future` — re-verify against 0.63 before building;
  the agent API surface has moved between russh minors before.

### tmux persistence

`tmux new -A -s <name>` runs over the same channel as the shell; no
client-side secret handling changes. Note tmux scrollback persists on the
remote host after disconnect — that's remote-side data at rest, out of
scope client-side in v1.

## CredentialManager **[Phase 2]**

Responsibilities (expands the doc 04 sketch; keyring scheme per doc 06):

1. **Lookup order:** keyring → prompt → store-if-checked.
2. Secret types: password, key passphrase, VNC password, RDP credentials.
3. The prompt runs on the GTK main thread (`MainContext::invoke` + oneshot
   back to the transport task). The dialog is cancellable; cancellation
   aborts the connect.
4. libsecret attributes (exact set in doc 06): `session_name`, `host`,
   `username`, plus `secret_type` ∈ {password, key_passphrase,
   vnc_password, rdp_password}.
5. On session delete, offer to remove the associated keyring entries.
6. Session **export/import never includes secrets** (doc 06 already
   states this — keep it that way).

## RDP Credentials

The doc-08 flow (`/p:` on the command line) leaks passwords via `/proc/<pid>/cmdline` to every local user. Use FreeRDP 3's stdin credential path instead:

```
xfreerdp /v:host /u:user /from-stdin:force ...
   → reads username/password (one per line) from stdin
```

- `/from-stdin[:force]` is documented in current xfreerdp manpages
  (FreeRDP 3.x; force re-prompts even when other credentials were given).
- **[Phase 2 spike, half a day]** verify on target distros: flag presence,
  exact line format, interaction with `/u` given on argv. If a target
  distro ships FreeRDP 2.x without `/from-stdin`, fall back to libsecret's
  `secret-tool`-style temp file? NO — temp files are worse. Fallback for
  FreeRDP 2.x: external window mode with xfreerdp's own GUI prompt
  (no embedded credentials at all), and a settings note telling the user
  to upgrade. Never write credentials to argv or disk.
- Long term, the libfreerdp C API (doc 08 "Subprocess vs Library") sets
  the password in-process and eliminates the issue entirely.

## Jump Hosts and Port Forwarding

Both are core MobaXterm use cases that no doc covered. Design now, build
in Phase 11 — the shapes below exist so the connection-layer refactor
(shared `SshConnection`, doc 04) doesn't have to be redone.

### Connection layer shape

```rust
/// One authenticated russh session, shareable by multiple consumers.
/// The transport, SFTP sidebar (doc 09), and future tunnels all open
/// channels on this. Created by the connection supervisor, NOT owned
/// by SSHTransport.
pub struct SshConnection {
    handle: russh::client::Handle<AppClientHandler>,
    host: String, port: u16, username: String,
}
impl SshConnection {
    async fn open_session_channel(&self) -> Result<Channel<Msg>>;
    async fn open_direct_tcpip(&self, host: &str, port: u16) -> Result<Channel<Msg>>;
    // direct-tcpip channels power: local forwards, jump-host hops, SFTP-over-hop
}
```

`SSHTransport` becomes: *a channel-open + PTY + shell/exec wrapper over an
`Arc<SshConnection>`* — it no longer owns the session. The SFTP sidebar
takes `Arc<SshConnection>` and opens its own channel. This resolves the
doc-04/doc-09 ownership seam.

### Jump hosts (ProxyJump semantics)

v1 target: one level of `ProxyJump`-style hopping (the common case; the
general graph is a non-goal).

- Host A (jump): plain `SshConnection` + PTY-less `direct-tcpip` channel
  to host B:22.
- Host B (target): the real session is opened *through* A's channel
  (`russh::client::connect_stream` style — the same approach
  `russh-config` uses for ProxyCommand; that crate is the reference).
- UI: session config gets `jump_host` (reference to another session).
  Depth > 1 → error message, not silent weirdness.
- Auth on each hop is independent (own CredentialManager lookups).
- Fallback for exotic hops: shell out to `ssh -W` like everyone else —
  run the system ssh as a `Stream` (again see russh-config), which also
  gives us ProxyCommand support nearly free.

### Port forwarding

Direct (local) forwards, driven by `open_direct_tcpip`:

```rust
// One tokio task per local listener; N tasks for its connections.
// local_bind: 127.0.0.1:8080 → remote web1.lan:80
pub struct LocalForward { local: SocketAddr, remote_host: String, remote_port: u16 }
```

- Local listener on a tokio task; each accepted connection opens one
  `direct-tcpip` channel on the associated `SshConnection` and pipes
  bytes both directions (this is exactly what a `Channel` is — an
  async byte stream — so the piping is `tokio::io::copy_bidirectional`).
- Dynamic (SOCKS) forwards: v2. (Needs a SOCKS server impl; scope it
  then.)
- Remote forwards (`tcpip-forward` global request): v2 — requires
  handler-side `channel_open_forwarded_tcpip` callbacks, which couples
  into `AppClientHandler`; design when built.
- UI/DB: forward rules live on the session (`forward_rules` JSON column,
  doc 06) with type, bind spec, and enabled flag; a "Tunnels" sub-dialog
  lists them with status (listening / error / closed).
- Failure semantics: if the connection drops, listeners close and are
  re-established on reconnect (tmux toggle doesn't affect forwards —
  they're transport-level, not shell-level).

### Agent and X11 forwarding

- **Agent forwarding:** NOT in v1. Needs `ssh-agent` protocol serving over
  a remote unix socket — real work, real risk (see Limitations). Users on
  jump hosts will ask; the honest v1 answer is "configure your agent on
  the jump host itself".
- **X11 forwarding:** out of scope v1; low demand for a GTK4/Linux-native
  audience (Wayland makes it messier). Revisit after jumps work.

## Testing Strategy **[Phase 2 CI]**

The transport layer is where correctness lives, and it's fully testable
without a GUI because of the trait split:

1. **Integration (real sshd):** two provisioning paths, both real sshd
   instances. **CI** (`.github/workflows/ci.yml`, "Provision test sshd")
   configures the **runner's system sshd on port 22** — generated host
   keys, a throwaway `wayterm-test` user, ephemeral `SSH_TEST_*` creds in
   `GITHUB_ENV`. **Local** (`scripts/run-sshd-tests.sh`) starts a
   **separate throwaway instance on port 2422** from
   `scripts/sshd-test-config.inc`, so the system sshd and
   `/etc/ssh/sshd_config` are never modified or restarted. Either way the
   tests connect, auth (password / key / keyboard-interactive), exec,
   resize, collect output. This is the russh repo's own approach for its
   client tests.
2. **Host-key tests:** unknown host (TOFU prompt path), matching key,
   mismatched key (must fail), `accept-new` behavior, hashed-entries
   file. Assertions on the verifier's decision table — pure unit tests
   given a temp known_hosts file.
3. **Auth state machine:** mock `Handle` trait? russh is concrete —
   instead run against a real sshd with `AuthenticationMethods` variants
   (password-only, keyboard-interactive-only via PAM, key-only) and
   assert method fallback order and retry counts.

   **Server-gated locally since 2026-10 (verified against OpenSSH
   10.0p2).** `tests/auth_matrix.rs` runs the matrix;
   `scripts/auth-matrix-sshd.sh` provisions one throwaway sshd PER MODE,
   each on its own port, rendered from a shared base config
   (`scripts/sshd-auth-matrix-config.inc`) plus one per-mode auth
   fragment (`scripts/auth-matrix/<mode>.inc`). Exact combos:

   | mode | `AuthenticationMethods` | per-method toggles |
   |---|---|---|
   | password-only | `password` | `PasswordAuthentication yes`, `PubkeyAuthentication yes`, `KbdInteractiveAuthentication no` |
   | kbd-interactive-only | `keyboard-interactive:pam` | `KbdInteractiveAuthentication yes` (needs `UsePAM yes`), `PasswordAuthentication no`, `PubkeyAuthentication no` |
   | key-only | `publickey` | `PubkeyAuthentication yes`, `PasswordAuthentication no`, `KbdInteractiveAuthentication no` |

   `PubkeyAuthentication yes` in the password-only mode is deliberate: the
   client has to be able to OFFER its key and have the server refuse it,
   because that rejection is what forces the fall-through to password. A
   variant with pubkey switched off would pass for the wrong reason.

   Assertions are on `AuthOutcome` / `methods_remaining`, which is exactly
   the list of methods the chain never ATTEMPTED — so a method's absence
   from it proves "attempted and rejected" without adding a hook to the
   chain. Per mode that means: the rejected methods produce a `Failed`
   with the expected never-attempted list; the full chain reaches
   `Success` (key rejected → password succeeds; password rejected →
   keyboard-interactive/pam succeeds; key succeeds first try); a wrong
   password triggers EXACTLY ONE re-prompt (doc 11 §Flow retry rule) and
   then a clean `Failed` with an empty never-attempted list; and a
   nonexistent key path is likewise a clean `Failed`, with the chain
   continuing on to the remaining methods rather than aborting.

   **CI still runs only the all-methods sshd.** CI provisions one system
   sshd on port 22 and sets no `AuthenticationMethods`, so it cannot
   express this matrix; `tests/auth_matrix.rs` skips itself there
   (`SSH_MATRIX_*` unset), exactly as it does under a bare `cargo test`.
   The matrix is a local / pre-merge gate:
   `scripts/auth-matrix-sshd.sh`.

   **sshd-refusal findings: none on the verified host** (OpenSSH
   10.0p2) — all three `AuthenticationMethods` combos passed `sshd -t` and
   each mode's tests went green. The runner still treats an `sshd -t`
   refusal as a FINDING and moves on to the next mode instead of forcing
   the combo, because `keyboard-interactive:pam` legitimately cannot be
   served on a host without `/etc/pam.d/sshd` and `pam_unix.so`.
4. **UI-less loop:** TerminalBackend gets a `FakeBackend` (in-memory
   grid, records writes) — already implied by doc 02's trait; make it a
   test fixture from day one.
5. **SFTP:** russh-sftp ships a test server; exercise list/download/
   upload against it in CI (Phase 3 gate).
6. **Fuzzing (later, Phase 5+):** transport parsers (see doc 01's
   "untrusted input" rationale) — VTE handles the terminal side, but
   our known_hosts parser and SFTP path joiner get `cargo-fuzz` targets.

CI: GitHub Actions, `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test` with the sshd service container. Windows/macOS: N/A (Linux
app). Nightly job: `cargo audit` for advisory sweep.

## Limitations (explicit, v1)

- Host-key SSH **certificates** (not raw keys) are rejected with a clear
  message. Proper CA support is a v2 item.
- `off` host-key policy exists but is loud (warning badge) on purpose.
- Agent forwarding, X11 forwarding, remote forwards, dynamic SOCKS
  forwards: not in v1 (see above for where each lands).
- Jump hops: exactly 0 or 1 level.

## Version anchors

- russh 0.63.1 — `keys::known_hosts::{check,learn}_known_hosts_path`,
  `client::Handler::check_server_key(&PublicKeyOrCertificate)`,
  `KeyboardInteractiveAuthResponse` (client module), keys re-exported
  from `russh::keys` (was `russh_keys` pre-0.4x).
  **SUPERSEDED (2026-10, verified against 0.64.1 at Phase 2 implementation,
  commit ae73f23):** the three-state semantics CHANGED in 0.64 —
  `check_known_hosts_path` now returns `Ok(false)` for an UNKNOWN host
  (no entry), and a known-but-mismatched key as
  `Err(KeysError::KeyChanged { line })`. The 0.63-era mapping in this
  doc's §Host Key Verification ("Ok(false) = known and MISMATCHED")
  is WRONG for 0.64+. The implemented mapping: Ok(true)=match→accept;
  Ok(false)=unknown→apply policy; Err(KeyChanged)=mismatch→hard fail,
  no prompt. Other read errors (missing file etc.) are treated as
  unknown → policy applies (prompt under strict; never silent accept).
  **ALSO CORRECTED (commit afd122e):** keyboard-interactive in 0.64 is
  driven from the client side — `Handle::authenticate_keyboard_interactive_start()`
  + `authenticate_keyboard_interactive_respond()`, with
  `KeyboardInteractiveAuthResponse::{Success, Failure{remaining_methods,
  partial_success}, InfoRequest{name, instructions, prompts: Vec<Prompt>}}`
  and `Prompt{prompt: String, echo: bool}`. It is NOT a client Handler
  callback (that's the server side). Agent auth:
  `AgentClient::connect_env()` → `request_identities()` →
  `authenticate_publickey_with()`.
- russh-sftp 2.4.0 — `client::SftpSession` (high-level, `std::fs`-like).
- FreeRDP 3.x — `/from-stdin[:force]` in xfreerdp (verify per distro).
- VTE ≥ 0.62 — SIXEL support exists but is opt-in
  (`vte_terminal_set_enable_sixel`); see docs 02/03.

Re-verify all anchors at Phase 1 kickoff; crate APIs move.