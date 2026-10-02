# Phase 1: Wayterm Skeleton — Implementation Plan

> **For Hermes:** Execute task-by-task via subagent-driven-development.
> Each task is dispatched to a FRESH subagent with the full task text in
> context. Two-stage review (spec compliance, then code quality) before
> the next task starts. All work on branch `agent/phase1-skeleton`,
> worktree `../wayterm-phase1`.

**Goal:** A launching GTK4 app with the session tree, SQLite store, and
CRUD UI — doc 10 Phase 1. No connections yet.

**Architecture:** Rust + GTK4, modules per AGENTS.md layout. The store
layer (src/store/) is pure logic with unit tests; UI layer (src/ui/) is
thin GTK on top. The two never touch each other's internals — store
exposes typed CRUD, UI consumes it.

**Tech stack:** gtk4 0.11 (v4_18 feature), vte4 0.10, rusqlite 0.40
(bundled), tokio, serde. Toolchain: stable, installed on the VM.

**Verification baseline for EVERY task:** `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, all green
in the task's worktree before commit. CI (rust job) activates on main
when Cargo.toml lands, so Task 1 must be clippy-clean.

---

### Task 1: Cargo scaffold + minimal GTK window

**Objective:** A compiling crate that opens a Wayterm window; repo
hygiene for the Rust era.

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/app.rs`
- Modify: `.gitignore` (remove the `Cargo.lock` line — bin crates
  commit the lockfile)

**Steps:**

1. Write `Cargo.toml`:

```toml
[package]
name = "wayterm"
version = "0.1.0"
edition = "2021"
description = "An open source, multi-protocol connection manager and terminal for Linux"
license = "Apache-2.0"

[dependencies]
gtk4 = { version = "0.11", features = ["v4_18"] }
vte4 = "0.10"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync"] }
rusqlite = { version = "0.40", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
tempfile = "3"
```

2. `src/main.rs`:

```rust
mod app;

fn main() -> glib::ExitCode {
    app::run()
}
```

3. `src/app.rs` — application id, activate builds the window:

```rust
use gtk4::prelude::*;

pub const APP_ID: &str = "com.alex-eckstein.wayterm";

pub fn run() -> glib::ExitCode {
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| build_ui(app));
    app.run()
}

fn build_ui(app: &gtk4::Application) {
    let win = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Wayterm")
        .default_width(1200)
        .default_height(800)
        .build();
    win.present();
}
```

4. Remove the `Cargo.lock` ignore line from `.gitignore`; commit the
   lockfile.

5. Verify: `cargo build` clean; `cargo clippy --all-targets -- -D
   warnings` clean; `cargo fmt --check` clean; smoke test:
   `xvfb-run -a timeout 10 cargo run` → exit code 124 (killed by
   timeout = window launched and stayed up).

**Commit:** `feat: cargo scaffold — GTK4 app shell, window launches`

---

### Task 2: SessionConfig model (src/store/model.rs)

**Objective:** The typed data model per docs/06-session-store.md.

**Files:**
- Create: `src/store/mod.rs`, `src/store/model.rs`, `src/store/model/tests.rs`
- Modify: `src/main.rs` (add `mod store;`)

**Spec:** Transcribe from docs/06-session-store.md §"Session Config
Struct" verbatim — `SessionType`, `AuthMethod` (5 variants incl.
`KeyboardInteractive`), `HostKeyPolicy`, `ForwardKind`, `ForwardRule`,
`SshOptions`, `RdpOptions`, `VncOptions`, `SessionConfig`, `Folder`.
All derive `Clone, Debug, Serialize, Deserialize`.

**Tests (write first, they fail):**
- serde round-trip: every enum serializes to the exact snake_case
  string doc 06 stores in TEXT columns (`"ssh"`, `"keyboard-interactive"`
  — use `#[serde(rename_all = "snake_case")]` and assert the wire
  format in the test)
- `SessionConfig` JSON round-trip with all fields populated
- `Default` for `SessionConfig` is sane (SSH, no folder)

