# Plan: Phase 10 — libghostty-vt terminal backend

Date: 2026-10-15. Owner lane: `agent/phase10-ghostty-backend`.
Dispatched to Claude Code by Hermes (Alex's instruction 2026-10-15).

## Goal

A second `TerminalBackend` implementation (doc 02) backed by libghostty-vt
(docs 03/05), behind an opt-in `backend-ghostty` Cargo feature. VTE stays
the default backend. The build must work with no ghostty present
(feature off) and with the feature on.

## Design decisions

- **Pin the dependency.** libghostty-vt has no tagged stable release; the
  lane pins one upstream commit (hash recorded in this file and in the
  build script) and links FFI against it. Upstream churn is handled by a
  deliberate pin bump, never silently.
- **Feature-gated.** Everything ghostty lives behind `backend-ghostty`.
  Default build = VTE only, unchanged.
- **UI is a runtime toggle, not a compile-time user choice.** Settings
  gets an "Experimental Ghostty terminal" option (persisted in the
  session store per app preferences, doc 06). When the feature is off,
  the option is hidden/disabled with a note. VTE is the fallback for
  every tab unless the user opted in, per tab created after the toggle.
- **Trait unchanged.** `TerminalBackend` (doc 02) signatures untouched;
  the ghostty backend is a new impl next to `VteBackend`/`FakeBackend`.

## Tasks

1. **P10-1 — FFI crate + build**: a small `src/terminal/ghostty/ffi`
   (or `vendor` + build.rs) wrapper around the pinned libghostty-vt C
   API. Build path documented (Zig version, env). Feature-gated.
2. **P10-2 — GhosttyBackend**: `TerminalBackend` impl over the FFI
   (feed input, resize, snapshot/grid read for rendering state). Unit
   tests with FakeBackend-style expectations where the API allows.
3. **P10-3 — render widget + wiring**: GTK4 custom render widget
   drawing the ghostty grid; factory that picks backend per the
   settings toggle; VTE remains default; docs 02/03/10 updated.
4. **P10-4 — benchmark vs VTE**: criterion or simple bench behind the
   feature, comparing feed/parse throughput (docs 03 §evaluation).

## Verification

Per task and integrated: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test` — run with the
feature **off** (must be identical to before) and, when the pinned
library builds, with the feature **on**. Skips are documented in the
report. Doc link check passes (docs CI).

## Status

- [ ] P10-1 through P10-4