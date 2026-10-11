# Phase 9 — Import/export and packaging

Status: lanes dispatching · 2026-10-11 · Orchestrator: Hermes (manager
profile) · Fleet: Claude Code one-shot lanes (standing instruction).

## Goal

Doc 10 Phase 9: `~/.ssh/config` import first (the adoption driver, offered
on first run), JSON export/import, Remmina, MobaXterm, Asbru — plus
packages: .deb (cargo-deb), Flatpak manifest, AppImage, release workflow,
and an installed icon.

## Current state (verified on main 9fb19c3)

- No import/export code exists outside the hotkeys transfer module
  (`src/hotkeys/transfer.rs`, hotkeys-JSON only) and the store's serde
  tests (`rg -il "import|export" src/`; those are the only hits).
- Store API ready for transfer: `SessionStore::insert_session`,
  `insert_folder`, `list_folders(parent)`, `list_sessions(folder)`,
  `get_session`, `update_session`, `in_memory()` for tests
  (src/store/db.rs). `SessionConfig` (src/store/model.rs) never holds
  secrets; its serde spellings are locked by tests.
- `russh-config` 0.58.0 verified from source: per-alias resolution only
  (`parse/parse_home/parse_path` → one `Config`), NO Host-block
  enumeration, no `Include` handling (unknown keywords are skipped).
  Doc 06's "via russh-config" is wrong — the docs lane patches doc 06.
- GTK chooser precedent: `gtk4::FileDialog` in `src/ui/sftp_sidebar.rs`.
- CI has detect/docs/rust/audit jobs; no release job. Cargo.lock exists
  (audit job is live).

## Design decisions (orchestrator-settled 2026-10-11 — do not relitigate)

**1. SSH-config import engine = own read-only block scanner; russh-config
stays OUT of Cargo.toml.** Verified reason: russh-config 0.58 cannot
enumerate Host blocks (API is `parse(file, host)` per alias; `Include` is
swallowed as an unknown keyword). The scanner: exact-name `Host` blocks →
sessions (aliases joined `"; "` per OpenSSH multi-name lines); wildcard
blocks (`*`, `?`, lists) → template sessions with a marker in `notes`;
`Match` blocks skipped, counted in the report; `Include` followed with
depth ≤ 5 and cycle detection (relative paths resolve against the
including file's dir; `~` expands); negated `!` patterns never create
sessions. Mapped keys: `HostName`→host, `User`→username, `Port`→port,
`IdentityFile`→key_path (first one), `IdentitiesOnly yes`→auth_method
Key, `ServerAliveInterval`→`ssh_options.keepalive_interval`, `ProxyJump`
→ `jump_session_id` aliased to an imported peer session when the peer's
block exists in the same import, else dropped with a warning. `~`
in IdentityFile expands on import; import never reads key files.

**2. Importers are pure readers:** source text in →
`(Vec<FolderSpec>, Vec<SessionConfig>, ImportReport)` out, no store
access, no network, no exec, no writes. FolderSpec is (name, parent-name)
-ordered; the thin store-writing layer in `transfer/mod.rs`
(`apply_import`) inserts folders parent-first, remaps ids, inserts
sessions, and returns the report. Remmina secrets never transfer: any
`password=` line is skipped with a warning.

**3. JSON wire shape:** `{"version":1,"folders":[{"name","icon",
"children":[…],"sessions":[…nested…]}]}`. Top-level bare `sessions` array
lands in root on import; export never produces one (folders own their
sessions). `version > 1` → a clear error naming the version. Roundtrip
test locks the shape. Secrets excluded by construction; a test asserts
no `password`-ish keys in any exported text.

**4. Remmina/MobaXterm/Asbru parsers:** Remmina `.remmina` key=value
(`server=user@host:port` style, `group=`/`groups=` as folder hint,
`ssh_*`/`name` fields; encrypted `password` lines → warnings); MobaXterm
ini — own small parser, no new dep (`[Bookmarks*]` sections, `SubRepl`/
nested-node conventions → folders; anything unmappable → warning);
Asbru YAML via `serde_yaml_ng` 0.10 (serde_yaml is archived upstream;
ng is the maintained fork — settled). Unmappable entries land in
warnings, never silently dropped.

**5. Packaging:** .deb via cargo-deb (`[package.metadata.deb]` in
Cargo.toml; assets/icons installed via `assets` field + maintainer
essentials kept minimal). Flatpak manifest
`packaging/flatpak/com.alex-eckstein.wayterm.json` builds from local
source with system deps declared (gtk4, vte4 ≥ 0.72, sqlite via bundled
rusqlite — no portal work until first-run test); AppImage recipe
`packaging/appimage/wayterm.yml` (pkg2appimage) — CI-only tooling.
Release workflow `.github/workflows/release.yml`: on tag push `v*`,
ubuntu-24.04 runner builds the .deb (cargo-deb) and the AppImage, and
attaches both to a GitHub Release. Flatpak is built locally/verified in
CI only as a lint-level check (manifest validity) — a full flatpak CI
build is out of scope this phase.

**6. Icon:** assets/icons committed in P9-2 (orchestrator-generated
2026-10-11: hicolor 64/128/256 PNG + scalable SVG master; do not
regenerate). Desktop file + AppStream metainfo ride with P9-2.

**7. Wire-or-delete rule (fleet standing):** every item is either
consumed by code added in this task or deleted — no `#[allow]` walls.

## Lanes

### P9-1 — transfer engines (branch `agent/p9-transfer`, worktree
../wayterm-p9-transfer)