**Acceptance:** `cargo test` green; doc-06 wire formats locked by tests.

**Commit:** `feat(store): SessionConfig model + serde wire formats`

---

### Task 3: SQLite open + migrations + seed (src/store/db.rs)

**Objective:** `SessionStore::open(path)` creates the full schema from
docs/06-session-store.md §"Database Schema" (folders, sessions,
hotkeys, settings, connection_history + all indexes), seeds default
data (§"Default Data" — 29 hotkeys incl. sftp_* rows, settings incl.
`host_key_policy=strict`), and sets WAL + foreign_keys pragmas.

**Files:**
- Create: `src/store/db.rs`, tests in `src/store/db/tests.rs`
- Modify: `src/store/mod.rs`

**Key signatures:**

```rust
pub struct SessionStore { conn: rusqlite::Connection }

impl SessionStore {
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self>;
    pub fn in_memory() -> rusqlite::Result<Self>;  // tests + CI
}
```

**Schema source of truth:** the SQL in doc 06 is copied verbatim into
`db.rs` as a `MIGRATION_001` const string. Schema lives in ONE place
in code — do not paraphrase doc 06; copy it.

**Tests:**
- `open` on a temp dir creates all 5 tables (query `sqlite_master`)
- `open` twice on same path is idempotent (`IF NOT EXISTS`)
- seed check: `SELECT COUNT(*) FROM hotkeys` = 29 rows; `settings`
  contains `host_key_policy=strict`
- WAL pragma active after open

**Acceptance:** `cargo test` green.

**Commit:** `feat(store): SQLite schema, migrations, default seed`

---

### Task 4: Folder CRUD (src/store/db.rs)

**Objective:** Typed folder operations.

**Signatures:**

```rust
impl SessionStore {
    pub fn insert_folder(&self, name: &str, parent_id: Option<i64>) -> rusqlite::Result<i64>;
    pub fn list_folders(&self, parent_id: Option<i64>) -> rusqlite::Result<Vec<Folder>>;
    pub fn rename_folder(&self, id: i64, new_name: &str) -> rusqlite::Result<()>;
    pub fn delete_folder(&self, id: i64) -> rusqlite::Result<()>;  // cascades
}
```

**Tests:** insert → list shows it; nested folders (parent_id chain);
rename visible; delete cascades child folders AND nulls orphaned
sessions' folder_id (doc 06: `ON DELETE CASCADE` for folders,
`ON DELETE SET NULL` for sessions.folder_id — assert BOTH behaviors).

**Commit:** `feat(store): folder CRUD`

---

### Task 5: Session CRUD (src/store/db.rs)

**Objective:** Typed session persistence, JSON columns via the Task-2
model.

**Signatures:**

```rust
impl SessionStore {
    pub fn insert_session(&self, config: &SessionConfig) -> rusqlite::Result<i64>;
    pub fn get_session(&self, id: i64) -> rusqlite::Result<SessionConfig>;
    pub fn list_sessions(&self, folder_id: Option<i64>) -> rusqlite::Result<Vec<SessionConfig>>;
    pub fn update_session(&self, config: &SessionConfig) -> rusqlite::Result<()>;
    pub fn delete_session(&self, id: i64) -> rusqlite::Result<()>;
    pub fn touch_connected(&self, id: i64) -> rusqlite::Result<()>; // last_connected, connect_count++
}
```

JSON columns (`ssh_options`, `rdp_options`, `vnc_options`,
`forward_rules`) serialize via serde from the Task-2 model — no
stringly-typed leakage.

**Tests:** full CRUD round-trip (insert → get == original; update →
get reflects; list by folder filters; delete removes); JSON columns
round-trip a populated `ForwardRule` and `SshOptions`; `touch_connected`
increments count.

**Commit:** `feat(store): session CRUD with JSON column mapping`

---

### Task 6: Main window layout (src/app.rs, src/ui/mod.rs)

**Objective:** The doc-01 layout — `GtkPaned`: session tree left,
`GtkNotebook` right.

