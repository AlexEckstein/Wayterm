# Credential Sources: OS keyring (oo7) + Bitwarden CLI

**Date:** 2026-10-12
**Status:** dispatching
**Base:** origin/main @ af8f2c2
**Outcome:** wire the keyring stub with a real Secret Service backend and add
a Bitwarden CLI source, so the chain becomes keyring → bitwarden → prompt;
KeePassXC works through the same Secret Service backend (it *is* an
`org.freedesktop.secrets` provider when its Secret Service integration is
enabled).

## Background

`CredentialManager` (doc 11) walks sources first-hit-wins; `KeyringSource` is
a stub (always misses), so every connect prompts. The comparison against
RustConn (GPL-3.0, ideas only) and MobaRust (Apache-2.0) settled the design:
oo7 (MIT) for the keyring, env-var CLI invocation for Bitwarden, secrets stay
`Zeroizing` and native.

## Verified design facts (transcribed from origin/main @ af8f2c2)

- Chain construction: `src/ui/tab.rs` `credential_manager(session_name)` at
  ~line 818 — `CredentialManager::new(vec![Box::new(KeyringSource),
  Box::new(prompt)])`. Also consumed by `ui/rdp_tab.rs:250` and
  `ui/vnc_tab.rs:432 dial()`.
- **`auth_config` (`src/transport/connection.rs:150-191`) uses a hardcoded
  `SESSION_ID = "_live_connect"` for `creds.lookup(SESSION_ID, …)` — the
  transport layer never receives the session's row id.** Keyring entries keyed
  on that constant would all collide. The lane must thread the real session id
  from the dial path (or a stable per-session id) through to `lookup`, and
  `CredentialManager` must pass the session's display name on lookup for
  keyring labels. This changes the lookup call-shape inside
  `transport::connection` and `ui::tab` — a transport-internal refactor, not a
  trait-seam change (the `CredentialSource` trait stays unchanged).
- `CredentialSource` trait (src/transport/credentials.rs): sync,
  `lookup(&self, session_id, secret_type) -> Option<Zeroizing<Vec<u8>>>`,
  `store(...)` with `CredentialError::{Unavailable, StoreError}`;
  `CredentialManager::store` walks sources, Unavailable falls through.
- Store API for flags: `SessionStore::get_setting/set_setting` (src/store/db.rs
  ~206-230) + `flag_enabled` (db.rs ~300) on the seeded `settings` table.
- Session delete: `SessionStore::delete_session(id)` (db.rs:143), called from
  `ui/session_tree.rs:441`.
- `run_challenges(name, instructions, prompts)` (ui/auth_dialog.rs:87) — the
  existing challenge dialog; `credential_manager`'s prompt closure hops to
  the GTK thread via `auth_dialog::run_on_gtk_main_thread`.
- Settings UI: `ui/settings.rs` pages() adds `general::page(store)` +
  `hotkeys::page(engine, store)` to a Stack; settings keys are plain strings
  (`MINIMIZE_TO_TRAY_KEY` pattern).
- No production caller of `CredentialManager::store` exists today.
- Cargo.toml deps: zeroize 1, serde 1, serde_json 1.
- **oo7 0.6.0** (verified from published tarball): default features
  tokio+native_crypto; `Keyring::new()` is async; auto-selects file/portal
  backend when sandboxed, else DBus; `create_item(label, attributes, secret,
  replace)`, `search_items(attributes)`, `delete(attributes)`; `Item::secret()
  -> Secret` with `.as_bytes()`; `HashMap<String,String>` impls
  `AsAttributes`; async APIs driven on a blocking thread with
  `pollster`/`block_on` inside `KeyringSource` (async internals, sync surface).
  `blocking` feature is async-std internals only — do not enable.
  `XDG_SCHEMA_ATTRIBUTE` + custom schema string as an attribute. Do not
  enable `schema` feature.

## Design decisions (orchestrator-settled, not lane-relitigable)

1. **oo7 0.6 via pollster block_on** inside `KeyringSource`, sync trait surface
   unchanged. Async runtime independence: pollster, not tokio::task::block_on
   (keyring may be consulted from non-tokio threads).