Files allowed: `src/transfer/` (NEW — mod.rs, ssh_config.rs, json.rs,
remmina.rs, mobaxterm.rs, asbru.rs; each with unit tests or one tests.rs)
and `src/lib.rs` (add one `pub mod transfer;` line). Nothing else. New
deps allowed: `serde_yaml_ng = "0.10"` only. Do NOT add russh-config.

Tasks:

1. Define `ImportSource`, `ImportReport { imported, skipped, templates,
   warnings }`, the importer trait/fn signatures, folder spec, and the
   pure-parsing engines for the four sources (decisions 1–4).
2. `export_json` / `apply_import` store bridge (decision 2/3 shape),
   built on the store API above; `apply_import` is the ONLY place that
   writes.
3. Table-driven tests for the scanner (Include chain, cycle, depth cap,
   Match skip, wildcard→template, negation, multi-alias hosts, quoted
   values, ProxyJump alias + drop-with-warning), JSON roundtrip
   (folder→subfolder→session, then into a fresh in-memory store,
   equality on (name, host, port, type) and folder nesting), one fixture
   test per Remmina/MobaXterm/Asbru (fixtures as test constants or
   `tests/` files), and the secrets-absent assertion.
4. Gates in the worktree: `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test` — all bare,
   all pass.

Acceptance: engines complete with passing gates; no store-write outside
`transfer::apply_import`; no secret ever lands in a SessionConfig.

### P9-2 — packaging (branch `agent/p9-package`, worktree
../wayterm-p9-package)

Files allowed: `packaging/**` (NEW: flatpak manifest, appimage recipe),
`.github/workflows/release.yml` (NEW), `packaging/deb/` maintainer
extras if needed, `Cargo.toml` (+ `[package.metadata.deb]` section only),
`Cargo.lock` ( regenerated if the metadata machinery requires),
`assets/icons/**` (committed as generated), `README.md` (one packaging
section). Nothing else. Do NOT touch `src/` except when cargo-deb's
metadata requires a `[[bin]]` tweak — flag it in the report if so.

Tasks:

1. `[package.metadata.deb]`: maintainer `Alex Eckstein
   <alex@alex-eckstein.com>` (check git config; do not invent — use
   `Packaged for Wayterm contributors` if unset), section `net`, assets
   install: bin → /usr/bin, icons → /usr/share/icons/hicolor/...,
   desktop file, metainfo → /usr/share/metainfo.
2. `packaging/desktop/com.alex-eckstein.wayterm.desktop` (NEW) and
   `packaging/metainfo/com.alex-eckstein.wayterm.appdata.xml` (NEW;
   releases list may cite "see GitHub" — no invented release notes).
3. Flatpak manifest per decision 5; AppImage recipe per decision 5.
4. `.github/workflows/release.yml` per decision 5; the CI rust job stays
   untouched; release job must not run on plain pushes.
5. `README.md`: packaging + import/export section (short).
6. Validate locally: `cargo deb --no-build --no-strip` succeeds (proves
   metadata + asset map); icons and desktop file land in the staging
   tree (report the paths). Flatpak manifest: `flatpak-builder --show-manifest`
   or a JSON-schema eyeball — flatpak-builder may not be installed;
   say so if skipped. If the appimage tooling is absent locally, CI-only
   verification is acceptable and reported as skipped-locally.

Acceptance: local `cargo deb --no-build --no-strip` green; release.yml
lints as valid YAML; icons/desktop/metainfo all present in the tree with
correct app id; gates pass (cargo-side).

### P9-3 — first-run SSH-config import UI (branch `agent/p9-import-ui`,
worktree ../wayterm-p9-import-ui) — AFTER the P9-1 wave merges; spec
pinned to delivered transfer API then.

Files: `src/ui/main_window.rs`, `src/ui/session_tree.rs` (context-menu
entries), `src/app.rs` (first-run offer), `src/ui/import_dialog/` (NEW),
`docs/06-session-store.md` + `docs/10-build-order.md` status sync if
trivially safe (else the docs lane). Details pinned after P9-1 lands.

### P9-4 — docs refresh (branch `docs/phase9-refresh`) — closeout lane

AGENTS.md "Current state" (Phases 1–8 line), doc 06 §Import and export
("via russh-config" → own scanner per decision 1), doc 10 Phase 9
checkboxes, README if the packaging lane left drift. Lands after the
waves so docs describe reality.

## Verification plan

- Lanes gate themselves (report format below); orchestrator re-runs all
  gates on each lane's worktree before trusting anything.
- Wave: all gates on the integrated branch; `cargo deb --no-build
  --no-strip`; release workflow validated by `actionlint` if present
  (else python yaml parse); docs cross-links resolve (CI docs job).
- Doc contradictions: the doc 06 "via russh-config" line — fixed by
  P9-4 with the decision 1 rationale.

## Report format (each lane)

Branch + head commit · files touched · gate results (fmt/clippy/test,
bare, pasted failures if any) · doc contradictions found · open
decisions.

## Open decisions for Alex

1. PR split: P9-1 and P9-2 as two PRs now, P9-3 after review — my
   recommendation (keeps packaging reviewable and unblocks the UI lane).
2. Release naming/tag policy (v0.1.0 first tag?) — parked until P9-2
   returns; no release is cut without your say-so.
3. keyring is still stubbed (doc 10 gap) — unchanged by this phase;
   flagged so packaging isn't mistaken for credential wiring.