# AGENTS.md — Operating Rules for AI Agents Working on Wayterm

> Read this file fully before doing anything in this repository.
> It applies identically on every machine and to every agent — Claude
> Code, Codex, OpenCode, Hermes subagents, CI bots, or a human in a
> hurry. If it conflicts with code you see, this file wins unless a
> human said otherwise.

## What this is

**Wayterm** (app id `com.alex-eckstein.wayterm`) — an open source,
multi-protocol connection manager and terminal for Linux. Rust + GTK4 +
VTE, russh SSH, rusqlite, libsecret keyring.
Repo: github.com/AlexEckstein/Wayterm.

**Current state:** feature-complete for its planned scope. The design
docs `docs/01-…` through `docs/15-…`, one per feature, describe the
system as built and are the source of truth for architecture; each ends
with a **Limitations** section listing that feature's known gaps.
`docs/01-architecture.md` is the map.

## Non-negotiable rules

1. **Docs are the contract.** Read the design doc(s) relevant to your
   task before writing code. If your task contradicts a doc, STOP and
   report the contradiction — do not code around it.
2. **The traits are the seams.** `Transport` (doc 03),
   `TerminalBackend` (doc 02), the SFTP command enum (doc 07), and
   `SshConnection` (doc 04) are module boundaries (doc 01 §Seams).
   Changing a trait signature is an orchestration-level decision: escalate, don't
   improvise.
3. **No secrets in code, commits, or logs.** Not even test fixtures.
   Secrets come only from the credential chain of doc 05 (OS keyring,
   enabled Bitwarden, prompt) — never in code, SQLite, logs, or plain
   files.
4. **No scope creep.** Implement the task as written. Ideas go in the
   report, not the diff.
5. **Verification is part of the task.** Done means `cargo fmt
   --check`, `cargo clippy -- -D warnings`, and `cargo test` all pass,
   plus the task's acceptance criteria, and every doc you touched —
   its Limitations section included — stays consistent with the code
   and with every cross-referenced doc.
6. **Small, single-purpose commits.** Conventional-commit style
   (`feat:`, `fix:`, `docs:`, `test:`, `chore:`). Never amend or
   rebase a branch someone else may have fetched.
7. **main is trunk.** Never commit directly to main. Work on branches;
   merges to main happen only through the merge protocol below.
8. **No AI attribution.** Never add "written by Claude" or any other
   agent or LLM credit, no Co-Authored-By trailer, no tool-generated
   signature, to commits or PRs. Your contributions are work, not
   advertisements for AI companies.

## Branch discipline

- Branch names: `agent/<task-slug>` (autonomous work) or
  `<type>/<slug>` (fix/hotfix). One task = one branch = one worktree
  (`git worktree add ../wayterm-<slug> <branch>`).
- Two agents never share one worktree or one branch.
- Commit early and often; push your branch before reporting done.
- Blocked? Commit with a `WIP:` prefix, push, and report the blocker.
  Never silently abandon a branch.
- Report back with: branch name, files touched, test results, and any
  doc contradictions found.

## Merge protocol

- An orchestrator (or Alex) merges to main — agents never merge each
  other's branches.
- On conflicts, classify every hunk: **disjoint-intent** (combine
  both), **same-question-different-answer** (pick one per stated
  intents and surface the decision), **superseded** (keep the
  survivor, note why). Record decisions in the merge commit body.
- Same file conflicting across multiple merge rounds = hotspot: flag
  it for decomposition instead of reconciling it again next round.
- CI must be green before merge. Never merge red.

## CI (what "green" means)

`.github/workflows/ci.yml` runs on pushes to main and on every PR:

- **detect** — checks whether `Cargo.toml`/`Cargo.lock` exist; gates
  the rust, ghostty and audit jobs.
- **docs** — always runs: validates that relative links across the
  design docs resolve.
- **rust** — fmt check, clippy `--all-targets -D warnings`, `cargo
  test` (sshd-gated suites skip themselves), then a real sshd for
  `tests/transport_sshd.rs` (`SSH_TEST_*` env vars, doc 15 §CI).
- **ghostty** — clippy and `cargo test` with `--features
  backend-ghostty`, then the Ghostty widget test under xvfb, against
  libghostty-vt built at the pin and cached per pin (doc 02
  §libghostty-vt pin).
- **audit** — weekly cargo advisory sweep once `Cargo.lock` exists.

Do not disable or bypass checks to get green. Do not push with
`--no-verify`. If a check is wrong, fix the check in a PR.

## Multi-machine rules

- Git IS the sync mechanism. Push branches; `git fetch --prune`
  before starting any work; never copy files between machines.
- Anything not pushed doesn't exist. WIP survives via pushed
  branches, not local state.
- The agent runtime (herdr, tmux, bare terminals) is a per-machine
  choice; this file and the repo are the shared contract.

## Repository layout

```
docs/            ← design docs 01–15, one per feature (doc 01 is the map)
src/terminal/    ← TerminalBackend, VteBackend, GhosttyBackend, FakeBackend (doc 02)
src/transport/   ← Transport, SSHTransport, SshConnection, supervisor, auth,
                   host keys, jump, forward, credentials, local, serial,
                   reconnect, transport factory (docs 03, 04, 05, 06)
src/sftp/        ← SFTP task + commands (doc 07)
src/vnc/          ← VNC client + framebuffer, engine task (doc 08)
src/rdp/          ← RDP process wrapper + cert policy (doc 08)
src/store/       ← SQLite session store and model (doc 09)
src/transfer/    ← session import/export: ssh config, Remmina, MobaXterm, Asbru, JSON (doc 10)
src/hotkeys/     ← ActionRegistry, HotkeyEngine (doc 11)
src/macros/      ← macro model, recorder, input tap, key notation, playback (doc 12)
src/ui/          ← GTK window, settings, status bar, tray, appearance (docs 01, 02, 11, 13)
src/ui/tabs/     ← tab pages and connect flows, TerminalTab wiring, lifecycle, restore, tab and terminal actions (docs 02, 03, 04, 06, 08, 11)
src/ui/dialogs/  ← auth, host key, session, folder, tunnels, host picker (docs 03, 04, 09)
src/ui/panels/   ← side dock, session tree, SFTP sidebar, macros panel and editor (docs 01, 07, 09, 12)
tests/           ← integration tests; tests/common is the shared sshd fixture (doc 15)
benches/         ← cargo benchmarks (terminal output parsing)
assets/          ← SVG icons (app icon, window mark, session-tree icons)
packaging/       ← desktop entry, metainfo, Flatpak manifest, AppImage recipe (doc 14)
scripts/         ← local sshd runners, the auth-method matrix, and the serial
                   loopback used by the integration suites
```