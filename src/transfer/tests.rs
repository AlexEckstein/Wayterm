use std::fs;
use std::path::Path;

use super::*;
use crate::store::model::{AuthMethod, SessionType, SshOptions};

fn ssh(text: &str, base: Option<&Path>) -> Parsed {
    parse_source(ImportSource::SshConfig, text, base).unwrap()
}

fn find<'a>(sessions: &'a [SessionConfig], name: &str) -> &'a SessionConfig {
    sessions
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no session {name}"))
}

fn has_warning(report: &ImportReport, needle: &str) -> bool {
    report.warnings.iter().any(|w| w.contains(needle))
}

// ---- ~/.ssh/config scanner ----

#[test]
fn ssh_config_value_forms() {
    let text = "\
# leading comment
Host web db   # two aliases
  HostName=10.0.0.5
  user = deploy
  PORT 2222
  IdentityFile \"/keys/my key\" # quoted, with a space
  IdentityFile /keys/second
  IdentitiesOnly yes
  ServerAliveInterval 30
Host bare
  user first
  User second
";
    let (folders, sessions, report) = ssh(text, None);
    assert!(folders.is_empty());
    assert_eq!(sessions.len(), 3);
    for name in ["web", "db"] {
        let s = find(&sessions, name);
        assert_eq!(s.session_type, SessionType::SSH);
        assert_eq!(s.host.as_deref(), Some("10.0.0.5"));
        assert_eq!(s.username.as_deref(), Some("deploy"));
        assert_eq!(s.port, Some(2222));
        assert_eq!(s.key_path.as_deref(), Some("/keys/my key"));
        assert_eq!(s.auth_method, Some(AuthMethod::Key));
        assert_eq!(
            s.ssh_options,
            Some(SshOptions {
                compression: false,
                keepalive_interval: Some(30),
                keepalive_count: None,
                ssh_w_fallback: false,
            })
        );
    }
    let bare = find(&sessions, "bare");
    assert_eq!(bare.username.as_deref(), Some("first"), "first value wins");
    assert_eq!(bare.host, None, "host stays empty without HostName");
    assert_eq!(report.templates, 0);
    assert_eq!(report.skipped, 0);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn ssh_config_continuation_line() {
    let (_, sessions, _) = ssh("Host one \\\n two\n  User u\n", None);
    assert_eq!(sessions.len(), 2);
    assert_eq!(find(&sessions, "two").username.as_deref(), Some("u"));
}

#[test]
fn ssh_config_wildcards_become_templates_and_negations_skip() {
    let text = "\
Host *.corp !bastion.corp exact
  HostName %h.internal
  User ops
Host ?b
  Port 23
Host !nope
  User ghost
";
    let (_, sessions, report) = ssh(text, None);
    let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["*.corp", "exact", "?b"]);
    let template = find(&sessions, "*.corp");
    assert_eq!(template.host, None, "no host for a wildcard pattern");
    assert_eq!(template.username.as_deref(), Some("ops"));
    assert_eq!(find(&sessions, "?b").port, Some(23));
    let exact = find(&sessions, "exact");
    assert_eq!(exact.host.as_deref(), Some("%h.internal"));
    assert_eq!(report.templates, 2);
    assert_eq!(report.skipped, 2);
}

#[test]
fn ssh_config_match_blocks_are_skipped() {
    let text = "\
Host a
  User ua
Match host a exec \"true\"
  User from-match
  Port 1
Match all
  Include never-read
Host b
  User ub
";
    let (_, sessions, report) = ssh(text, None);
    assert_eq!(sessions.len(), 2);
    assert_eq!(find(&sessions, "a").username.as_deref(), Some("ua"));
    assert_eq!(find(&sessions, "a").port, None);
    assert_eq!(find(&sessions, "b").username.as_deref(), Some("ub"));
    assert!(
        has_warning(&report, "2 Match blocks skipped"),
        "{:?}",
        report.warnings
    );
    assert!(!has_warning(&report, "never-read"));
}

#[test]
fn ssh_config_global_options_are_reported() {
    let (_, sessions, report) = ssh("User everyone\nHost a\n", None);
    assert_eq!(find(&sessions, "a").username, None);
    assert!(has_warning(&report, "outside any Host block"));
}

