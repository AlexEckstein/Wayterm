//! Compiles the built-in tree icons (`assets/tree-icons`, doc 09 §Icons)
//! and the app icon (`assets/icons`, doc 14 §App icon) into GResource
//! bundles that `ui::icons` embeds, records the git commit for the About
//! page (doc 13 §About), and — with the `backend-ghostty` feature, on by
//! default — builds and links libghostty-vt (doc 02).

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
/// §libghostty-vt pin): fetched, checked and built here at the pin, once
/// per machine, unless `$WAYTERM_GHOSTTY_VT_DIR` names a library built
/// elsewhere.
#[cfg(feature = "backend-ghostty")]
mod ghostty {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::{env, fs};

    /// The pin: libghostty-vt 1.0.0 (ghostty-org/ghostty tag `vt-1.0.0`,
    /// 2026-11-06). Both checksums are from the release's signed
    /// `SHA256SUMS`. Bump the pin deliberately, never silently, and doc 02
    /// with it.
    const VERSION: &str = "1.0.0";
    const SOURCE_SHA256: &str = "4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676";
    const DEPS_SHA256: &str = "4d5ea5e448e4c7ea8fb55ccbaa8668f2a97d44f438cee831e2b7ecd162d85c29";
    const RELEASES: &str = "https://release.files.ghostty.org/libghostty-vt";

    /// The Zig this release builds with, and the checksum of its x86_64
    /// Linux tarball (ziglang.org/download/index.json).
    const ZIG_VERSION: &str = "0.17.0";
    const ZIG_HOST: &str = "x86_64-linux";
    const ZIG_SHA256: &str = "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026";

    /// Zig builds for the CPU it runs on unless told otherwise, and such a
    /// library dies with SIGILL on an older one. `baseline` runs wherever
    /// the binary does.
    const CPU: &str = "baseline";

    const SHIM: &str = "src/terminal/ghostty/shim.c";
    const HEADER: &str = "include/ghostty/vt.h";
    const LIBRARY: &str = "lib/libghostty-vt.a";

    /// Ends every failure: the two ways to build without this step.
    const WAYS_OUT: &str = "To build without the Ghostty backend, pass --no-default-features; \
         to link a libghostty-vt built elsewhere, set WAYTERM_GHOSTTY_VT_DIR \
         (docs/02-terminal.md, libghostty-vt pin).";

