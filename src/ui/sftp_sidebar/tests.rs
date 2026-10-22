//! Display-free tests for the SFTP sidebar's model layer (doc 09).
//!
//! Everything exercised here is pure: row mapping, column formatting, the
//! toolbar enable/disable rules, the `text/uri-list` fallback, the pending
//! command queue and the whole [`SftpView`] state machine. The GTK layer only
//! renders a view, so none of it needs a display.

use super::*;
use crate::sftp::DirEntry;

fn entry(name: &str, is_dir: bool, size: u64) -> DirEntry {
    DirEntry {
        name: name.to_string(),
        size,
        modified: "2026-08-28 12:00".to_string(),
        permissions: if is_dir {
            "drwxr-xr-x".to_string()
        } else {
            "-rw-r--r--".to_string()
        },
        is_dir,
    }
}

fn listing() -> Vec<DirEntry> {
    vec![entry("docs", true, 0), entry("notes.txt", false, 1234)]
}

// --- row mapping and columns ----------------------------------------------

#[test]
fn listing_puts_the_parent_row_first_then_the_entries() {
    let rows = listing_rows("/home/user", &listing());
    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, [PARENT_ROW, "docs", "notes.txt"]);
    assert!(rows[0].is_parent);
    assert!(!rows[1].is_parent);
}

#[test]
fn listing_omits_the_parent_row_at_the_filesystem_root() {
    let rows = listing_rows("/", &listing());
    assert_eq!(rows.len(), 2, "root has nowhere to go up to");
    assert!(rows.iter().all(|row| !row.is_parent));
}

#[test]
fn directories_have_no_size_and_files_get_a_human_one() {
    let rows = listing_rows("/home/user", &listing());
    let dir = &rows[1];
    let file = &rows[2];
    assert_eq!(dir.size, "", "a directory has no meaningful size");
    assert_eq!(file.size, "1.2 KB");
    assert_eq!(dir.modified, "2026-08-28 12:00");
    assert_eq!(dir.permissions, "drwxr-xr-x");
}

#[test]
fn format_size_uses_binary_units_and_keeps_bytes_exact() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1023), "1023 B");
    assert_eq!(format_size(1024), "1.0 KB");
    assert_eq!(format_size(1536), "1.5 KB");
    assert_eq!(format_size(1024 * 1024), "1.0 MB");
    assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    // The unit table stops at PB, so the largest u64 saturates there instead
    // of inventing an unit.
    assert_eq!(format_size(u64::MAX), "16384.0 PB");
}

#[test]
fn row_icons_distinguish_directories_files_and_the_parent_row() {
    let parent = parent_row();
    let rows = listing_rows("/home/user", &listing());
    assert_ne!(parent.icon, rows[1].icon, "parent and directory differ");
    assert_ne!(rows[1].icon, rows[2].icon, "directory and file differ");
    assert!(parent.is_dir, "the parent row navigates like a directory");
}

// --- path arithmetic -------------------------------------------------------

#[test]
fn join_path_never_doubles_or_drops_the_separator() {
    assert_eq!(join_path("/home/user", "notes.txt"), "/home/user/notes.txt");
    assert_eq!(
        join_path("/home/user/", "notes.txt"),
        "/home/user/notes.txt"
    );
    assert_eq!(join_path("", "notes.txt"), "notes.txt");
    assert_eq!(join_path(".", "notes.txt"), "./notes.txt");
    assert_eq!(join_path("/home/user", "/etc/hosts"), "/etc/hosts");
}

#[test]
fn parent_path_walks_to_the_root_and_stops_there() {
    assert_eq!(
        parent_path("/home/user/docs"),
        Some("/home/user".to_string())
    );
    assert_eq!(parent_path("/home/user/"), Some("/home".to_string()));
    assert_eq!(parent_path("/home"), Some("/".to_string()));
    assert_eq!(parent_path("/"), None, "root has no parent");
    assert_eq!(parent_path(""), None);
    assert_eq!(parent_path("docs"), Some(".".to_string()));
}

#[test]
fn file_name_takes_the_last_component() {
    assert_eq!(file_name("/home/user/notes.txt"), "notes.txt");
    assert_eq!(file_name("/home/user/"), "user");
    assert_eq!(file_name("notes.txt"), "notes.txt");
    assert_eq!(file_name("/"), "");
}

