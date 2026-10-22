# Settings

The Settings window, every settings row the app reads or writes, and how
the window chrome follows the desktop's light or dark theme.

Code: `src/ui/settings.rs` (the window) and `src/ui/settings/` (one module
per page), `src/store/db.rs` (`settings` table, seed, boolean rows),
`src/ui/appearance.rs` (theme following).

## Settings window

The header bar's **Settings** button opens it: a sidebar of pages beside
the selected one. Every page saves each change at once and applies it
live; there is no Apply button.

### General

`src/ui/settings/general.rs`:

| Control | Row | Applies |
|---|---|---|
| Confirm before closing a connected tab | `confirm_close` | at the next close, open tabs included (doc 03 §Closing a tab) |
| Reconnect automatically | `reconnect_on_drop` | at the next drop, open tabs included (doc 03 §Reconnect) |
| Open SFTP on connect | `sftp_on_connect` | to tabs connected afterwards (doc 07 §Connection model) |
| Minimize to tray on close | `minimize_to_tray` | at each window close (doc 03 §Tray) |
| Experimental Ghostty terminal | `default_backend` (`ghostty`/`vte`) | to tabs opened afterwards; disabled with a "rebuild with the backend-ghostty feature to enable" tooltip in a default build (doc 02 §Backend selection) |
| Default color scheme | `default_color_scheme` (`System`/`Light`/`Dark`) | to every open terminal at once (doc 02 §Color schemes) |
| Clear all reminders | writes `1` to `duplicate_tmux_notice` and `confirm_close_notice` | every "Do not show me again" notice shows again; greyed out while none is silenced |

Each switch writes `1`/`0`. The stored values are pushed to the modules
that act on them at startup and after every change.

### Credentials

The Bitwarden switch and CLI path, and whether the keyring and the CLI
answer (doc 05 §Settings).

### Keyboard Shortcuts

Every action with its binding, Capture and Reset (doc 11 §Settings page).

### Backup / Restore

Import from other tools and export to JSON (doc 10 §Backup / Restore
page). It opens by itself once on a first start with an empty store.

## Settings rows

The `settings` table (doc 09 §Schema) is a key-value store. The seed runs
once, inside a transaction, when the table is empty; `hotkeys` has no
`UNIQUE (action, context)`, so re-running it would duplicate rows. It also
inserts 28 informational hotkey rows (doc 11 §Persistence).

| Row | Seed | Meaning |
|---|---|---|
| `default_backend` | `vte` | terminal backend for new tabs (doc 02 §Backend selection) |
| `default_color_scheme` | `System` | `System`, `Light` or `Dark` (doc 02 §Color schemes) |
| `default_font_family` / `default_font_size` | `Monospace` / `12.0` | not applied (doc 02 §Limitations) |
| `default_opacity` | `1.0` | not applied |
| `scrollback_limit` | `10000` | not applied |
| `confirm_close` | `1` | ask before closing a live tab |
| `reconnect_on_drop` | `1` | reconnect automatically vs ask each attempt |
| `tmux_default` | `0` | not read |
| `host_key_policy` | `strict` | global host-key policy (doc 04 §Host key verification) |
| `minimize_to_tray` | — | missing = on |
| `sftp_on_connect` | — | missing = on; off defers the SFTP channel until first viewed |
| `duplicate_tmux_notice` | — | missing = on; off after "Do not show me again" on the duplicate-tab tmux notice |
| `confirm_close_notice` | — | missing = on; off after closing with "Do not show me again" ticked |
| `bitwarden.enabled` | — | missing = **off** (doc 05 §Settings) |
| `bitwarden.bin` | — | missing or blank = `bw` |
| `import_offered` | — | set once the first-run import page was shown |
| `open_tabs` | — | JSON tab list, written at runtime (doc 03 §Restoring tabs) |

**Boolean rows** are on unless explicitly `0`/`false`/`no`/`off`: a
missing or unreadable row is on, so a typo never disables a safety prompt.
`bitwarden.enabled` is the exception — off unless `1`/`true`/`yes`/`on` —
because running an external CLI on every connect must be opt-in.

The seed applies to new stores only. An existing store keeps its rows (a
store created before `System` existed keeps `Dark`).

## Window theme

The window chrome follows the same light/dark preference as the
terminals' `System` scheme (doc 02 §Color schemes), from
xdg-desktop-portal's `color-scheme`, read at start and followed through
`SettingChanged`:

- On GTK 4.20+ `ui::appearance` sets `gtk-interface-color-scheme` and
  resets `gtk-application-prefer-dark-theme`, because GTK only reloads the
  theme when the latter or the theme name notifies, and it warns when the
  latter is set. Older GTK sets `gtk-application-prefer-dark-theme`.
- When the portal withdraws its preference, both settings go back to GTK.
- KDE recolors GTK apps by rewriting the `colors.css` that
  `$XDG_CONFIG_HOME/gtk-4.0/gtk.css` imports, and GTK loads that file once.
  So `ui::appearance` watches the directory and, once it has been quiet
  for 200 ms (a theme switch rewrites several files), reloads `gtk.css`
  into its own user-priority style provider. The provider is added on the
  first change only, so an unchanged file is never applied twice.

## Limitations

- `tmux_default` is seeded but never read: new sessions start with tmux
  off.
- The font, opacity and scrollback rows are not applied (doc 02
  §Limitations).
- The global `host_key_policy` has no control in the window; it can only
  be changed in the database.
