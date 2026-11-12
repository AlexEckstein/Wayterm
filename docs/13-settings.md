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
| Open SFTP on connect | `sftp_on_connect` | to SSH tabs connected afterwards (doc 07 §Connection model); local tabs never open it (doc 07 §Local tabs) |
| Minimize to tray on close | `minimize_to_tray` | at each window close (doc 03 §Tray) |
| Experimental Ghostty terminal | `default_backend` (`ghostty`/`vte`) | to tabs opened afterwards, for sessions without their own; disabled with a "rebuild with the backend-ghostty feature to enable" tooltip in a default build (doc 02 §Backend selection) |
| Clear all reminders | writes `1` to `duplicate_tmux_notice` and `confirm_close_notice` | every "Do not show me again" notice shows again; greyed out while none is silenced |

Each switch writes `1`/`0`. The stored values are pushed to the modules
that act on them at startup and after every change.

### Customize

`src/ui/settings/customize.rs`, with `customize/rows.rs` building the
controls. Terminal comes first, for both backends:

| Control | Row | Applies |
|---|---|---|
| Font | `default_font_family` | to every open terminal without its own (doc 02 §Font, opacity and scrollback) |
| Font size | `default_font_size` | as Font, once settled |
| Background opacity | `default_opacity` | as Font, once settled (doc 02 §Opacity) |
| Scrollback lines | `scrollback_limit` | to every open terminal, once settled |

Then a section per backend (`customize/vte.rs`, `customize/ghostty.rs`),
whose rows reach only that backend's tabs (doc 02 §View options). Ghostty's
is shown in a build with the backend, whatever `default_backend` says, since
a session can name its own.

| VTE control | Row |
|---|---|
| Color scheme | `vte.color_scheme` (`System`/`Light`/`Dark`) |
| Background color, Text color | `vte.background`, `vte.foreground` |
| Cursor shape | `vte.cursor_shape` (`Block`/`I-beam`/`Underline`) |
| Cursor blink | `vte.cursor_blink` (`System`/`On`/`Off`) |
| Audible bell | `vte.audible_bell` |
| Selection color, Selection text color | `vte.selection_bg`, `vte.selection_fg` |
| Line height | `vte.line_height` (1.0–2.0) |
| Bold text is bright | `vte.bold_is_bright` |
| Hide mouse while typing | `vte.hide_mouse` |

| Ghostty control | Row |
|---|---|
| Color scheme | `ghostty.color_scheme` (`System`/`Light`/`Dark`) |
| Background color, Text color | `ghostty.background`, `ghostty.foreground` |
| Cursor style | `ghostty.cursor_style` (`Block`/`Bar`/`Underline`/`Hollow`) |
| Blinking cursor | `ghostty.cursor_blinks` |
| Cursor blink period, Text blink period | `ghostty.cursor_blink_ms`, `ghostty.text_blink_ms` (50–5000) |
| Mouse wheel speed | `ghostty.wheel_rows` (0.5–30) |
| Selection color, Selection text color | `ghostty.selection_bg`, `ghostty.selection_fg` |
| Search match color, Current match color, Search text color | `ghostty.search_bg`, `ghostty.search_current_bg`, `ghostty.search_fg` |
| Kitty image storage | `ghostty.image_limit_mb` (0–320) |

Each reaches open terminals at once, numbers once settled. Switches write
`1`/`0` and colors `#rrggbb`; a color's Reset blanks its row, which reads as
not set. A scheme's row reads through to `default_color_scheme` while it is
missing (doc 02 §Color schemes).

The font picker lists monospace families and sets the family only: a
terminal's rows hold no face, so Bold could not be kept. Numeric controls
save 150 ms after their last change, because each save re-applies to every
tab and a size change resizes every remote's grid. The mouse wheel over them
scrolls the page and leaves the value alone, since a lowered scrollback
drops lines from open tabs.

### Credentials

The Bitwarden switch and CLI path, and whether the keyring and the CLI
answer (doc 05 §Settings).

### Keyboard Shortcuts

Every action with its binding, Capture and Reset (doc 11 §Settings page).

### Backup / Restore

Import from other tools and export to JSON (doc 10 §Backup / Restore
page). It opens by itself once on a first start with an empty store.

### About

`src/ui/settings/about.rs`, read-only. The Wayterm mark
(`assets/icons/scalable/apps/wayterm-mark-symbolic.svg`, embedded with the
app icon) in the theme's text color, with the name under it, then:

| Row | Shows |
|---|---|
| Wayterm | the crate version |
| Commit | the short commit hash `build.rs` recorded (doc 14 §Build); left out of a build made outside a git checkout |
| libghostty-vt | the linked library's version (doc 02 §libghostty-vt pin), or "Not included in this build" without the `backend-ghostty` feature |
| VTE | the loaded library's version |
| GTK | the loaded library's version |

The values can be selected and copied. A Project section links the
repository and names the license, both from `Cargo.toml`.

The mark is a copy of `wayterm_mark.svg` without its invisible
background rectangle, because GTK fills every shape of a `-symbolic` icon.
The name is a label, not the wordmark `wayterm_full.svg`: that file's
word is live text in a font few systems have.

## Settings rows

The `settings` table (doc 09 §Schema) is a key-value store. The seed runs
once, inside a transaction, when the table is empty; `hotkeys` has no
`UNIQUE (action, context)`, so re-running it would duplicate rows. It also
inserts 28 informational hotkey rows (doc 11 §Persistence).

| Row | Seed | Meaning |
|---|---|---|
| `default_backend` | `vte` | terminal backend for new tabs of sessions without their own (doc 02 §Backend selection) |
| `default_color_scheme` | `System` | `System`, `Light` or `Dark`, for a backend without its own scheme; no control writes it (doc 02 §Color schemes) |
| `default_font_family` / `default_font_size` | `Monospace` / `12.0` | terminal font, in points, for sessions without their own (doc 02 §Font, opacity and scrollback) |
| `default_opacity` | `1.0` | terminal background opacity, 0–1, for sessions without their own (doc 02 §Opacity) |
| `scrollback_limit` | `10000` | lines each terminal keeps, 0 for none |
| `vte.*`, `ghostty.*` | — | one backend's options (§Customize); missing or unusable = that option's default |
| `confirm_close` | `1` | ask before closing a live tab |
| `reconnect_on_drop` | `1` | reconnect automatically vs ask each attempt |
| `tmux_default` | `0` | not read |
| `host_key_policy` | `strict` | global host-key policy (doc 04 §Host key verification) |
| `minimize_to_tray` | — | missing = on |
| `sftp_on_connect` | — | missing = on; off defers the SFTP channel until first viewed |
| `sftp_columns` | — | the SFTP listing's optional columns that are shown, e.g. `size,modified`; missing = `size,modified,permissions`; written by the listing's header menu, no Settings control (doc 07 §Listing) |
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
- The global `host_key_policy` has no control in the window; it can only
  be changed in the database.
