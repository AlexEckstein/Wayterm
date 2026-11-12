//! Widget checks; they need a display and run from `ui::gtk_tests`.

use super::*;
use crate::sftp::DirEntry;

fn entry(name: &str, is_dir: bool) -> DirEntry {
    DirEntry {
        name: name.to_string(),
        size: 2048,
        modified: "2026-10-11 12:00".to_string(),
        permissions: "rw-r--r--".to_string(),
        is_dir,
    }
}

/// The listing shows every row under doc 07's columns; selection feeds
/// the toolbar rules, and a re-render (which clears the selection)
/// does not re-enter the state borrow.
pub(in crate::ui) fn listing_renders_and_selection_drives_toolbar() {
    let sidebar = SftpSidebar::new();
    let inner = &sidebar.inner;
    let titles: Vec<String> = (0..inner.column_view.columns().n_items())
        .filter_map(|index| inner.column_view.columns().item(index))
        .filter_map(|column| column.downcast::<gtk4::ColumnViewColumn>().ok())
        .filter_map(|column| column.title().map(|title| title.to_string()))
        .collect();
    assert_eq!(titles, ["Name", "Size", "Modified", "Permissions"]);

    inner.state.borrow_mut().attach();
    inner
        .state
        .borrow_mut()
        .set_listing("/home/u", &[entry("docs", true), entry("a.txt", false)]);
    inner.render();
    assert_eq!(inner.model.n_items(), 3, "`..`, docs, a.txt");

    inner.selection.set_selected(2);
    assert_eq!(
        inner.state.borrow().selected_path().as_deref(),
        Some("/home/u/a.txt")
    );
    assert!(inner.buttons.download.is_sensitive());

    inner.selection.set_selected(0);
    assert_eq!(inner.state.borrow().selected_path(), None);
    assert!(!inner.buttons.download.is_sensitive());

    inner.selection.set_selected(2);
    inner.render();
    assert_eq!(inner.state.borrow().selected_path(), None);
    assert!(!inner.buttons.download.is_sensitive());
}

fn visible_columns(sidebar: &SftpSidebar) -> Vec<Column> {
    let columns = &sidebar.inner.columns;
    let shown = columns.iter().filter(|(_, column, _)| column.is_visible());
    shown.map(|(column, _, _)| *column).collect()
}

/// Every header carries the menu of optional columns. Unticking one
/// hides it, stores the choice, and reaches another tab's sidebar when
/// that is shown.
pub(in crate::ui) fn the_header_menu_picks_the_columns() {
    let saved = Rc::new(RefCell::new(None));
    save_columns_with({
        let saved = saved.clone();
        move |columns| *saved.borrow_mut() = Some(columns.to_setting())
    });
    install_columns(Columns::ALL);
    let sidebar = SftpSidebar::new();
    let other = SftpSidebar::new();
    let inner = &sidebar.inner;
    let view_columns = inner.column_view.columns();
    for index in 0..view_columns.n_items() {
        let column = view_columns
            .item(index)
            .and_downcast::<gtk4::ColumnViewColumn>();
        let menu = column.and_then(|column| column.header_menu());
        assert_eq!(menu.map(|menu| menu.n_items()), Some(3), "column {index}");
    }
    assert_eq!(visible_columns(&sidebar), Column::ALL);

    let permissions = &inner.columns[2].2;
    assert_eq!(permissions.name(), "permissions");
    permissions.change_state(&false.to_variant());
    assert_eq!(visible_columns(&sidebar), [Column::Size, Column::Modified]);
    assert_eq!(permissions.state(), Some(false.to_variant()));
    assert_eq!(saved.borrow().as_deref(), Some("size,modified"));

    assert_eq!(visible_columns(&other), Column::ALL, "not in view yet");
    other.show();
    assert_eq!(visible_columns(&other), [Column::Size, Column::Modified]);

    permissions.change_state(&true.to_variant());
    assert_eq!(visible_columns(&sidebar), Column::ALL);
    assert_eq!(saved.borrow().as_deref(), Some("size,modified,permissions"));
    save_columns_with(|_| {});
}