**Files:**
- Create: `src/ui/mod.rs`, `src/ui/main_window.rs`
- Modify: `src/app.rs` (build_ui delegates to MainWindow::new)

**Spec:** Horizontal `GtkPaned`, position 280. Left pane: placeholder
`GtkLabel` ("Sessions") until Task 7. Right pane: `GtkNotebook` with
one empty default tab ("Home"). CSS: dark-friendly default none (GTK
handles it). Window title "Wayterm".

**Verify:** `xvfb-run -a timeout 10 cargo run` → 124; clippy/test
green.

**Commit:** `feat(ui): main window — paned layout with tree slot + notebook`

---

### Task 7: Session tree widget (src/ui/session_tree.rs)

**Objective:** Folder hierarchy from the store, rendered.

**Spec:** `GtkTreeView` + `GtkTreeStore` (columns: icon, name, hidden
id, hidden is_folder). Populate: top-level folders, then sessions
within (icon per SessionType: 📁 folder, 🖥 ssh, 🖼 rdp, 📺 vnc,
🔌 serial, 🏠 local). A `refresh()` method re-reads the store. Tree is
read-only this task — CRUD UI is Tasks 8/10.

**Acceptance:** populate a temp store with nested folders + mixed
session types → tree shows hierarchy correctly (unit-test the model
mapping; visual via smoke run).

**Commit:** `feat(ui): session tree renders folder hierarchy`

---

### Task 8: Folder UI CRUD (src/ui/session_tree.rs)

**Objective:** Right-click context menu on the tree: New Folder,
Rename, Delete. Name via `GtkAlertDialog`/entry dialog. All ops go
through the store; `refresh()` after each. Delete confirms first
(`confirm_close` style dialog), and the dialog states that child
folders delete while sessions move to root (Task-4 semantics).

**Acceptance:** smoke test creates/renames/deletes a folder; store
state matches after each op.

**Commit:** `feat(ui): folder CRUD via context menu`

---

### Task 9: Session config dialog (src/ui/session_dialog.rs)

**Objective:** GTK4 dialog creating/editing a `SessionConfig`.

**Spec:** Fields per doc 06: Name, Type (dropdown: ssh/rdp/vnc/serial/
local), Folder (dropdown of folders), Host, Port, Username, Auth
Method (dropdown incl. keyboard-interactive), Notes. Type-specific
fields show/hide on type change (e.g. serial shows device/baud; rdp
shows domain/resolution). Two constructors:
`SessionDialog::new_session(folders)`, `SessionDialog::edit(config)`.
Returns `Option<SessionConfig>` on OK.

**Acceptance:** dialog round-trips a full SSH config (unit test the
field→config mapping where possible without a display; visual check
via smoke run).

**Commit:** `feat(ui): session config dialog`

---

### Task 10: Session UI CRUD (src/ui/session_tree.rs)

**Objective:** Context menu: New Session (Task 9 dialog), Edit
Session, Delete Session; double-click a session shows a placeholder
tab (Task 11 widget). All ops persist via store + `refresh()`.

**Acceptance:** create → persists across app restart (smoke test with
same DB path twice).

**Commit:** `feat(ui): session CRUD — full doc-06 store exposed in tree`

---

### Task 11: Placeholder terminal tab (src/ui/tab.rs)

**Objective:** Double-click session → `GtkNotebook` tab with the
session's name and a placeholder `GtkLabel` ("terminal lands in Phase
2 — doc 04"). Tab close button works. This is the seam where Phase 2's
`TerminalTab` plugs in.

**Acceptance:** open 3 sessions → 3 tabs, closable, titles correct.

**Commit:** `feat(ui): placeholder session tabs — Phase 2 seam`

---

### Task 12: Phase 1 closeout

**Objective:** Green everything, push, PR-ready.

**Steps:**
1. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` — all green
2. Full smoke: `xvfb-run -a timeout 10 cargo run` → 124
3. Update README Status section: "Phase 1 complete — skeleton app"
4. Push `agent/phase1-skeleton`; report files, tests, any doc
   contradictions found

**Commit:** `docs: Phase 1 complete`