#[test]
fn navigating_into_a_row_and_back_is_a_round_trip() {
    let rows = listing_rows("/home/user", &listing());
    let docs = &rows[1];
    let into = join_path("/home/user", &docs.name);
    assert_eq!(into, "/home/user/docs");
    assert_eq!(parent_path(&into), Some("/home/user".to_string()));
}

// --- toolbar rules ---------------------------------------------------------

#[test]
fn toolbar_is_fully_disabled_without_a_connection() {
    let selection = row_from_entry(&entry("notes.txt", false, 1));
    assert_eq!(
        toolbar_state(Some(&selection), false, true),
        ToolbarState::DISABLED,
        "doc 09 §Availability: no client, no operations"
    );
}

#[test]
fn toolbar_needs_a_selected_file_to_download() {
    let file = row_from_entry(&entry("notes.txt", false, 1));
    let dir = row_from_entry(&entry("docs", true, 0));
    let parent = parent_row();

    let with_file = toolbar_state(Some(&file), true, false);
    assert!(with_file.download && with_file.delete && with_file.rename);

    let with_dir = toolbar_state(Some(&dir), true, false);
    assert!(
        !with_dir.download,
        "the task transfers one file per command"
    );
    assert!(with_dir.delete && with_dir.rename);

    assert!(!toolbar_state(Some(&parent), true, false).delete);
    assert!(!toolbar_state(Some(&parent), true, false).rename);
    assert!(!toolbar_state(Some(&parent), true, false).download);

    let none = toolbar_state(None, true, false);
    assert!(none.upload && none.refresh && none.mkdir);
    assert!(!none.download && !none.delete && !none.rename);
}

#[test]
fn toolbar_cancel_follows_the_transfer_state() {
    let idle = toolbar_state(None, true, false);
    assert!(!idle.cancel);
    let busy = toolbar_state(None, true, true);
    assert!(busy.cancel);
}

// --- uri-list fallback -----------------------------------------------------

#[test]
fn parse_uri_list_reads_the_uri_list_drag_format() {
    let payload = "file:///home/alice/a.txt\r\nfile:///home/alice/b.txt\r\n";
    assert_eq!(
        parse_uri_list(payload),
        ["/home/alice/a.txt", "/home/alice/b.txt"]
    );
}

#[test]
fn parse_uri_list_skips_comments_and_blank_lines() {
    let payload = "#This is a comment\n\nfile:///tmp/one\n\n#another\nfile:///tmp/two\n";
    assert_eq!(parse_uri_list(payload), ["/tmp/one", "/tmp/two"]);
}

#[test]
fn parse_uri_list_decodes_percent_escapes() {
    let payload = "file:///home/alice/My%20File%2B%2Bx.txt";
    assert_eq!(parse_uri_list(payload), ["/home/alice/My File++x.txt"]);
}

#[test]
fn parse_uri_list_accepts_localhost_and_rejects_other_hosts() {
    assert_eq!(parse_uri_list("file://localhost/tmp/one"), ["/tmp/one"]);
    assert!(
        parse_uri_list("file://example.com/tmp/one").is_empty(),
        "a remote file cannot be uploaded"
    );
}

#[test]
fn parse_uri_list_rejects_non_file_uris_and_invalid_escapes_safely() {
    assert!(parse_uri_list("https://example.com/a.txt").is_empty());
    assert!(parse_uri_list("file://").is_empty());
    // A broken escape stays verbatim instead of losing the character.
    assert_eq!(parse_uri_list("file:///tmp/100%"), ["/tmp/100%"]);
    assert_eq!(parse_uri_list("file:///tmp/a%zzb"), ["/tmp/a%zzb"]);
    assert!(parse_uri_list("").is_empty());
}

// --- following the shell (OSC 7) --------------------------------------------

#[test]
fn the_follower_takes_the_reported_directory_percent_decoded() {
    let mut follower = DirectoryFollower::default();
    assert_eq!(
        follower.report("file://web01/srv/my%20site").as_deref(),
        Some("/srv/my site")
    );
    assert_eq!(follower.report("file://web01/").as_deref(), Some("/"));
}