/// Run the main loop until `rx` has a command (the sidebar sends from
/// idle and reply futures).
fn next_command(rx: &mut mpsc::Receiver<SftpCommand>) -> SftpCommand {
    let context = glib::MainContext::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(command) = rx.try_recv() {
            return command;
        }
        assert!(std::time::Instant::now() < deadline, "no command was sent");
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn settle() {
    let context = glib::MainContext::default();
    while context.iteration(false) {}
}

/// Run the main loop until `done`, or for as long as a save takes to
/// be noticed and looked at several times over.
fn spin(done: impl Fn() -> bool) {
    let context = glib::MainContext::default();
    let deadline = std::time::Instant::now() + SETTLE * 5;
    while !done() && std::time::Instant::now() < deadline {
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// A sidebar over a fake task, its opened files' copies under `root`.
fn editing_sidebar(root: &Path) -> (SftpSidebar, mpsc::Receiver<SftpCommand>) {
    let (tx, rx) = mpsc::channel(8);
    let sidebar = SftpSidebar::new();
    let client = SftpClient::from_parts(tx, tokio_util::sync::CancellationToken::new());
    sidebar.attach_client(client);
    *sidebar.inner.edit_root.borrow_mut() = Some(root.join("sftp-edit"));
    (sidebar, rx)
}

fn expect_download(rx: &mut mpsc::Receiver<SftpCommand>) -> (PathBuf, Reply) {
    match next_command(rx) {
        SftpCommand::Download {
            local_path, reply, ..
        } => (PathBuf::from(local_path), reply),
        _ => panic!("no download was sent"),
    }
}

fn expect_upload(rx: &mut mpsc::Receiver<SftpCommand>) -> (PathBuf, String, Reply) {
    match next_command(rx) {
        SftpCommand::Upload {
            local_path,
            remote_path,
            reply,
            ..
        } => (PathBuf::from(local_path), remote_path, reply),
        _ => panic!("no upload was sent"),
    }
}

type Reply = oneshot::Sender<Result<u64, String>>;

/// Open `/srv/notes.txt` from a listing by activating its row, serve
/// the download with `content`, and return the local copy.
fn open_notes(
    sidebar: &SftpSidebar,
    rx: &mut mpsc::Receiver<SftpCommand>,
    content: &[u8],
) -> PathBuf {
    let inner = &sidebar.inner;
    inner.state.borrow_mut().attach();
    inner
        .state
        .borrow_mut()
        .set_listing("/srv", &[entry("notes.txt", false)]);
    inner.render();
    inner.column_view.emit_by_name::<()>("activate", &[&1u32]);
    let (staged, reply) = expect_download(rx);
    assert!(inner.launched.borrow().is_empty(), "opened before it came");
    std::fs::write(&staged, content).unwrap();
    reply.send(Ok(content.len() as u64)).unwrap();
    settle();
    assert!(!staged.exists(), "the download was not moved to the copy");
    let (local, ask) = inner.launched.borrow()[0].clone();
    assert!(!ask);
    local
}

/// Activating a file downloads it to a local copy and opens that; a
/// save of the copy is uploaded over the remote file, an untouched
/// copy is not, and the copy goes when the sidebar detaches.
pub(in crate::ui) fn an_opened_file_uploads_each_save() {
    let tmp = tempfile::tempdir().unwrap();
    let (sidebar, mut rx) = editing_sidebar(tmp.path());
    let inner = &sidebar.inner;
    let local = open_notes(&sidebar, &mut rx, b"from the server");
    assert!(local.starts_with(tmp.path()) && local.ends_with("notes.txt"));
    assert_eq!(std::fs::read(&local).unwrap(), b"from the server");

    // An editor that saves by renaming a new file over the old one.
    let scratch = local.with_file_name("notes.txt~");
    std::fs::write(&scratch, b"edited in the editor").unwrap();
    std::fs::rename(&scratch, &local).unwrap();
    let (sent, remote, reply) = expect_upload(&mut rx);
    assert_eq!((sent, remote.as_str()), (local.clone(), "/srv/notes.txt"));
    reply.send(Ok(20)).unwrap();
    assert!(
        matches!(next_command(&mut rx), SftpCommand::Cd { .. }),
        "the listing shows the new size"
    );

    // Nothing changed since: the monitor's echoes upload nothing.
    spin(|| false);
    assert!(rx.try_recv().is_err());
    assert_eq!(inner.launched.borrow().len(), 1);

    sidebar.detach();
    assert!(!local.exists(), "the copy outlived the session");
    settle();
}

/// Opening a file again never loses a save: one not yet uploaded is
/// sent instead of downloaded over, and a download that fails leaves
/// the copy as it was.
pub(in crate::ui) fn opening_again_keeps_an_unsent_save() {
    let tmp = tempfile::tempdir().unwrap();
    let (sidebar, mut rx) = editing_sidebar(tmp.path());
    let inner = &sidebar.inner;
    let local = open_notes(&sidebar, &mut rx, b"from the server");

    std::fs::write(&local, b"saved a moment ago").unwrap();
    inner.open_file("/srv/notes.txt".into(), false);
    let (sent, _, reply) = expect_upload(&mut rx);
    assert_eq!(sent, local);
    assert_eq!(std::fs::read(&local).unwrap(), b"saved a moment ago");
    assert_eq!(inner.launched.borrow().len(), 2, "the copy is shown");
    reply.send(Ok(18)).unwrap();
    assert!(matches!(next_command(&mut rx), SftpCommand::Cd { .. }));

    inner.open_file("/srv/notes.txt".into(), false);
    let (staged, reply) = expect_download(&mut rx);
    assert_ne!(staged, local, "downloaded straight into the copy");
    std::fs::write(&staged, b"half of").unwrap();
    reply.send(Err("connection lost".into())).unwrap();
    settle();
    assert_eq!(std::fs::read(&local).unwrap(), b"saved a moment ago");
    assert!(!staged.exists());
    assert_eq!(inner.launched.borrow().len(), 2, "a failed download opens");

    spin(|| false);
    assert!(rx.try_recv().is_err(), "nothing changed, nothing is sent");
    sidebar.detach();
    settle();
}

/// The sidebar replacing one whose session ended keeps its opened
/// files: a save made while there is no session is uploaded once the
/// new sidebar has a task, and the copies go with the new sidebar.
pub(in crate::ui) fn opened_files_survive_a_reconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, mut rx) = editing_sidebar(tmp.path());
    let local = open_notes(&old, &mut rx, b"from the server");

    let next = SftpSidebar::new();
    next.adopt_open_files(&old);
    old.detach();
    drop(old);
    assert!(local.exists(), "the copy went with the old session");

    std::fs::write(&local, b"saved while disconnected").unwrap();
    spin(|| next.inner.state.borrow().error.is_some());
    assert_eq!(
        next.inner.state.borrow().error.as_deref(),
        Some(NOT_CONNECTED)
    );

    let (tx, mut rx) = mpsc::channel(8);
    let shutdown = tokio_util::sync::CancellationToken::new();
    next.attach_client(SftpClient::from_parts(tx, shutdown));
    next.inner.send_unsent_edits();
    let (sent, remote, reply) = expect_upload(&mut rx);
    assert_eq!((sent, remote.as_str()), (local.clone(), "/srv/notes.txt"));
    reply.send(Ok(24)).unwrap();
    assert!(matches!(next_command(&mut rx), SftpCommand::Cd { .. }));

    next.detach();
    assert!(!local.exists(), "the copy outlived the tab");
    settle();
}

/// A file whose download fails is not opened, and keeps no copy.
pub(in crate::ui) fn a_failed_open_leaves_no_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let (sidebar, mut rx) = editing_sidebar(tmp.path());
    let inner = &sidebar.inner;
    inner.open_file("/srv/secret".into(), true);
    let (staged, reply) = expect_download(&mut rx);
    let dir = staged.with_extension("");
    assert!(dir.is_dir());
    std::fs::write(&staged, b"half of").unwrap();
    reply.send(Err("permission denied".into())).unwrap();
    settle();
    assert!(inner.launched.borrow().is_empty());
    assert!(inner.edits.borrow().is_empty());
    assert!(!dir.exists() && !staged.exists(), "the failed copy stayed");
    assert!(inner.state.borrow().status.contains("permission denied"));
}

