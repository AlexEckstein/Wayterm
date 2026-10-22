//! Compiles the built-in tree icons (`assets/tree-icons`, doc 09 §Icons)
//! and the app icon (`assets/icons`, doc 14 §App icon) into GResource
//! bundles that `ui::icons` embeds, and — with the `backend-ghostty` feature only —
//! links libghostty-vt (doc 02).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    compile_resources("assets/tree-icons", "tree-icons");
    compile_resources("assets/icons", "app-icon");

    #[cfg(feature = "backend-ghostty")]
    ghostty::link();
}

/// `<source>/<name>.gresource.xml` → `$OUT_DIR/<name>.gresource`.
fn compile_resources(source: &str, name: &str) {
    println!("cargo:rerun-if-changed={source}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let status = Command::new("glib-compile-resources")
        .arg(format!("--sourcedir={source}"))
        .arg(format!(
            "--target={}",
            out.join(format!("{name}.gresource")).display()
        ))
        .arg(format!("{source}/{name}.gresource.xml"))
        .status()
        .expect("glib-compile-resources not found (it ships with the GLib dev tools)");
    assert!(status.success(), "glib-compile-resources failed for {name}");
}

/// libghostty-vt for the `backend-ghostty` feature (doc 02
/// §GhosttyBackend).
///
/// Pinned upstream: ghostty-org/ghostty @ 22fa134a07f5749edd9e92c6bc17d8aaf61843be
/// (2026-10-15), built with Zig 0.16.0 — how to rebuild it is in
/// doc 02 §libghostty-vt pin. Bump the pin deliberately, never silently.
///
/// The static library and headers are looked up in
/// `$WAYTERM_GHOSTTY_VT_DIR` (default `third_party/libghostty-vt`):
/// `include/ghostty/vt.h` and `lib/libghostty-vt.a`. They are built
/// locally, not committed.
#[cfg(feature = "backend-ghostty")]
mod ghostty {
    use std::path::PathBuf;

    /// The C shim compiled against the headers.
    const SHIM: &str = "src/terminal/ghostty/shim.c";

    /// Compile the shim and link `libghostty-vt.a`, or fail with the build steps.
    pub fn link() {
        println!("cargo:rerun-if-env-changed=WAYTERM_GHOSTTY_VT_DIR");
        println!("cargo:rerun-if-changed={SHIM}");
        let root = match std::env::var_os("WAYTERM_GHOSTTY_VT_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .join("third_party/libghostty-vt"),
        };
        let include = root.join("include");
        let lib = root.join("lib");
        assert!(
            include.join("ghostty/vt.h").is_file() && lib.join("libghostty-vt.a").is_file(),
            "backend-ghostty: no libghostty-vt under {} — build it as described in \
             docs/02-terminal.md (libghostty-vt pin), or set WAYTERM_GHOSTTY_VT_DIR",
            root.display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            lib.join("libghostty-vt.a").display()
        );

        cc::Build::new()
            .file(SHIM)
            .include(&include)
            .warnings(true)
            .compile("wayterm_ghostty_shim");
        println!("cargo:rustc-link-search=native={}", lib.display());
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}