#[test]
fn the_listing_moves_only_when_the_shell_changes_directory() {
    let mut view = SftpView::detached();
    assert_eq!(
        view.shell_moved("/home/alice", true).as_deref(),
        Some("/home/alice")
    );
    assert_eq!(
        view.shell_moved("/home/alice", true),
        None,
        "every prompt re-reports; browsing elsewhere must stick"
    );
    assert_eq!(
        view.shell_moved("/home/alice", false),
        None,
        "a probe finding the same directory is no move either"
    );
    assert_eq!(view.shell_moved("/tmp", false).as_deref(), Some("/tmp"));
    assert!(view.shell_moved("/home/alice", true).is_some());
}

#[test]
fn unticking_follow_keeps_the_listing_and_ticking_catches_up() {
    let mut view = SftpView::detached();
    view.attach();
    view.set_listing("/srv", &listing());
    assert!(view.following, "on by default");

    assert_eq!(view.set_following(false), None);
    assert_eq!(view.shell_moved("/var/log", false), None, "not following");
    assert!(!view.wants_probe());
    assert_eq!(view.current_path(), "/srv");

    assert_eq!(view.set_following(true).as_deref(), Some("/var/log"));
    view.set_listing("/var/log", &listing());
    view.set_following(false);
    assert_eq!(
        view.set_following(true),
        None,
        "already there: nothing to catch up"
    );
}

#[test]
fn a_shell_that_reports_its_directory_is_not_probed() {
    let mut view = SftpView::detached();
    assert!(view.wants_probe());
    view.shell_moved("/home/alice", false);
    assert!(view.wants_probe(), "probe answers keep the probes coming");
    view.shell_moved("/home/alice", true);
    assert!(!view.wants_probe(), "OSC 7 takes over");

    view.new_shell();
    assert!(view.wants_probe(), "a new shell may not report");
    assert_eq!(
        view.shell_moved("/home/alice", false).as_deref(),
        Some("/home/alice"),
        "and its directory is news"
    );
}

#[test]
fn the_follower_passes_every_report_from_its_host() {
    let mut follower = DirectoryFollower::default();
    assert!(follower.report("file://web01/home/alice").is_some());
    assert!(
        follower.report("file://web01/home/alice").is_some(),
        "repeats are SftpView::shell_moved's to drop"
    );
}

#[test]
fn the_follower_ignores_hosts_other_than_the_first() {
    let mut follower = DirectoryFollower::default();
    assert!(follower.report("file://web01/home/alice").is_some());
    assert_eq!(
        follower.report("file://db01/var/lib"),
        None,
        "a nested ssh's paths are not on this connection"
    );
    assert!(follower.report("file://WEB01/etc").is_some(), "case-blind");
}

#[test]
fn the_follower_ignores_what_is_not_a_file_uri() {
    let mut follower = DirectoryFollower::default();
    assert_eq!(follower.report("kitty-shell-cwd://web01/tmp"), None);
    assert_eq!(follower.report("file://web01"), None);
    assert_eq!(follower.report(""), None);
    assert!(
        follower.report("file://db01/tmp").is_some(),
        "junk pins no host"
    );
}

#[test]
fn a_directory_reported_before_attach_is_where_browsing_starts() {
    let mut view = SftpView::detached();
    view.start_at("/srv/www");
    assert_eq!(view.current_path(), "/srv/www");
    assert_eq!(view.path_text, "/srv/www");

    view.attach();
    view.set_listing("/srv/www", &listing());
    view.start_at("/tmp");
    assert_eq!(view.current_path(), "/srv/www", "a listing outranks it");
    assert_eq!(view.path_text, "/srv/www");
}

#[test]
fn dropped_file_paths_become_upload_targets_in_the_current_directory() {
    // Both drop formats hand the sidebar *local* paths (gio::File::path()
    // and the parsed uri-list); the upload target is the file's name inside
    // the directory the sidebar is in.
    let current = "/srv/data";
    for dropped in [
        "/home/alice/report.pdf",
        "notes.txt",
        "/home/alice/My%20Report.pdf",
    ] {
        let name = if dropped.contains('%') {
            // What parse_uri_list hands over: already decoded.
            parse_uri_list(&format!("file://{dropped}"))
                .first()
                .map(|path| file_name(path))
                .expect("dropped file has a local path")
        } else {
            file_name(dropped)
        };
        assert_eq!(
            join_path(current, &name),
            "/srv/data/report.pdf".replace("report.pdf", &name)
        );
    }
}

// --- progress --------------------------------------------------------------

