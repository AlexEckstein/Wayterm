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

**Current state: design phase.** The numbered docs (`01-…` through
`11-…`) at the repo root are the source of truth for architecture —
reviewed, fact-checked (versions verified 2026-08-28), and committed.
No Rust code exists yet; Phase 1 (doc 10) creates the scaffold.

## Non-negotiable rules

1. **Docs are the contract.** Read the design doc(s) relevant to your
   task before writing code. If your task contradicts a doc, STOP and
   report the contradiction — do not code around it.
2. **The traits are the seams.** `Transport` (doc 04),
   `TerminalBackend` (doc 02), the SFTP command enum (doc 09), and
   `SshConnection` (docs 04/11) are module boundaries. Changing a trait
   signature is an orchestration-level decision: escalate, don't
   improvise.
3. **No secrets in code, commits, or logs.** Not even test fixtures.
   Credentials live in the OS keyring only (docs 06/11).
4. **No scope creep.** Implement the task as written. Ideas go in the
   report, not the diff.
5. **Verification is part of the task.** Done means `cargo fmt
   --check`, `cargo clippy -- -D warnings`, and `cargo test` all pass,
   plus the task's acceptance criteria (doc 10 for phase-level work).
   In design phase, done means every doc you touched stays consistent
   with every cross-referenced doc.
6. **Small, single-purpose commits.** Conventional-commit style
   (`feat:`, `fix:`, `docs:`, `test:`, `chore:`). Never amend or
   rebase a branch someone else may have fetched.
7. **main is trunk.** Never commit directly to main. Work on branches;
   merges to main happen only through the merge protocol below.

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

`.github/workflows/ci.yml` runs on every push and PR:

- **detect** — checks whether `Cargo.toml`/`Cargo.lock` exist; gates
  the rust and audit jobs.
- **docs** — always runs: validates that relative links across the
  design docs resolve.
- **rust** — activates automatically at Phase 1: fmt check, clippy
  `-D warnings`, `cargo test`, plus a local sshd for transport
  integration tests (`SSH_TEST_*` env vars, doc 11 §Testing). Phase
  2's definition of done requires this job.
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

## Repository layout (target; docs move to docs/ at Phase 1)

```
docs/           ← design docs 01–11
src/transport/  ← Transport trait + SSH/Local/Serial impls (doc 04)
src/backend/    ← TerminalBackend + VteBackend + FakeBackend (doc 02)
src/connection/ ← SshConnection, supervisor (docs 04/11)
src/sftp/       ← SFTP task + commands (doc 09)
src/ui/         ← GTK window, tabs, session tree, sidebar (docs 01/07/09)
src/store/      ← SQLite session store (doc 06)
src/hotkeys/    ← ActionRegistry, HotkeyEngine (doc 07)
tests/          ← integration tests incl. sshd CI tests (doc 11)
```