#[test]
fn ssh_config_include_chain() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    fs::create_dir(base.join("config.d")).unwrap();
    fs::write(
        base.join("config.d/10-work"),
        "Host work\n  HostName work.example\n  User inc\n",
    )
    .unwrap();
    fs::write(base.join("config.d/20-lab"), "Host lab\n  Port 2200\n").unwrap();
    fs::write(base.join("config.d/notes.txt"), "Host not-globbed\n").unwrap();
    fs::write(base.join("extra"), "Host extra\n").unwrap();
    let main = format!(
        "Include config.d/??-* extra\nInclude missing-file\nInclude {}/extra\n\
         Host work\n  User main\n  Port 2201\n",
        base.display()
    );
    let (_, sessions, report) = ssh(&main, Some(base));
    let work = find(&sessions, "work");
    assert_eq!(work.host.as_deref(), Some("work.example"));
    assert_eq!(
        work.username.as_deref(),
        Some("inc"),
        "included value came first"
    );
    assert_eq!(work.port, Some(2201), "later values fill gaps");
    assert_eq!(find(&sessions, "lab").port, Some(2200));
    find(&sessions, "extra");
    assert!(!sessions.iter().any(|s| s.name == "not-globbed"));
    assert!(
        has_warning(&report, "missing-file: file not found"),
        "{:?}",
        report.warnings
    );
}

#[test]
fn ssh_config_include_inside_host_applies_to_that_host() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("common"),
        "User shared\nHost other\n  User o\n",
    )
    .unwrap();
    let (_, sessions, _) = ssh("Host a\n  Include common\n  Port 7\n", Some(dir.path()));
    let a = find(&sessions, "a");
    assert_eq!(a.username.as_deref(), Some("shared"));
    assert_eq!(
        a.port,
        Some(7),
        "the including block resumes after the Include"
    );
    assert_eq!(find(&sessions, "other").username.as_deref(), Some("o"));
}

#[test]
fn ssh_config_include_cycle_and_depth_cap() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    fs::write(base.join("loop-a"), "Host la\nInclude loop-b\n").unwrap();
    fs::write(base.join("loop-b"), "Host lb\nInclude loop-a\n").unwrap();
    let (_, sessions, report) = ssh("Include loop-a\n", Some(base));
    assert_eq!(sessions.len(), 2);
    assert!(
        has_warning(&report, "include cycle"),
        "{:?}",
        report.warnings
    );

    for level in 1..=7 {
        let next = format!("Host d{level}\nInclude d{}\n", level + 1);
        fs::write(base.join(format!("d{level}")), next).unwrap();
    }
    let (_, sessions, report) = ssh("Include d1\n", Some(base));
    let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["d1", "d2", "d3", "d4", "d5"]);
    assert!(
        has_warning(&report, "nested deeper than 5"),
        "{:?}",
        report.warnings
    );
}

#[test]
fn ssh_config_include_refuses_non_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    fs::create_dir(base.join("adir")).unwrap();
    std::os::unix::fs::symlink(base.join("adir"), base.join("link-to-dir")).unwrap();
    let fifo = base.join("fifo");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo only creates the node.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let (_, sessions, report) = ssh("Include link-to-dir fifo\nHost after\n", Some(base));
    assert_eq!(sessions.len(), 1);
    let refusals = report
        .warnings
        .iter()
        .filter(|w| w.contains("not a regular file"))
        .count();
    assert_eq!(refusals, 2, "{:?}", report.warnings);
}

#[test]
fn ssh_config_relative_include_without_base_warns() {
    let (_, _, report) = ssh("Include config.d/*\n", None);
    assert!(has_warning(&report, "no base directory"));
}

#[test]
fn ssh_config_proxy_jump_resolves_or_drops() {
    let text = "\
Host app
  ProxyJump bastion
Host bastion
  HostName bastion.example
Host far
  ProxyJump nowhere
Host chain
  ProxyJump bastion,app
Host self
  ProxyJump self
";
    let (folders, sessions, report) = ssh(text, None);
    let bastion = sessions.iter().position(|s| s.name == "bastion").unwrap();
    assert_eq!(find(&sessions, "app").jump_session_id, Some(bastion as i64));
    for dropped in ["far", "chain", "self"] {
        assert_eq!(find(&sessions, dropped).jump_session_id, None, "{dropped}");
    }
    assert!(has_warning(&report, "far: ProxyJump nowhere is not a Host"));
    assert!(has_warning(&report, "several hops"));

    let store = SessionStore::in_memory().unwrap();
    let report = apply_import(&store, folders, sessions, report).unwrap();
    assert_eq!(report.imported, 5);
    let stored = store.list_sessions(None).unwrap();
    let bastion_id = find(&stored, "bastion").id;
    assert_eq!(find(&stored, "app").jump_session_id, Some(bastion_id));
}