/// "Follow terminal directory" is the bottom row, ticked. Unticked, the
/// shell's moves leave the sidebar where it is; ticking it again
/// catches up with the shell.
pub(in crate::ui) fn follow_checkbox_toggles_following() {
    let sidebar = SftpSidebar::new();
    let inner = &sidebar.inner;
    assert_eq!(
        inner.container.last_child().as_ref(),
        Some(inner.follow_check.upcast_ref::<gtk4::Widget>())
    );
    assert!(inner.follow_check.is_active());
    assert!(sidebar.is_following());

    sidebar.follow_shell("/srv".into());
    assert_eq!(sidebar.path_text(), "/srv");

    inner.follow_check.set_active(false);
    assert!(!sidebar.is_following());
    sidebar.follow_shell("/var/log".into());
    assert_eq!(sidebar.path_text(), "/srv", "unticked: stays");

    sidebar.set_following(true);
    assert!(inner.follow_check.is_active());
    assert_eq!(sidebar.path_text(), "/var/log", "ticked: catches up");
}

/// A local tab's sidebar reads nothing until it is shown. Then it
/// lists the shell's directory from the real file task, takes a typed
/// path from where it is, opens a file where that is, follows the
/// shell, and says so once no shell is behind it.
pub(in crate::ui) fn a_local_sidebar_browses_this_machines_files() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _entered = runtime.enter();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("docs")).unwrap();
    std::fs::write(tmp.path().join("docs/notes.txt"), b"here").unwrap();
    let start = tmp.path().to_string_lossy().into_owned();
    let docs = format!("{start}/docs");

    let sidebar = SftpSidebar::local(false);
    let inner = &sidebar.inner;
    sidebar.attach_local_on_connect(&start);
    assert!(inner.attached.borrow().is_none(), "browsing unseen");
    assert_eq!(sidebar.path_text(), start);

    let names = || -> Vec<String> {
        let state = inner.state.borrow();
        state.rows.iter().map(|row| row.name.clone()).collect()
    };
    let listed = |path: &str| inner.state.borrow().path == path;
    sidebar.show();
    spin(|| listed(&start));
    assert_eq!(names(), [PARENT_ROW, "docs"]);

    inner.cd("./docs/".into());
    spin(|| listed(&docs));
    assert_eq!(sidebar.path_text(), docs);
    assert_eq!(names(), [PARENT_ROW, "notes.txt"]);

    inner.column_view.emit_by_name::<()>("activate", &[&1u32]);
    assert_eq!(
        *inner.launched.borrow(),
        [(tmp.path().join("docs/notes.txt"), false)]
    );
    assert!(inner.edits.borrow().is_empty(), "a copy of a local file");

    inner.column_view.emit_by_name::<()>("activate", &[&0u32]);
    spin(|| listed(&start));
    assert_eq!(sidebar.path_text(), start);
    sidebar.follow_shell(docs.clone());
    spin(|| listed(&docs));
    assert_eq!(sidebar.path_text(), docs);

    sidebar.detach();
    inner.cd(start);
    assert_eq!(
        inner.state.borrow().error.as_deref(),
        Some(SHELL_NOT_RUNNING)
    );
    settle();
}