2. **Real per-session identity**: `auth_config` gains the session id; the
   keyring attribute map is `{schema, application, session_id, secret_type}`.
   Entries are per-session (id-keyed), label = session name + secret type.
   `_live_connect` lookup identity remains as fallback for the jump-hop path
   that passes no id. Jump hop lookup identity: pass the jump session's id.
   VNC/RDP paths already use the session id.
2b. **Session-name lookup support**: keyring lookups match on attributes
   `session_id` first; if nothing found, a second lookup with the session's
   name (doc 06 attributes include `session_name`) must resolve — this serves
   KeePassXC exposed groups where the user titled entries by name. Strict
   exact-token matching, never substring.
2c. **`lookup` must not crash on missing D-Bus service** — `oo7::Error` maps
   to `None` (miss) + a one-line log line, never a panic. Store errors map to
   `CredentialError::StoreError` with backend reason.
2d. **Keyring availability variant**: add `CredentialError::Unavailable`
   detail or a new variant (lane may choose exact shape; must be testable
   without a live Secret Service).
2e. No plaintext secret anywhere in logs, Debug, error text. Debug impls
   redacted, test-pinned.
3. **Bitwarden CLI source** (`BitwardenSource`): `bw get password <name>`
   via `Command::new`, `--nointeraction`, session key via `BW_SESSION` env
   (never argv), 10s timeout, configurable binary path via setting
   `bitwarden.bin` (default `bw`), enable flag `bitwarden.enabled`.
   Lookup identity: item name = `wayterm/<session_name>` — same convention
   RustConn uses, same as our keyring labels. No store support
   (`Unavailable`), read-only source. `bw get password <name>` output is the
   secret. Failure modes: binary missing → `None` + log; timeout → `None` +
   log; `bw` returns error → `None` + log. Never panic, never argv secrets.
4. **Chain order**: `CredentialManager` sources vec order = keyring,
   bitwarden (if enabled), prompt. The prompt source is ALWAYS last.
   Chain construction stays in `ui::tab::credential_manager` — insert the
   Bitwarden source between keyring and prompt when the setting is on.
   Default off.
4b. **Chain is per-connect**: chain built per dial, no cross-connect secret
   caching, no TTL cache (explicitly rejected: RustConn's 300s TTL leaves
   secrets resident in RAM).
4c. **Availability in the UI**: when the keyring source misses but the
   backend reports unavailable (no Secret Service), the challenge dialog
   shows a one-line hint "keyring unavailable — prompting" in the
   `instructions` field of `run_challenges` (the existing one-arg overload:
   `run_challenges(name, Some(hint), prompts)`), when the lookup returned the
   unavailable state and the prompt is about to be shown.
4d. **Settings UI (lane 2)**: a "Credentials" page in Settings with: enable
   Bitwarden toggle (flag `bitwarden.enabled`), binary path entry
   (`bitwarden.bin`, default `bw`), and a static status line showing detected
   state: keyring available / unavailable. Keyring status detection is a
   best-effort probe (`KeyringSource::probe()` or similar) run under xvfb in
   the page's construction only if display tests require it.
4d-note: settings keys are plain strings — reuse the `MINIMIZE_TO_TRAY_KEY`
   pattern (a `pub const &str` in the owning module).
4e. The Bitwarden status line on the page: present only when
   `bitwarden.enabled` — a status row that runs `bw status --nointeraction`
   (availability probe only, output trimmed, never secrets) and shows
   detected / not detected.
4f. **Store flow (lane 2)**: after a successful connect with a prompted
   password that the user wants saved: v1 scope cut — NO new save dialog.
   v1 uses the existing auth flow: the prompt closure in
   `credential_manager` does NOT store; storing is done at
   save-the-dialog-time: when the session dialog saves a session with
   `auth_method = password`, the dialog calls `CredentialManager::store`
   (keyring first) for the password only when the user checks a new
   "Save password to keyring" checkbox in the SSH group of the session
   dialog. Store-on-save, not store-on-connect. This checkbox lives in
   `ui/session_dialog.rs` SSH group.