// ---- apply_import ----

#[test]
fn apply_import_remaps_folders_and_drops_second_hops() {
    let folders = vec![
        FolderSpec {
            name: "a".into(),
            parent: None,
            icon: Some("star".into()),
        },
        FolderSpec {
            name: "a".into(),
            parent: Some("a".into()),
            icon: None,
        },
        FolderSpec {
            name: "orphan".into(),
            parent: Some("nope".into()),
            icon: None,
        },
    ];
    let session = |name: &str, folder: Option<i64>, jump: Option<i64>| SessionConfig {
        name: name.into(),
        folder_id: folder,
        jump_session_id: jump,
        ..SessionConfig::default()
    };
    let sessions = vec![
        session("inner", Some(1), Some(1)),
        session("hop", Some(0), Some(2)),
        session("target", None, None),
        session("lost", Some(9), Some(8)),
    ];
    let store = SessionStore::in_memory().unwrap();
    let report = apply_import(&store, folders, sessions, ImportReport::default()).unwrap();
    assert_eq!(report.imported, 4);
    let root = store.list_folders(None).unwrap();
    let outer = root.iter().find(|f| f.name == "a").unwrap();
    assert_eq!(outer.icon.as_deref(), Some("star"), "folder icons import");
    assert!(root.iter().any(|f| f.name == "orphan"));
    let inner_folder = &store.list_folders(Some(outer.id)).unwrap()[0];
    assert_eq!(inner_folder.name, "a");
    let inner = &store.list_sessions(Some(inner_folder.id)).unwrap()[0];
    assert_eq!(inner.name, "inner");
    assert_eq!(inner.jump_session_id, None, "its target itself jumps");
    let root_sessions = store.list_sessions(None).unwrap();
    let hop = &store.list_sessions(Some(outer.id)).unwrap()[0];
    assert_eq!(hop.jump_session_id, Some(find(&root_sessions, "target").id));
    let lost = find(&root_sessions, "lost");
    assert_eq!((lost.folder_id, lost.jump_session_id), (None, None));
    for needle in [
        "parent \"nope\" not found",
        "inner\": jump host dropped",
        "lost\": folder not found",
        "lost\": jump host dropped — target not found",
    ] {
        assert!(
            has_warning(&report, needle),
            "{needle}: {:?}",
            report.warnings
        );
    }
}

// ---- JSON ----

/// A store with root/sub folders, nested sessions, a jump, and a root
/// session.
fn sample_store() -> SessionStore {
    let store = SessionStore::in_memory().unwrap();
    let prod = store.insert_folder("prod", None).unwrap();
    let web = store.insert_folder("web", Some(prod)).unwrap();
    store.insert_folder("empty", None).unwrap();
    let bastion = store
        .insert_session(&SessionConfig {
            name: "bastion".into(),
            folder_id: Some(prod),
            host: Some("bastion.example".into()),
            port: Some(22),
            auth_method: Some(AuthMethod::Password),
            ..SessionConfig::default()
        })
        .unwrap();
    store
        .insert_session(&SessionConfig {
            name: "web1".into(),
            folder_id: Some(web),
            host: Some("10.0.0.1".into()),
            port: Some(2222),
            jump_session_id: Some(bastion),
            ..SessionConfig::default()
        })
        .unwrap();
    store
        .insert_session(&SessionConfig {
            name: "desk".into(),
            session_type: SessionType::RDP,
            host: Some("desk.example".into()),
            port: Some(3389),
            ..SessionConfig::default()
        })
        .unwrap();
    store
}

/// (folder path, name, host, port, type) for every session, sorted.
type Row = (String, String, Option<String>, Option<u16>, SessionType);

