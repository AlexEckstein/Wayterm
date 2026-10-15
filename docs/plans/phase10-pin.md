# libghostty-vt pin (Phase 10)

The `backend-ghostty` feature (doc 02 §Engine backends, doc 05) links
libghostty-vt from one pinned upstream commit. libghostty-vt has no tagged
release and its C API still moves, so the pin only changes in a deliberate
bump: rebuild, rerun the gates below, update this file and the comment in
`build.rs` together.

| | |
|---|---|
| Upstream | <https://github.com/ghostty-org/ghostty> |
| Commit | `22fa134a07f5749edd9e92c6bc17d8aaf61843be` |
| Commit date | 2026-10-15 08:22:33 UTC (`main` HEAD when pinned) |
| Pinned | 2026-10-15 |
| Zig | 0.16.0 (`minimum_zig_version` in upstream `build.zig.zon`) |
| Built on | Debian 13, x86_64, kernel 6.12 |
| Library version | `0.1.0-dev` (upstream pkg-config) |

## Building it

Outside the Wayterm checkout:

```sh
# Zig 0.16.0, user-local (sha256 from ziglang.org/download/index.json:
# 70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00)
curl -LO https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz
tar -xJf zig-x86_64-linux-0.16.0.tar.xz -C ~/.local/zig
ln -s ~/.local/zig/zig-x86_64-linux-0.16.0/zig ~/.local/bin/zig

git clone https://github.com/ghostty-org/ghostty.git ghostty-build
cd ghostty-build
git checkout --detach 22fa134a07f5749edd9e92c6bc17d8aaf61843be
zig build -Demit-lib-vt -Doptimize=ReleaseFast --prefix "$PWD/out"
```

That takes about 1.5 minutes on 10 cores. `zig build` fetches the
upstream build's own dependencies into `~/.cache/zig/p` (hash-pinned in
upstream `build.zig.zon`: uucode, translate-c, aro, and five tarballs from
`deps.files.ghostty.org`); nothing else is downloaded.

`out/` then holds `include/ghostty/vt.h` (+ `vt/`), `lib/libghostty-vt.a`,
and a shared library Wayterm does not use.

## Linking it into Wayterm

`build.rs` looks for the headers and the static library under
`$WAYTERM_GHOSTTY_VT_DIR`, default `third_party/libghostty-vt/` in the
checkout:

```sh
mkdir -p third_party/libghostty-vt/lib
cp -r ghostty-build/out/include third_party/libghostty-vt/
cp ghostty-build/out/lib/libghostty-vt.a third_party/libghostty-vt/lib/
printf '*\n' > third_party/libghostty-vt/.gitignore   # never committed
cargo test --features backend-ghostty
```

The library is not committed (17 MB, built per machine). Without it, a
build with the feature fails in `build.rs` with a pointer to this file; the
default build never looks for it. `build.rs` compiles
`src/terminal/ghostty/shim.c` against the headers with the `cc` crate (an
optional build dependency enabled only by the feature) and links
`libghostty-vt.a` statically; no system libraries beyond libc are needed.

## Verified with this pin

- `cargo clippy --all-targets --features backend-ghostty -- -D warnings`
- `cargo test --features backend-ghostty` — the engine unit tests in
  `src/terminal/ghostty/engine.rs` exercise the FFI end to end.
- `env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a cargo test
  --features backend-ghostty -- --ignored ghostty_display` — the render
  widget under xvfb.
- `cargo bench --features backend-ghostty` — doc 03 §Benchmark.