4f-note: The VNC dialog note (VNC_KEYRING_NOTE) says "stored on first
   connect" — doc drift. Lane 2 adds the store-on-save flow and updates the
   VNC note text to match the store-on-save reality (store happens in
   session dialog, not on connect). All changes stay within lane 2's file
   set (session_dialog.rs and vnc_tab.rs note ref).
   VNC/RDP passwords (secret_type `vnc_password`/`rdp_password`) use the
   same store flow in their dialogs. v1 scope cut: RDP and VNC dialogs do
   NOT get the checkbox; only SSH does. RDP/VNC passwords are prompted per
   connect (status quo for them is already per-connect prompt via the
   chain).
4g. **Delete-on-session-delete** (lane 2): when `delete_session` succeeds at
   `ui/session_tree.rs:441`, call a new `delete_keyring_entries(session_id,
   session_name)` on the KeyringSource (delete by attributes `{session_id}`
   and by label/name) — offer to delete as a confirm dialog variant;
 v1: delete silently when the keyring is available, warn in the report if
 deletion errors; silently skip when keyring unavailable (log only).
 Confirm dialog v2.
4g-note: **Do not change `SessionStore::delete_session` itself** — the
   keyring delete rides the existing UI delete path, not the store layer.
   (Doc 06 §Credential Storage + doc 11 §CredentialManager updated to
   describe this.)
4h. **Docs updates (lane 2)**: doc 06 §Credential Storage rewritten for the
   chain, KeePassXC-via-Secret-Service, attribute schema, delete-on-delete,
   store-on-save checkbox; doc 11 §CredentialManager rewritten for the chain
   with availability hints, Bitwarden source, store-on-save, and the
   `_live_connect` fallback identity. No other docs touched.
4h-note: doc links are CI-checked — keep all relative links intact.

## Lanes

### Lane K1 — `agent/keyring-source`: KeyringSource via oo7

**Files owned:** `src/transport/credentials.rs` (+ sibling tests file if the
module grows), `src/transport/connection.rs`, `src/transport/mod.rs` (mod
declarations only), `src/ui/tab.rs` (chain construction + prompt hint),
`Cargo.toml`, `Cargo.lock`.

**Hard lines:** no trait-signature changes to `CredentialSource`; no
secrets in logs/Debug; no `#[allow]` walls; no scope creep (no Bitwarden,
no settings page, no session-dialog checkbox, no delete flow) — those are
lane K2's. Wire-or-delete: every new item consumed or deleted.

**Acceptance:**
1. `KeyringSource` real impl: oo7-backed lookup/store/delete by attributes;
   sync surface, pollster block_on internally; per-session id attributes +
   name fallback; `_live_connect` fallback for id-less callers; missing
   service → None + log, never panic.
2. `auth_config` threads the real session id (parameter added internally;
   call sites updated; no trait change).
3. `credential_manager` chain unchanged in order but `KeyringSource` now
   constructed with the session id/name (needs the store row id threaded
   from tab open path) — the lane must verify with `rg` that every
   `creds.lookup` call passes a real session id (or `_live_connect`
   fallback for the jump-hop-id-less case).
4. session-id/name threading: lookup first by `session_id` attr, then by
   `session_name` attr fallback (exact match).