fn snapshot(store: &SessionStore) -> Vec<Row> {
    fn walk(store: &SessionStore, parent: Option<i64>, path: &str, out: &mut Vec<Row>) {
        for s in store.list_sessions(parent).unwrap() {
            out.push((path.into(), s.name, s.host, s.port, s.session_type));
        }
        for f in store.list_folders(parent).unwrap() {
            let child = format!("{path}/{}", f.name);
            out.push((
                child.clone(),
                String::new(),
                None,
                None,
                SessionType::LocalShell,
            ));
            walk(store, Some(f.id), &child, out);
        }
    }
    let mut out = Vec::new();
    walk(store, None, "", &mut out);
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

#[test]
fn json_roundtrip_keeps_nesting_and_jumps() {
    let source = sample_store();
    let text = export_json(&source).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["folders"][1]["name"], "prod");
    assert_eq!(doc["folders"][1]["children"][0]["name"], "web");
    let web1 = &doc["folders"][1]["children"][0]["sessions"][0];
    assert_eq!(web1["name"], "web1");
    assert_eq!(web1["jump_session"], "bastion");
    for key in ["id", "folder_id", "jump_session_id"] {
        assert!(web1.get(key).is_none(), "export carries no {key}");
    }
    assert_eq!(
        doc["sessions"][0]["name"], "desk",
        "root sessions stay at the root"
    );

    let (folders, sessions, report) = parse_source(ImportSource::Wayterm, &text, None).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let target = SessionStore::in_memory().unwrap();
    let report = apply_import(&target, folders, sessions, report).unwrap();
    assert_eq!(report.imported, 3);
    assert_eq!(snapshot(&source), snapshot(&target));

    let prod = target.list_folders(None).unwrap();
    let prod = prod.iter().find(|f| f.name == "prod").unwrap();
    let bastion = &target.list_sessions(Some(prod.id)).unwrap()[0];
    let web = &target.list_folders(Some(prod.id)).unwrap()[0];
    let web1 = &target.list_sessions(Some(web.id)).unwrap()[0];
    assert_eq!(web1.jump_session_id, Some(bastion.id));
}

#[test]
fn json_roundtrip_keeps_icons() {
    let source = SessionStore::in_memory().unwrap();
    let folder = source.insert_folder("lab", None).unwrap();
    source
        .update_folder(folder, "lab", Some("wayterm-homelab"))
        .unwrap();
    source
        .insert_session(&SessionConfig {
            name: "nas".into(),
            folder_id: Some(folder),
            icon: Some("drive-multidisk-symbolic".into()),
            ..SessionConfig::default()
        })
        .unwrap();

    let text = export_json(&source).unwrap();
    let (folders, sessions, report) = parse_source(ImportSource::Wayterm, &text, None).unwrap();
    let target = SessionStore::in_memory().unwrap();
    let report = apply_import(&target, folders, sessions, report).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let lab = &target.list_folders(None).unwrap()[0];
    assert_eq!(lab.icon.as_deref(), Some("wayterm-homelab"));
    let nas = &target.list_sessions(Some(lab.id)).unwrap()[0];
    assert_eq!(nas.icon.as_deref(), Some("drive-multidisk-symbolic"));
}

#[test]
fn json_import_shapes() {
    // `notes` is in exports made before sessions lost the field: ignored.
    let text = r#"{
        "version": 1,
        "future_field": {"x": 1},
        "sessions": [{"name": "loose", "host": "h", "id": 99, "folder_id": 42, "notes": "old"}],
        "folders": [{"name": "f", "sessions": [{"name": "s", "jump_session": "ghost"}]}]
    }"#;
    let (folders, sessions, report) = parse_source(ImportSource::Wayterm, text, None).unwrap();
    assert_eq!(folders.len(), 1);
    let loose = find(&sessions, "loose");
    assert_eq!(
        (loose.id, loose.folder_id),
        (0, None),
        "ids are never imported"
    );
    assert_eq!(find(&sessions, "s").folder_id, Some(0));
    assert!(has_warning(&report, "\"ghost\" is not in this import"));
}

#[test]
fn json_newer_version_is_refused() {
    let err = parse_source(
        ImportSource::Wayterm,
        r#"{"version": 2, "folders": []}"#,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ImportError::UnsupportedVersion(2)));
    assert!(err.to_string().contains('2'));
    assert!(matches!(
        parse_source(ImportSource::Wayterm, "not json", None),
        Err(ImportError::Parse(_))
    ));
}

#[test]
fn json_export_holds_no_secrets() {
    let text = export_json(&sample_store()).unwrap();
    for needle in ["password=", "ssh_password="] {
        assert!(!text.contains(needle), "export contains {needle}");
    }
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut keys = Vec::new();
    collect_keys(&doc, &mut keys);
    for key in keys {
        let key = key.to_ascii_lowercase();
        assert!(
            !key.contains("password") && !key.contains("passphrase") && !key.contains("secret"),
            "secret-like key {key}"
        );
    }
}

fn collect_keys(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                out.push(key.clone());
                collect_keys(child, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_keys(v, out)),
        _ => {}
    }
}

