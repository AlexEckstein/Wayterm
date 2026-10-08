# Backend Plugins

Terminal backends are chosen behind `create_terminal()` (doc 02). Today
VTE is the only one, so there is nothing to select.

## Compile-time backends (when a second one exists)

Each additional backend becomes a Cargo feature (e.g. `backend-ghostty`)
and a match arm in `create_terminal`, keyed by the global `default_backend`
setting or a session's `terminal_backend` override (doc 06), with VTE as
the fallback. This keeps dispatch type-safe and needs no `dlopen`.
Distributions build with the features they want (Flatpak/AppImage bundle
all compiled-in backends; Debian could ship one as a separate package).

## Runtime plugins (only on demand)

If third parties ever want to ship a backend without recompiling Wayterm,
wrap the trait in a C ABI (`terminal_plugin_name`, `_features`, `_create`,
`_write`, `_destroy`, `_available`, …) and load `.so` files with
`libloading`:

- only from fixed directories (`/usr/lib/wayterm/plugins/`,
  `~/.local/share/wayterm/plugins/`), never arbitrary paths;
- plugins see terminal bytes, never credentials;
- keep the `Library` loaded while any instance exists;
- plugin calls happen on the GTK main thread.

The trait already supports this; nothing needs to change to add it later.