    pub fn link() {
        println!("cargo:rerun-if-env-changed=WAYTERM_GHOSTTY_VT_DIR");
        println!("cargo:rerun-if-changed={SHIM}");
        let root = match env::var_os("WAYTERM_GHOSTTY_VT_DIR") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                assert!(
                    is_built(&dir),
                    "WAYTERM_GHOSTTY_VT_DIR: no {HEADER} and {LIBRARY} under {}",
                    dir.display()
                );
                dir
            }
            None => pinned(),
        };
        // A deleted library reruns this script, which builds it again.
        println!("cargo:rerun-if-changed={}", root.join(LIBRARY).display());

        cc::Build::new()
            .file(SHIM)
            .include(root.join("include"))
            .warnings(true)
            .compile("wayterm_ghostty_shim");
        println!(
            "cargo:rustc-link-search=native={}",
            root.join("lib").display()
        );
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }

    fn is_built(root: &Path) -> bool {
        root.join(HEADER).is_file() && root.join(LIBRARY).is_file()
    }

    /// The pinned library's directory, building it first if this machine
    /// has not yet. The name carries everything the build varies with, so
    /// a bump builds into a new directory.
    fn pinned() -> PathBuf {
        let arch = env::consts::ARCH;
        let name = format!(
            "{VERSION}-{}-zig-{ZIG_VERSION}-{CPU}-{arch}",
            &SOURCE_SHA256[..12]
        );
        let root = cache_dir().join(name);
        if !is_built(&root) {
            build(&root);
        }
        root
    }

    /// `$XDG_CACHE_HOME/wayterm-build/libghostty-vt` (default under
    /// `~/.cache`), shared by every checkout and profile on the machine;
    /// `$OUT_DIR` where that cannot be made, as in a build sandbox.
    fn cache_dir() -> PathBuf {
        let user = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .map(|dir| dir.join("wayterm-build/libghostty-vt"));
        match user {
            Some(dir) if fs::create_dir_all(&dir).is_ok() => dir,
            _ => out_dir().join("libghostty-vt"),
        }
    }

    fn out_dir() -> PathBuf {
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"))
    }

    /// Download, check and build the pin, then copy the header tree and
    /// the static library to `root`. All work happens under `$OUT_DIR`
    /// and is deleted afterwards; `root` appears whole or not at all, so
    /// builds running at once cannot see each other's half.
    fn build(root: &Path) {
        let target = env::var("TARGET").expect("TARGET");
        assert!(
            env::var("HOST").is_ok_and(|host| host == target),
            "libghostty-vt is built for the machine it is built on, not for {target}. {WAYS_OUT}"
        );

        let work = out_dir().join("libghostty-vt-build");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).expect("create the libghostty-vt build directory");

        let source = format!("libghostty-vt-{VERSION}");
        let deps = format!("{source}-deps");
        for (name, sha256) in [(&source, SOURCE_SHA256), (&deps, DEPS_SHA256)] {
            let url = format!("{RELEASES}/{VERSION}/{name}.tar.gz");
            unpack(&fetch(&url, sha256, &work), &work);
        }
        let zig = zig(&work);

        // Offline: Zig takes its packages from the dependency tarball,
        // and its global cache must start empty.
        let out = work.join("out");
        run(
            Command::new(&zig)
                .current_dir(work.join(&source))
                .env("ZIG_GLOBAL_CACHE_DIR", work.join("zig-cache"))
                .arg("build")
                .arg("--system")
                .arg(work.join(&deps))
                .arg("-Doptimize=ReleaseFast")
                .arg(format!("-Dcpu={CPU}"))
                .arg("--prefix")
                .arg(&out),
            "zig build of libghostty-vt",
        );
        assert!(
            is_built(&out),
            "zig build left no {HEADER} and {LIBRARY} under {}. {WAYS_OUT}",
            out.display()
        );

        // Staged beside `root`, on its filesystem, so the rename is atomic.
        let mut staged = root.as_os_str().to_owned();
        staged.push(format!(".tmp-{}", std::process::id()));
        let staged = PathBuf::from(staged);
        let _ = fs::remove_dir_all(&staged);
        copy_dir(&out.join("include"), &staged.join("include"));
        fs::create_dir_all(staged.join("lib")).expect("create the library directory");
        fs::copy(out.join(LIBRARY), staged.join(LIBRARY)).expect("copy libghostty-vt.a");
        // The rename fails if `root` holds anything. A whole library, from
        // a build that finished first, is kept; anything less is replaced.
        if fs::rename(&staged, root).is_err() && !is_built(root) {
            let _ = fs::remove_dir_all(root);
            fs::rename(&staged, root).unwrap_or_else(|err| {
                panic!(
                    "installing libghostty-vt to {}: {err}. {WAYS_OUT}",
                    root.display()
                )
            });
        }
        let _ = fs::remove_dir_all(&staged);
        let _ = fs::remove_dir_all(&work);
        // Shown once the script ends: why this build took longer.
        println!(
            "cargo:warning=built libghostty-vt {VERSION}, kept for later builds in {}",
            root.display()
        );
    }

    /// The `zig` on `PATH` if it is the pinned version, else the pinned
    /// release, downloaded into `work`.
    fn zig(work: &Path) -> PathBuf {
        let on_path = Command::new("zig").arg("version").output().ok();
        if on_path.is_some_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == ZIG_VERSION
        }) {
            return PathBuf::from("zig");
        }
        let host = format!("{}-{}", env::consts::ARCH, env::consts::OS);
        assert!(
            host == ZIG_HOST,
            "building libghostty-vt needs Zig {ZIG_VERSION} on PATH: \
             it is only downloaded on {ZIG_HOST}, not {host}. {WAYS_OUT}"
        );
        let name = format!("zig-{ZIG_HOST}-{ZIG_VERSION}");
        let url = format!("https://ziglang.org/download/{ZIG_VERSION}/{name}.tar.xz");
        unpack(&fetch(&url, ZIG_SHA256, work), work);
        work.join(name).join("zig")
    }

    /// Download `url` into `dir` and return the file, once its sha256 is
    /// the pinned one.
    fn fetch(url: &str, sha256: &str, dir: &Path) -> PathBuf {
        let file = dir.join(url.rsplit('/').next().expect("a file name"));
        run(
            Command::new("curl")
                .args(["--fail", "--silent", "--show-error", "--location"])
                .args(["--proto", "=https", "--retry", "3"])
                .arg("--output")
                .arg(&file)
                .arg(url),
            &format!("downloading {url}"),
        );
        let sum = Command::new("sha256sum")
            .arg(&file)
            .output()
            .unwrap_or_else(|err| panic!("sha256sum: {err}. {WAYS_OUT}"));
        let sum = String::from_utf8_lossy(&sum.stdout);
        let sum = sum.split_whitespace().next().unwrap_or_default();
        assert!(
            sum == sha256,
            "{url} has sha256 {sum:?}, not the pinned {sha256}, and was not unpacked. {WAYS_OUT}"
        );
        file
    }

    fn unpack(archive: &Path, into: &Path) {
        run(
            Command::new("tar")
                .arg("-xf")
                .arg(archive)
                .arg("-C")
                .arg(into),
            &format!("unpacking {}", archive.display()),
        );
    }

    fn run(command: &mut Command, what: &str) {
        let status = command.status().unwrap_or_else(|err| {
            panic!(
                "{what}: could not run {:?}: {err}. {WAYS_OUT}",
                command.get_program()
            )
        });
        assert!(status.success(), "{what} failed ({status}). {WAYS_OUT}");
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("create a header directory");
        for entry in fs::read_dir(from).expect("read a header directory") {
            let entry = entry.expect("read a header directory");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("a header's file type").is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).expect("copy a header");
            }
        }
    }
}
