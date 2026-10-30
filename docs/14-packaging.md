# Building and Packaging

How Wayterm is built, which Cargo features exist, and how the `.deb`,
AppImage and Flatpak are made and released.

## Build

Needs Rust (stable), GTK 4, VTE for GTK 4 (≥ 0.72 at runtime), and
`glib-compile-resources` from the GLib dev tools. On Debian/Ubuntu:
`libgtk-4-dev libvte-2.91-gtk4-dev libsqlite3-dev` (the first pulls in
`libglib2.0-dev-bin`, which has the tool). SQLite is bundled by
`rusqlite`, so no system library is linked for it.

```sh
cargo run
cargo build --release
```

`build.rs`:

- compiles `assets/tree-icons/` (the session-tree and side-dock icons,
  doc 09 §Icons) and `assets/icons/` (the app icon) into GResource bundles
  that `ui::icons` embeds in the binary;
- with the `backend-ghostty` feature only, compiles the libghostty-vt shim
  and links the pinned static library (doc 02 §libghostty-vt pin).

## Cargo features

| Feature | Default | What |
|---|---|---|
| `backend-ghostty` | off | the experimental libghostty-vt terminal backend (doc 02 §GhosttyBackend); needs the library built locally; enables the `terminal_parse` benchmark |

The default build has no optional native dependencies beyond GTK 4 and
VTE.

## App icon

The app icon (`assets/icons/`: an SVG plus 64, 128 and 256 px PNGs) is
installed into the hicolor theme by every package. It is also embedded as
a GResource and set as the default window icon, so the title bar shows it
even when Wayterm is not installed; the tray sends the 64 px PNG as pixels
for the same reason (doc 03 §Tray).

The desktop entry (`packaging/desktop/com.alex-eckstein.wayterm.desktop`)
and AppStream metainfo (`packaging/metainfo/…appdata.xml`) use the app id
`com.alex-eckstein.wayterm`, which is also baked into the keyring schema
and data paths (doc 05 §Keyring).

## Packages

### .deb

`cargo deb` (metadata under `[package.metadata.deb]` in `Cargo.toml`,
output in `target/debian/`) installs the binary, the desktop entry, the
metainfo and the icons.

### AppImage

`packaging/appimage/wayterm.yml` is a pkg2appimage recipe built from the
`.deb`: pkg2appimage resolves the runtime libraries (GTK 4, VTE) from the
Ubuntu resolute (26.04) archive, the release the `.deb` is built on, and
the recipe copies the desktop file and the 256 px icon to the AppImage
root.

```sh
WAYTERM_DEB=$PWD/target/debian/wayterm_<ver>_amd64.deb \
  ./pkg2appimage packaging/appimage/wayterm.yml
```

### Flatpak

`packaging/flatpak/com.alex-eckstein.wayterm.json`: GNOME runtime 47 with
the Rust SDK extension, VTE 0.78 built for GTK 4 as a module, then
Wayterm.

```sh
flatpak-builder --user --install build-dir packaging/flatpak/com.alex-eckstein.wayterm.json
```

Sandbox permissions: network, Wayland with X11 fallback (RDP embedding
needs X11, doc 08 §Wayland), DRI, PulseAudio, the Secret Service
(`org.freedesktop.secrets`), and the home directory (SSH keys and config,
file transfers). Inside the sandbox the keyring source uses the portal
(doc 05 §Keyring).

## Release

`.github/workflows/release.yml` runs on a `v*` tag push: it builds the
`.deb` with `cargo-deb` on `ubuntu-latest` (Ubuntu 26.04, as CI uses),
builds the AppImage from it with a pinned pkg2appimage, and attaches both
to the GitHub Release for that tag. A manual run (`workflow_dispatch`)
builds the artifacts without releasing.

## Limitations

- The Flatpak build fetches crates over the network
  (`--share=network` at build time) — fine for local builds, not for
  Flathub, which needs vendored cargo sources. The release workflow does
  not build it, and its sandbox access to SSH keys, the keyring and files
  is not verified.
- Packages are built without `backend-ghostty`.
- The AppImage and `.deb` target x86_64 only.
- The AppImage bundles GTK 4 and GLib from Ubuntu 26.04, which need
  glibc 2.43, so it does not run on Ubuntu 24.04 or Debian 13. The
  `.deb` uses the system's libraries and needs only what its `Depends`
  names (libc6 2.39 at 0.1.0).
- The recipe names its archive (`resolute`) while the workflow follows
  `ubuntu-latest`: when GitHub moves `ubuntu-latest` to a newer Ubuntu,
  the recipe must move with it.
