//! The display-free half of the icons: defaults, naming, and SVG import.

use std::path::Path;

use crate::store::model::SessionType;

use super::*;

const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"/>"#;

#[test]
fn every_session_type_has_a_symbolic_default() {
    for session_type in [
        SessionType::SSH,
        SessionType::RDP,
        SessionType::VNC,
        SessionType::Serial,
        SessionType::LocalShell,
    ] {
        let icon = session_type_icon(&session_type);
        assert!(icon.ends_with("-symbolic"), "{icon}");
        assert!(BUILTIN.contains(&icon), "{icon} is built in");
    }
    assert!(BUILTIN.contains(&FOLDER_ICON));
}

fn builtin_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/tree-icons")
}

/// Every name in `BUILTIN` has an SVG, every SVG is listed, and the
/// GResource manifest bundles exactly those files.
#[test]
fn builtin_list_matches_the_svgs_and_the_manifest() {
    let svgs = custom_icons(&builtin_dir().join("scalable/actions"));
    let mut listed: Vec<String> = BUILTIN.iter().map(|name| name.to_string()).collect();
    listed.sort();
    let mut unique = listed.clone();
    unique.dedup();
    assert_eq!(listed, unique, "no duplicates");
    assert_eq!(svgs, listed);

    let manifest = std::fs::read_to_string(builtin_dir().join("tree-icons.gresource.xml")).unwrap();
    for name in BUILTIN {
        assert!(
            manifest.contains(&format!(">scalable/actions/{name}.svg<")),
            "{name} is bundled"
        );
    }
    assert_eq!(manifest.matches("<file ").count(), BUILTIN.len());
}

/// GTK recolors a symbolic SVG by overriding the fill of `rect`, `circle`
/// and `path` only: any other shape, or a stroke, would keep its own color
/// and vanish on a dark theme.
#[test]
fn builtin_svgs_use_only_recolorable_shapes() {
    for name in BUILTIN {
        let path = builtin_dir().join(format!("scalable/actions/{name}.svg"));
        let svg = std::fs::read_to_string(&path).unwrap();
        assert!(svg.contains(r#"viewBox="0 0 16 16""#), "{name}: 16×16");
        assert!(!svg.contains("stroke"), "{name}: no strokes");
        for tag in svg.split('<').skip(1) {
            let element = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .next()
                .unwrap();
            assert!(
                ["svg", "rect", "circle", "path"].contains(&element),
                "{name}: <{element}> is not recolored"
            );
        }
    }
}

#[test]
fn icon_labels_are_readable() {
    assert_eq!(icon_label("wayterm-server-rack-symbolic"), "server rack");
    assert_eq!(icon_label("wayterm-proxmox"), "proxmox");
    assert_eq!(icon_label("network-server-symbolic"), "network server");
}

#[test]
fn import_never_takes_a_builtin_name() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("server-symbolic.svg");
    std::fs::write(&source, SVG).unwrap();
    let name = import_svg(&source, &tmp.path().join("icons")).unwrap();
    assert_eq!(name, "wayterm-server-2-symbolic");
}

#[test]
fn resolve_icon_falls_back_to_the_default() {
    let available = |name: &str| name == "there";
    assert_eq!(resolve_icon(Some("there"), "default", available), "there");
    assert_eq!(resolve_icon(Some(" there "), "default", available), "there");
    assert_eq!(resolve_icon(Some("gone"), "default", available), "default");
    assert_eq!(resolve_icon(Some(""), "default", available), "default");
    assert_eq!(resolve_icon(None, "default", available), "default");
}

#[test]
fn icon_names_are_prefixed_and_sanitized() {
    let name = |file: &str| icon_name_for_file(Path::new(file));
    assert_eq!(name("Proxmox Logo.svg"), "wayterm-proxmox-logo");
    assert_eq!(name("/x/truenas_scale--v2.SVG"), "wayterm-truenas-scale-v2");
    assert_eq!(name("pi-symbolic.svg"), "wayterm-pi-symbolic");
    assert_eq!(name("wayterm-nas.svg"), "wayterm-nas", "no double prefix");
    assert_eq!(name("ünïcode!.svg"), "wayterm-n-code");
    assert_eq!(name("!!!.svg"), "wayterm-icon");
    assert_eq!(name("-symbolic.svg"), "wayterm-icon");
}

#[test]
fn import_copies_reuses_and_numbers() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("icons");
    let source = tmp.path().join("Synology-symbolic.svg");
    std::fs::write(&source, SVG).unwrap();

    let name = import_svg(&source, &dir).unwrap();
    assert_eq!(name, "wayterm-synology-symbolic");
    assert_eq!(
        std::fs::read_to_string(dir.join("wayterm-synology-symbolic.svg")).unwrap(),
        SVG
    );
    // The same file again: same name, no copy.
    assert_eq!(import_svg(&source, &dir).unwrap(), name);
    assert_eq!(custom_icons(&dir), std::slice::from_ref(&name));

    // A different file with the same stem leaves the first alone.
    let other = tmp.path().join("other").join("Synology-symbolic.svg");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, SVG.replace("16 16", "24 24")).unwrap();
    assert_eq!(
        import_svg(&other, &dir).unwrap(),
        "wayterm-synology-2-symbolic"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("wayterm-synology-symbolic.svg")).unwrap(),
        SVG
    );
    assert_eq!(
        custom_icons(&dir),
        ["wayterm-synology-2-symbolic", "wayterm-synology-symbolic"]
    );
}

#[test]
fn import_refuses_anything_but_svg() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("icons");

    let png = tmp.path().join("logo.png");
    std::fs::write(&png, SVG).unwrap();
    assert!(import_svg(&png, &dir).is_err(), "wrong extension");

    let fake = tmp.path().join("fake.svg");
    std::fs::write(&fake, "not an image").unwrap();
    assert!(import_svg(&fake, &dir).is_err(), "no <svg> element");

    let huge = tmp.path().join("huge.svg");
    let mut text = String::from(SVG);
    text.push_str(&" ".repeat(MAX_SVG_BYTES as usize));
    std::fs::write(&huge, text).unwrap();
    assert!(import_svg(&huge, &dir).is_err(), "over the size cap");

    assert!(custom_icons(&dir).is_empty(), "nothing was copied");
}

#[test]
fn custom_icons_of_a_missing_dir_is_empty() {
    assert!(custom_icons(Path::new("/nonexistent/wayterm/icons")).is_empty());
}
