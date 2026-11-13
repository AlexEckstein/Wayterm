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

- compiles `assets/tree-icons/` (the session-tree, side-dock and SFTP
  sidebar icons, doc 09 §Icons) and `assets/icons/` (the app icon, and
  the mark the About page draws) into GResource bundles that `ui::icons`
  embeds in the binary;
- sets `WAYTERM_GIT_COMMIT` to the checkout's short commit hash for the
  About page (doc 13 §About); a build outside a git checkout, or without
  git, leaves it unset;
- with the `backend-ghostty` feature only, compiles the libghostty-vt shim
  and links the pinned static library (doc 02 §libghostty-vt pin).

## Cargo features

| Feature | Default | What |
|---|---|---|
| `backend-ghostty` | off | the libghostty-vt terminal backend (doc 02 §GhosttyBackend); needs the library built locally; enables the `terminal_parse` benchmark |

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
builds the AppImage from it with a pkg2appimage pinned by checksum, and
attaches both to the GitHub Release for that tag. A manual run
(`workflow_dispatch`) builds the artifacts without releasing. The
workflow's token is read-only; only the job that publishes, which runs no
build, gets one that can write. The `deb` job fails before building
unless the tag matches `Cargo.toml`'s `version`, and the Release's notes
are generated from the merged PRs, then edited to lead with what the
release is, how to install it, and its limitations.

A release goes out like this:

1. main is green on CI for the commit being released.
2. The release commit sets `version` in `Cargo.toml` (SemVer: minor for
   features, patch for fixes) and adds a `<release>` entry to the
   metainfo with the day's date and a short description.
3. A `workflow_dispatch` run builds the artifacts without releasing, and
   they are smoke-tested before anything is published: the `.deb` is
   checked with `dpkg-deb`, installed into a clean Debian container,
   and its binary launched under Xvfb; the AppImage is extracted and its
   contents inspected, not run — it needs a newer glibc than the smoke
   host (§Limitations).
4. The annotated tag `v<version>` is pushed; a published tag is never
   moved.
5. The workflow publishes the Release, the notes get their lead
   paragraph, and the assets are verified from the published release.

## Limitations

- The Flatpak build fetches crates over the network
  (`--share=network` at build time) — fine for local builds, not for
  Flathub, which needs vendored cargo sources. The release workflow does
  not build it, and its sandbox access to SSH keys, the keyring and files
  is not verified, nor is handing a file opened from the SFTP sidebar to
  another application (doc 07 §Opening files).
- Packages are built without `backend-ghostty`.
- The AppImage and `.deb` target x86_64 only.
- The AppImage bundles GTK 4 and GLib from Ubuntu 26.04, which need
  glibc 2.42 (the highest version symbol its bundled libraries ask for;
  the image's name carries it), so it does not run on Ubuntu 24.04 or
  Debian 13. The `.deb` uses the system's libraries and needs only
  what its `Depends` names (libc6 2.39 at 0.1.0).
- The recipe names its archive (`resolute`) while the workflow follows
  `ubuntu-latest`: when GitHub moves `ubuntu-latest` to a newer Ubuntu,
  the recipe must move with it.