#[test]
fn progress_fraction_clamps_and_survives_a_zero_total() {
    assert_eq!(progress_fraction(0, 0), 0.0, "unknown size stays empty");
    assert_eq!(progress_fraction(512, 1024), 0.5);
    assert_eq!(progress_fraction(1024, 1024), 1.0);
    assert_eq!(progress_fraction(2048, 1024), 1.0, "never exceeds full");
}

#[test]
fn progress_and_status_lines_use_human_sizes() {
    assert_eq!(progress_text(1234, 4096), "1.2 KB / 4.0 KB");
    assert_eq!(progress_text(1234, 0), "1.2 KB / ?");
    assert_eq!(
        running_status("Upload a.txt", 512, 1024),
        "Upload a.txt — 512 B / 1.0 KB"
    );
    assert_eq!(
        done_status("Upload a.txt", 2048),
        "Upload a.txt — done — 2048 bytes"
    );
    assert_eq!(
        failed_status("Upload a.txt", "permission denied"),
        "Upload a.txt — failed: permission denied"
    );
    assert_eq!(started_status("Upload a.txt"), "Upload a.txt — starting…");
}

// --- the view state machine ------------------------------------------------

#[test]
fn detached_view_is_empty_disabled_and_starts_at_the_home_path() {
    let view = SftpView::detached();
    assert!(!view.connected);
    assert!(view.rows.is_empty());
    assert_eq!(view.path, "");
    assert_eq!(view.current_path(), START_PATH);
    assert_eq!(view.toolbar(), ToolbarState::DISABLED);
    assert!(view.shown, "doc 09 starts with the sidebar shown");
}

#[test]
fn listing_updates_path_rows_and_clears_the_error() {
    let mut view = SftpView::detached();
    view.attach();
    view.set_error("subsystem unavailable");
    view.set_listing("/home/user", &listing());

    assert!(view.connected);
    assert_eq!(view.path, "/home/user");
    assert_eq!(view.path_text, "/home/user");
    assert_eq!(view.current_path(), "/home/user");
    assert_eq!(view.rows.len(), 3);
    assert!(view.error.is_none());
    assert!(
        view.toolbar().upload,
        "an attached client enables the toolbar"
    );
}

#[test]
fn a_failed_cd_shows_the_error_without_moving_the_sidebar() {
    let mut view = SftpView::detached();
    view.attach();
    view.set_listing("/home/user", &listing());
    let rows_before = view.rows.clone();

    view.set_error("permission denied");

    assert_eq!(view.error.as_deref(), Some("permission denied"));
    assert_eq!(view.path, "/home/user", "the path bar stays put");
    assert_eq!(view.path_text, "/home/user");
    assert_eq!(view.rows, rows_before, "the listing stays put");
}

#[test]
fn select_picks_the_row_the_toolbar_rules_need() {
    let mut view = SftpView::detached();
    view.attach();
    view.set_listing("/home/user", &listing());

    let file = view.rows[2].clone();
    view.select(Some(&file));
    assert!(view.toolbar().download);
    assert_eq!(
        view.selected_path(),
        Some("/home/user/notes.txt".to_string())
    );

    let parent = view.rows[0].clone();
    view.select(Some(&parent));
    assert!(
        !view.toolbar().delete,
        "the parent row is not an operation target"
    );
    assert_eq!(view.selected_path(), None);

    view.select(None);
    assert!(!view.toolbar().download);
    assert_eq!(view.selected_path(), None);
}

#[test]
fn selecting_a_directory_enables_delete_but_not_download() {
    let mut view = SftpView::detached();
    view.attach();
    view.set_listing("/home/user", &listing());
    let dir = view.rows[1].clone();
    view.select(Some(&dir));
    assert!(!view.toolbar().download);
    assert!(view.toolbar().delete);
    assert!(view.toolbar().rename);
}

#[test]
fn transfer_progress_moves_the_bar_and_the_status_line() {
    let mut view = SftpView::detached();
    view.attach();
    view.begin_transfer(7, "Upload notes.txt".to_string());

    assert_eq!(view.fraction, 0.0);
    assert!(view.toolbar().cancel, "a running transfer enables Cancel");
    assert_eq!(view.status, "Upload notes.txt — starting…");

    view.on_progress(7, 512, 1024);
    assert_eq!(view.fraction, 0.5);
    assert_eq!(view.progress_text, "512 B / 1.0 KB");
    assert_eq!(view.status, "Upload notes.txt — 512 B / 1.0 KB");

    view.end_transfer(7, &Ok(1024));
    assert_eq!(view.fraction, 1.0, "a finished transfer shows 100%");
    assert_eq!(view.status, "Upload notes.txt — done — 1024 bytes");
    assert!(!view.toolbar().cancel, "no transfer, no Cancel");
    assert!(view.transfers.is_empty());
}