// ---- other sources ----

/// No imported field holds `secret`.
fn assert_secret_absent(sessions: &[SessionConfig], secret: &str) {
    for session in sessions {
        let text = serde_json::to_string(session).unwrap();
        assert!(!text.contains(secret), "{} leaked a secret", session.name);
    }
}

const REMMINA: &str = "\
[remmina]
name=Build box
group=Work/CI
protocol=SSH
server=builder@ci.example:2022
username=ignored
ssh_auth=1
ssh_privatekey=/home/u/.ssh/id_ci
password=c2VjcmV0LXZhbHVl
ssh_passphrase=c2VjcmV0LXZhbHVl
ssh_tunnel_enabled=1
window_width=640
";

#[test]
fn remmina_fixture() {
    let (folders, sessions, report) = parse_source(ImportSource::Remmina, REMMINA, None).unwrap();
    assert_eq!(
        folders,
        vec![
            FolderSpec {
                name: "Work".into(),
                parent: None,
                icon: None
            },
            FolderSpec {
                name: "CI".into(),
                parent: Some("Work".into()),
                icon: None
            },
        ]
    );
    let s = &sessions[0];
    assert_eq!(s.name, "Build box");
    assert_eq!(s.folder_id, Some(1));
    assert_eq!(s.host.as_deref(), Some("ci.example"));
    assert_eq!(s.port, Some(2022));
    assert_eq!(s.username.as_deref(), Some("builder"));
    assert_eq!(s.auth_method, Some(AuthMethod::Key));
    assert_eq!(s.key_path.as_deref(), Some("/home/u/.ssh/id_ci"));
    assert_secret_absent(&sessions, "c2VjcmV0LXZhbHVl");
    assert!(has_warning(&report, "password: secret skipped"));
    assert!(has_warning(&report, "ssh_passphrase: secret skipped"));
    assert!(has_warning(&report, "tunnel"));

    let rdp =
        "[remmina]\nprotocol=RDP\nserver=[fe80::1]:3390\ndomain=CORP\nresolution_width=1920\n";
    let (_, sessions, _) = parse_source(ImportSource::Remmina, rdp, None).unwrap();
    let s = &sessions[0];
    assert_eq!(s.session_type, SessionType::RDP);
    assert_eq!(
        s.name, "[fe80::1]:3390",
        "the server names an unnamed profile"
    );
    assert_eq!((s.host.as_deref(), s.port), (Some("fe80::1"), Some(3390)));
    assert_eq!(s.rdp_domain.as_deref(), Some("CORP"));
    assert_eq!(s.rdp_width, Some(1920));
}

const MOBAXTERM: &str = "\
[Bookmarks]
SubRep=
ImgNum=42
root box=#109#0%10.1.1.1%22%admin%%-1%-1%%%%%0%0%0%%%-1%0%0%0%%1080%%0%0%1#MobaFont%10%0#0# #-1

[Bookmarks_1]
SubRep=Sites\\Berlin
ImgNum=41
desk=#91#4%desk.berlin%3389%anna%-1%0%0%0#MobaFont%10#0# #-1
screen=#128#5%vnc.berlin%5900%%-1#MobaFont%10#0# #-1
old telnet=#98#1%legacy%23%%#MobaFont#0# #-1

[Passwords]
admin@10.1.1.1=c2VjcmV0LXZhbHVl

[Misc]
Theme=dark
";

#[test]
fn mobaxterm_fixture() {
    let (folders, sessions, report) =
        parse_source(ImportSource::MobaXterm, MOBAXTERM, None).unwrap();
    let paths: Vec<String> = folders.iter().map(FolderSpec::path).collect();
    assert_eq!(paths, ["Sites", "Sites/Berlin"]);
    assert_eq!(sessions.len(), 3);
    let root = find(&sessions, "root box");
    assert_eq!(root.folder_id, None);
    assert_eq!(
        (root.host.as_deref(), root.port, root.username.as_deref()),
        (Some("10.1.1.1"), Some(22), Some("admin"))
    );
    let desk = find(&sessions, "desk");
    assert_eq!(desk.session_type, SessionType::RDP);
    assert_eq!(desk.folder_id, Some(1));
    let screen = find(&sessions, "screen");
    assert_eq!(screen.session_type, SessionType::VNC);
    assert_eq!(screen.username, None);
    assert_eq!(report.skipped, 1);
    assert!(has_warning(&report, "old telnet: session type 98"));
    assert!(has_warning(&report, "[Passwords] section skipped"));
    assert!(has_warning(&report, "only host, port and user"));
    assert_secret_absent(&sessions, "c2VjcmV0LXZhbHVl");
}

