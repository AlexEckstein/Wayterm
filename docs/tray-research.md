# Tray research: Linux tray standard + SNI crate decision (P4-5)

Date: 2026-10-08. Task: Phase 4 "Tray icon (minimize to tray, notifications
for disconnects)" (`10-build-order.md` Phase 4). This doc is the REQUIRED
research step before code; implementation lives in `src/ui/tray.rs`.

## 1. What is the 2026 Linux tray standard for GTK4 apps?

**StatusNotifierItem (SNI) + DBusMenu over D-Bus.** The freedesktop/KDE
`org.kde.StatusNotifierItem` (+ `org.kde.StatusNotifierWatcher` for
registration, `com.canonical.dbusmenu` for menus) protocol is the de-facto
standard in 2026:

- KDE Plasma implements SNI natively (it is KDE's spec).
- GNOME Shell has no built-in tray; the ecosystem standard is the
  "AppIndicator and KStatusNotifierItem Support" extension (3.1M+ downloads,
  supports Shell 45–51) or the newer self-contained "Status Tray" extension
  (GNOME 45–50), both of which speak SNI/DBusMenu and provide their own
  `org.kde.StatusNotifierWatcher`.
  Sources: <https://extensions.gnome.org/extension/615/appindicator-support>,
  <https://extensions.gnome.org/extension/9164/status-tray>,
  <https://github.com/ubuntu/gnome-shell-extension-appindicator>,
  <https://github.com/keithvassallomt/status-tray>.
- The legacy XEmbed systray (`GtkStatusIcon`, removed in GTK4) is dead:
  no GTK4 API exists for it, and modern hosts don't implement it.
- `libappindicator-gtk3` / Ayatana AppIndicator is the "legacy but
  universal" C path — but it is GTK3-bound. Linking GTK3 into a GTK4
  process for one icon is heavy, pulls the deprecated stack into our
  Flatpak, and buys nothing SNI doesn't already provide.

Conclusion: a GTK4 app in 2026 implements SNI directly over D-Bus.
No GTK widget is involved; the icon/menu live in the host (shell
extension, Plasma panel, wlroots bar).

## 2. Candidate pure-Rust SNI crates (checked 2026-10-08)

| Crate | Version / last release | Maint. | Notes |
|---|---|---|---|
| `ksni` | **0.3.6, 2026-09-27** (11 days ago; 0.3.3 was 2025-12-19, steady cadence) | Active (`iovxw/ksni`); ~1.29M downloads; 59 reverse deps incl. `tray-icon`, Slint (`i-slint-core`), songrec | Pure Rust (zbus), no system libs. **Tokio by default** — matches Wayterm (tokio full already a dep). `Tray` trait + `TrayMethods::spawn().await` (+ `TrayService` builder, blocking API behind feature). Menu via `MenuItem`/`StandardItem` with `FnMut(&mut Self)` activate closures. Sources: <https://crates.io/crates/ksni>, <https://docs.rs/ksni/0.3.6/ksni/trait.Tray.html> |
| `tray-item` | 0.10.0, **2024-04-07** (stale, >2y) | Thin wrapper whose Linux backends *are* ksni or libappindicator-gtk3 | Adds no capability over ksni directly; extra layer, stale. <https://crates.io/crates/tray-item> |
| `tray-icon` (tauri-apps) | 0.26.1, active | Cross-platform (muda menu model), own event loop | Overkill for Linux-only v1; heavier menu/event model; MSRV 1.90. Useful if we ever go cross-platform. <https://crates.io/crates/tray-icon> |
| `libappindicator` bindings | 0.9.0, **2023-10-01** (legacy) | GTK3 system lib required | Rejected: GTK3 in a GTK4 process. <https://crates.io/crates/libappindicator> |
| `rustsni` | new (host-side lib: implements Watcher+Host for writing panels) | Wrong side of the protocol | We are an *item*, not a host. Noted only to avoid confusion. <https://docs.rs/rustsni> |

**Chosen: `ksni` 0.3 directly** (not via `tray-item`). Rationale: actively
maintained (release 11 days before this writing), tokio-native (zero new
async runtime; Wayterm already depends on tokio 1/full), pure Rust over
zbus (no new system dependencies — important for CI and Flatpak), most
adopted (reverse deps prove the API surface is exercised), and no GTK
version coupling (it's D-Bus only, so no GTK3/GTK4 conflict by
construction).

## 3. GTK4 main-loop integration cost

ksni owns no GTK objects; integration is one bridge in each direction:

- **Tray → GTK (menu clicks, left-click activate):** ksni invokes
  `StandardItem` activate closures on its async tasks, while GTK objects
  (`gtk4::Application` is `!Send`) may only be touched on the main
  thread. Bridge as implemented: a std mpsc queue plus
  `glib::MainContext::invoke` (thread-safe, `FnOnce() + Send`); `fire`
  queues the action and invokes a capture-free drain closure that runs on
  the main thread, where the app handle + receiver live in a
  main-thread-local. Two earlier designs were rejected during
  implementation: holding an `Application` in the `Tray` struct (fails
  `Tray: Send` — `gtk4::Application` is `!Send` in gtk-rs) and an
  fd-source drain (`glib::unix_fd_add_local` and `gio::UnixInputStream`
  no longer exist in glib 0.22 / gio 0.22). Cost: ~30 lines, std + glib
  only, no channels crate, no polling.
- **GTK → tray (spawn, lifetime):** `spawn()` is async and needs a Tokio
  runtime, but `build_ui` runs on the GTK main thread with no runtime.
  Pattern: `init_tray` builds a small dedicated
  `tokio::runtime::Runtime` (1 worker) on the calling thread and
  `block_on`s `spawn()` once (a D-Bus handshake — fast fail/success). The
  returned `ksni::Handle` + `Runtime` are kept alive in a `TrayHandle`
  stashed on the window (`set_data`, same pattern as the status bar),
  so the D-Bus service outlives the init call. The runtime is otherwise
  idle; ksni drives its own I/O on it.
- **Close-to-tray:** `connect_close_request` on the window (installed from
  `app.rs`, NOT in `main_window.rs` — that file is owned by another task).
  If tray init succeeded AND `minimize_to_tray` is on: `hide()` +
  `Propagation::Stop`. Otherwise `Propagation::Proceed` (existing
  quit-on-close behavior preserved). Hidden-but-alive window keeps the
  `GtkApplication` running — no `hold()` bookkeeping needed.

## 4. Notification story

Candidates: `notify-rust` 4.18.2 (active, pure Rust, zbus-based;
<https://crates.io/crates/notify-rust>) vs `gio::Notification` +
`GApplication::send_notification` (already in tree: `gio` is a direct
dep).

**Chosen: `gio::Notification`.** Zero new dependencies; app-id-scoped
notification ids (`wayterm-disconnect-<session>`); works under Flatpak
via the portal path that `GApplication` notifications take; no extra
D-Bus connection (shares the app's). `notify-rust` would duplicate all
of this for no Phase-4 benefit. Disconnect source: the transport's
`TransportEvent::ConnectionLost` (doc 04) via `TerminalTab::take_state_rx`
(doc 04 §Wiring "second glib channel"); `tray.rs` provides the
display-free triage (`ConnectionLost` → alert, everything else → ignore)
plus an async forwarder, and the one-line gio send on the main thread.
Full per-tab wiring is a later step; the seam is implemented and tested,
the display-bound attach is doc-commented.

## 5. Fallback / graceful degradation (headless CI is a first-class case)

- **No session bus / D-Bus down:** `spawn()` errors → `init_tray`
  returns `Err(TrayInitError)` → app keeps default quit-on-close.
  `should_minimize_to_tray(tray_available=false, ..)` is `false`
  regardless of the setting. Covered by unit test.
- **Bus up, no Watcher/Host** (plain Xvfb, GNOME without the extension):
  verified live: ksni's `spawn()` fails fast with
  `org.freedesktop.DBus.Error.ServiceUnknown` for
  `org.kde.StatusNotifierWatcher`, so `init_tray` returns `Err` and the
  app keeps quit-on-close. No crash in any branch — the xvfb smoke test
  (`xvfb-run cargo run`, expect 124 timeout-kill, no panic) pins this,
  and the stderr line `Wayterm: tray unavailable (...)` proves the
  fallback branch was taken.
- **Icon:** freedesktop name `utilities-terminal` (always in hicolor);
  no Wayterm icon file needed for Phase 4. Installing
  `com.alex-eckstein.wayterm` icons is packaging work (Phase 9).

## 6. What this deliberately does NOT do (no scope creep)

- No `minimize_to_tray` seed-row migration: the 10-row seed in `db.rs` /
  doc 06 is pinned by tests; the setting is fail-open (missing row = ON)
  via the new generic `get/set_setting` accessor. A seed/defaults change
  is a schema-adjacent decision for the settings-UI owner.
- No per-tab live wiring in `main_window.rs` (owned by another task):
  the disconnect forwarder + gio sender are provided and unit-tested;
  attachment to live tabs is a documented one-liner follow-up.
- No libappindicator fallback, no XEmbed, no Windows/macOS tray.