#[test]
fn a_failed_transfer_reports_the_error_and_keeps_the_bar() {
    let mut view = SftpView::detached();
    view.attach();
    view.begin_transfer(3, "Download a.bin".to_string());
    view.on_progress(3, 256, 1024);
    view.end_transfer(3, &Err("No such file".to_string()));

    assert_eq!(view.status, "Download a.bin — failed: No such file");
    assert_eq!(view.fraction, 0.25, "the bar keeps the last sample");
    assert!(view.transfers.is_empty());
}

#[test]
fn concurrent_transfers_show_the_last_started_one_and_cancel_targets_it() {
    let mut view = SftpView::detached();
    view.attach();
    view.begin_transfer(1, "Upload a".to_string());
    view.begin_transfer(2, "Upload b".to_string());

    assert_eq!(view.cancel_target(), Some(2), "last-updated wins");
    view.on_progress(1, 512, 1024);
    view.on_progress(2, 256, 1024);
    assert_eq!(view.fraction, 0.25, "the bar follows the last sample");

    view.end_transfer(2, &Ok(1024));
    assert_eq!(view.cancel_target(), Some(1), "the older one is still live");
    assert!(view.toolbar().cancel);
}

#[test]
fn progress_for_an_unknown_transfer_is_ignored() {
    let mut view = SftpView::detached();
    view.attach();
    view.on_progress(99, 512, 1024);
    assert_eq!(view.fraction, 0.0);
    assert_eq!(view.status, "");
    view.end_transfer(99, &Ok(10));
    assert!(view.transfers.is_empty());
}

#[test]
fn only_the_first_show_counts() {
    let mut view = SftpView::detached();
    view.shown = false;
    assert!(view.show(), "the first show");
    assert!(view.shown);
    assert!(!view.show(), "already shown");
    assert!(view.shown);
}

// --- availability ----------------------------------------------------------

#[test]
fn a_connect_attaches_at_once_unless_sftp_on_connect_is_off_and_hidden() {
    // (sftp_on_connect, sidebar shown) → attach now?
    assert!(attach_on_connect(true, true));
    assert!(
        attach_on_connect(true, false),
        "a sidebar built while the setting was off still attaches"
    );
    assert!(attach_on_connect(false, true), "the user already opened it");
    assert!(
        !attach_on_connect(false, false),
        "no SFTP channel until first shown"
    );
}

#[test]
fn sftp_on_connect_names_its_settings_row() {
    assert_eq!(SFTP_ON_CONNECT_SETTING, "sftp_on_connect");
}

// --- constants -------------------------------------------------------------

#[test]
fn sidebar_messages_name_the_condition_they_report() {
    assert!(NOT_CONNECTED.contains("SSH connection"));
    assert!(
        SESSION_ENDED.contains("reconnect"),
        "doc 09 asks for a prompt"
    );
    assert!(!COMMAND_FAILED.is_empty());
    assert!(
        JUMPED_UNAVAILABLE.contains("jumped"),
        "a jumped tab must show the v1 limitation, not a spinner"
    );
}

#[test]
fn jumped_sessions_show_the_limitation_instead_of_attaching() {
    // Doc 11 v1: no SFTP task ever starts over a jumped connection.
    let mut view = SftpView::detached();
    view.mark_jump_unsupported();
    assert!(!view.connected);
    assert_eq!(view.error.as_deref(), Some(JUMPED_UNAVAILABLE));
    assert_eq!(view.toolbar(), ToolbarState::DISABLED);

    // Even an attached view falls back to the notice, never a listing.
    let mut view = SftpView::detached();
    view.attach();
    view.set_listing("/home/user", &listing());
    view.mark_jump_unsupported();
    assert!(!view.connected);
    assert_eq!(view.error.as_deref(), Some(JUMPED_UNAVAILABLE));
    assert_eq!(view.toolbar(), ToolbarState::DISABLED);
}