3b. `CredentialError` gains the availability distinction (variant shape is
   the lane's call), testable without a live service (mock).
3c. Zeroize-move semantics: lookup returns `Zeroizing<Vec<u8>>` without a
   second plaintext copy at the oo7 boundary where achievable
   (`Secret::as_bytes().to_vec()` into `Zeroizing` is the accepted baseline;
   `into_zeroizing`-style move is a bonus, not a requirement).
4. Unit tests: attribute-map builder, error mapping, name-fallback logic —
   all display-free, no live Secret Service required (mock/no-service
   cases). Keyring paths without a live service must pass green by mapping
   to `None`+log.
5. Mutation checks: break the attribute builder (wrong session id) → lookup
   test fails; break error mapping → error test fails.
6. Gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` all green.
7. Push `agent/keyring-source`.

### Lane K2 — `agent/bitwarden-source`: Bitwarden CLI source + chain + UI + docs

**Files owned:** `src/transport/credentials.rs` (Bitwarden source addition
only — no KeyringSource edits), `src/ui/settings.rs`, `src/ui/settings/`
(new `credentials.rs` page), `src/ui/session_dialog.rs` (SSH-group
checkbox), `src/ui/session_tree.rs` (delete flow), `src/ui/tab.rs`
(chain-order insertion point), `src/ui/vnc_tab.rs` (note text only),
`docs/06-session-store.md`, `docs/11-ssh-auth-security.md`.

**Hard lines:** do not edit the K1 KeyringSource code (except the
chain-order insertion in `credential_manager` if K1 landed first — take
the union). All K1 lane hard lines apply. Wire-or-delete. No secrets in
logs, no argv secrets, session key never in argv.

**Acceptance:**
1. `BitwardenSource` in credentials.rs: env-var session key, `--nointeraction`,
   10s timeout, configurable binary (setting `bitwarden.bin`, default `bw`),
   enable flag (`bitwarden.enabled`, default off), lookup identity
   `wayterm/<session_name>`, read-only (`store` returns Unavailable),
   failures → `None`+log.
2. Chain: `credential_manager` inserts `BitwardenSource` between keyring and
   prompt when `bitwarden.enabled` — requires threading the store handle
   into `credential_manager` (it currently takes only session_name; the
   `SessionTab::open` path has the store). NOTE from main: the K1 lane
   threads the session id; K2 threads the store handle. NOTE from
   session_dialog's existing `SessionDialog::edit` call shape (line ~771
   `SessionDialog::edit(&parent, &stored, &[], &[])`) — the checkbox needs a
   store handle for the settings row read, not for the dialog itself.
2b. Store-on-save: session dialog SSH group gains "Save password to
   keyring" checkbox → `CredentialManager::store` (keyring first, walk
   Unavailable). If keyring unavailable, the checkbox stays enabled and the
   store attempts; failure surfaces as a dialog toast/inline message, not a
   silent miss. Failure to store ≠ failure to save the session.
2b-note: session dialog needs the store handle — `SessionDialog::edit`
   shape already receives the session row from the tree; the checkbox
   write-side calls `store.set_setting` only if lane adds a
   `save_password` preference — v1: no per-session preference; the
   checkbox stores at save time only, no preference row.
2c. Delete flow: on `delete_session` success at session_tree.rs:441 flow,
   delete keyring entries by attributes (session_id) and by name —
   silently when keyring available; when unavailable, log and continue
   (session deletion never blocked).
2d. **Docs**: doc 06 §Credential Storage rewritten (chain, KeePassXC story,
   attributes, delete-on-delete, store-on-save); doc 10 checkbox for this
   work item? No — doc 10 is the build order; only docs 06/11 updated.
2e. Unit tests: BitwardenSource with a fake `bw` (a shell script fixture
   committed under `tests/` fixtures) — missing binary, timeout, error
   exit, success path; settings page construction under xvfb + a
   display-free model test; session-dialog checkbox logic display-free
   test; delete flow display-free test.
2f. Mutation checks: break fake-bw invocation (drop env passthrough) →
   test fails; break chain order (bitwarden after prompt) → test fails.
2g. Gates: fmt, clippy -D warnings, cargo test green. Push
   `agent/bitwarden-source`.

### Integration (orchestrator, after both lanes)

- Merge K1 → K2 order (chain lanes merge as one branch first), wave branch
  `agent/credential-sources-wave` off main, run gates, real keyring rig
  test, open PR.
- Verification rig: a real Secret Service (gnome-keyring + secret-tool) in
  a `dbus-run-session` for a real create/search/delete round-trip test run
  by me, not the lanes (lanes stay display-free and service-free). If the
  host lacks gnome-keyring, install it (sudo is passwordless). **A green
  skip proves nothing** — the rig test is the proof the keyring path works
  against a real backend.
- PR body records the lane deviations and my rulings, owner decisions at
  the end.
- PR title: `feat: credential sources — OS keyring (oo7) + Bitwarden CLI`.