/// A sidebar detached mid-transfer shuts its task down and closes its
/// command channel, though the transfer's futures still hold the
/// sidebar.
pub(in crate::ui) fn detaching_mid_transfer_ends_the_task() {
    let (tx, mut rx) = mpsc::channel(8);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let sidebar = SftpSidebar::new();
    sidebar.attach_client(SftpClient::from_parts(tx, shutdown.clone()));
    sidebar.inner.download("/srv/f".into(), "/tmp/f".into());
    let Ok(SftpCommand::Download { reply, .. }) = rx.try_recv() else {
        panic!("no download sent");
    };

    sidebar.detach();
    drop(sidebar);
    assert!(shutdown.is_cancelled(), "the task was not shut down");
    assert_eq!(
        rx.try_recv().err(),
        Some(mpsc::error::TryRecvError::Disconnected),
        "the transfer keeps the task alive"
    );
    drop(reply);
    let context = glib::MainContext::default();
    while context.iteration(false) {}
}

/// The bar and Cancel show only while a transfer runs, its result
/// replaces them, and neither progress nor the result moves the
/// selection.
pub(in crate::ui) fn the_progress_row_shows_only_while_transferring() {
    let (tx, mut rx) = mpsc::channel(8);
    let sidebar = SftpSidebar::new();
    let inner = &sidebar.inner;
    sidebar.attach_client(SftpClient::from_parts(
        tx,
        tokio_util::sync::CancellationToken::new(),
    ));
    inner.state.borrow_mut().attach();
    inner
        .state
        .borrow_mut()
        .set_listing("/srv", &[entry("a.txt", false)]);
    inner.render();
    settle();
    assert!(!inner.progress_row.is_visible(), "idle shows no bar");
    assert!(!inner.status_label.is_visible(), "idle shows no status");

    inner.selection.set_selected(1);
    inner.download("/srv/a.txt".into(), "/tmp/a.txt".into());
    let SftpCommand::Download {
        progress, reply, ..
    } = next_command(&mut rx)
    else {
        panic!("no download sent");
    };
    assert!(inner.progress_row.is_visible());
    assert!(!inner.status_label.is_visible());

    progress
        .try_send(Progress {
            transferred: 1024,
            total: 4096,
        })
        .unwrap();
    spin(|| inner.progress_bar.fraction() > 0.0);
    assert_eq!(
        inner.progress_bar.text().as_deref(),
        Some("Download a.txt — 1.0 KB / 4.0 KB")
    );
    assert_eq!(inner.selection.selected(), 1, "a sample kept the selection");

    reply.send(Err("permission denied".into())).unwrap();
    spin(|| inner.status_label.is_visible());
    assert!(
        !inner.progress_row.is_visible(),
        "the result took its place"
    );
    assert_eq!(
        inner.status_label.text(),
        "Download a.txt — failed: permission denied"
    );
    assert_eq!(
        inner.selection.selected(),
        1,
        "the result kept the selection"
    );
    settle();
}

/// Dropping the last handle frees the sidebar, and with it the SFTP
/// task's command sender, so the task ends (doc 07 §Connection model).
pub(in crate::ui) fn a_dropped_sidebar_is_freed() {
    let local = SftpSidebar::local(false);
    local.attach_local_on_connect("/");
    for sidebar in [SftpSidebar::new(), SftpSidebar::hidden(), local] {
        let inner = Rc::downgrade(&sidebar.inner);
        drop(sidebar);
        assert!(
            inner.upgrade().is_none(),
            "the sidebar outlived its handles"
        );
    }
}
