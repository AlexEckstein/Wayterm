//! Compiles the built-in tree icons (`assets/tree-icons`, doc 09 §Icons)
//! and the app icon (`assets/icons`, doc 14 §App icon) into GResource
//! bundles that `ui::icons` embeds, records the git commit for the About
//! page (doc 13 §About), and — with the `backend-ghostty` feature only —
//! links libghostty-vt (doc 02).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    compile_resources("assets/tree-icons", "tree-icons");
    compile_resources("assets/icons", "app-icon");
    git_commit();

    #[cfg(feature = "backend-ghostty")]
    ghostty::link();
}

/// Set `WAYTERM_GIT_COMMIT` to the checkout's short commit hash. Outside a
/// git checkout, or without git, it stays unset.
fn git_commit() {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        (out.status.success() && !text.trim().is_empty()).then(|| text.trim().to_string())
    };
    let Some(commit) = git(&["rev-parse", "--short", "HEAD"]) else {
        return;
    };
    println!("cargo:rustc-env=WAYTERM_GIT_COMMIT={commit}");
    // HEAD moves on a checkout, the branch ref on a commit. Cargo reruns
    // every build for a missing path, so only existing ones are watched.
    let mut refs = vec!["HEAD".to_string(), "packed-refs".to_string()];
    refs.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for name in refs {
        if let Some(path) = git(&["rev-parse", "--git-path", &name])
            && PathBuf::from(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
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
/// Pinned upstream: libghostty-vt 1.0.0 (ghostty-org/ghostty tag `vt-1.0.0`,
/// 2026-11-06), source tarball sha256
/// 4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676, built
/// with Zig 0.17.0 — how to rebuild it is in doc 02 §libghostty-vt pin.
/// Bump the pin deliberately, never silently.
///
/// The static library and headers are looked up in
/// `$WAYTERM_GHOSTTY_VT_DIR` (default `third_party/libghostty-vt`):
/// `include/ghostty/vt.h` and `lib/libghostty-vt.a`. They are built
/// locally, not committed.
#[cfg(feature = "backend-ghostty")]
mod ghostty {
    use std::path::PathBuf;

    const SHIM: &str = "src/terminal/ghostty/shim.c";

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
