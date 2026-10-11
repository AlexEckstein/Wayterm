//! Compiles the built-in tree icons (`assets/tree-icons`, doc 06 §Icons)
//! into a GResource bundle that `ui::icons` embeds.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let source = "assets/tree-icons";
    println!("cargo:rerun-if-changed={source}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let status = Command::new("glib-compile-resources")
        .arg(format!("--sourcedir={source}"))
        .arg(format!(
            "--target={}",
            out.join("tree-icons.gresource").display()
        ))
        .arg(format!("{source}/tree-icons.gresource.xml"))
        .status()
        .expect("glib-compile-resources not found (it ships with the GLib dev tools)");
    assert!(status.success(), "glib-compile-resources failed");
}