const ASBRU: &str = "\
---
__PAC__EXPORTED__FULL__: 1
environments:
  g-1:
    _is_group: 1
    name: Datacenter
    parent: __PAC__ROOT__
    children:
      g-2: 1
  g-2:
    _is_group: 1
    name: Rack 4
    parent: g-1
  g-3:
    _is_group: '1'
    name: Unused
    parent: __PAC__ROOT__
  s-1:
    _is_group: 0
    name: db01
    parent: g-2
    method: SSH
    ip: db01.dc
    port: 2222
    user: postgres
    auth type: publickey
    public key: /home/u/.ssh/id_db
    pass: c2VjcmV0LXZhbHVl
    passphrase: c2VjcmV0LXZhbHVl
    description: primary database
    terminal options:
      use personal settings: 1
  s-2:
    name: kiosk
    parent: __PAC__ROOT__
    method: RDP (xfreerdp)
    ip: 10.9.9.9
  s-3:
    name: modem
    method: Serial (cu)
    ip: /dev/ttyS0
";

#[test]
fn asbru_fixture() {
    let (folders, sessions, report) = parse_source(ImportSource::Asbru, ASBRU, None).unwrap();
    let paths: Vec<String> = folders.iter().map(FolderSpec::path).collect();
    assert_eq!(paths, ["Datacenter", "Datacenter/Rack 4", "Unused"]);
    assert_eq!(sessions.len(), 2);
    let db = find(&sessions, "db01");
    assert_eq!(db.folder_id, Some(1));
    assert_eq!(
        (db.host.as_deref(), db.port, db.username.as_deref()),
        (Some("db01.dc"), Some(2222), Some("postgres"))
    );
    assert_eq!(db.auth_method, Some(AuthMethod::Key));
    assert_eq!(db.key_path.as_deref(), Some("/home/u/.ssh/id_db"));
    let kiosk = find(&sessions, "kiosk");
    assert_eq!(
        (kiosk.session_type.clone(), kiosk.folder_id),
        (SessionType::RDP, None)
    );
    assert_eq!(report.skipped, 1);
    assert!(has_warning(&report, "modem: method \"Serial (cu)\""));
    assert!(has_warning(&report, "secret skipped: pass, passphrase"));
    assert!(has_warning(
        &report,
        "Asbru fields not imported: description, terminal options"
    ));
    assert_secret_absent(&sessions, "c2VjcmV0LXZhbHVl");

    let store = SessionStore::in_memory().unwrap();
    let report = apply_import(&store, folders, sessions, report).unwrap();
    assert_eq!(report.imported, 2);
    let dc = &store.list_folders(None).unwrap()[0];
    let rack = &store.list_folders(Some(dc.id)).unwrap()[0];
    assert_eq!(store.list_sessions(Some(rack.id)).unwrap()[0].name, "db01");
}

#[test]
fn read_source_reads_regular_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config");
    fs::write(&file, "Host x\n").unwrap();
    assert_eq!(read_source(&file).unwrap(), "Host x\n");
    assert!(matches!(read_source(dir.path()), Err(ImportError::Io(_))));
    assert!(matches!(
        read_source(&dir.path().join("missing")),
        Err(ImportError::Io(_))
    ));
}

#[test]
fn split_server_forms() {
    let cases = [
        ("host", (None, "host", Ok(None))),
        ("u@host:22", (Some("u"), "host", Ok(Some(22)))),
        ("[::1]:2200", (None, "::1", Ok(Some(2200)))),
        ("fe80::1", (None, "fe80::1", Ok(None))),
        ("h:bad", (None, "h", Err("h:bad".to_string()))),
    ];
    for (input, (user, host, port)) in cases {
        let got = split_server(input);
        assert_eq!(
            got,
            (user.map(str::to_string), host.to_string(), port),
            "{input}"
        );
    }
}
