use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::time::timeout;

use super::*;

fn entry(name: &str, is_dir: bool) -> DirEntry {
    DirEntry {
        name: name.to_string(),
        size: 0,
        modified: String::new(),
        permissions: String::new(),
        is_dir,
    }
}

async fn expect_err<T: std::fmt::Debug>(rx: oneshot::Receiver<Result<T, String>>) -> String {
    timeout(Duration::from_secs(5), rx)
        .await
        .expect("reply timed out")
        .expect("task exited without replying")
        .expect_err("expected an error reply")
}

#[test]
fn dir_entries_sort_directories_first_then_case_insensitive() {
    let mut entries = vec![
        entry("beta", false),
        entry("Alpha", false),
        entry("gamma", true),
        entry("Beta", true),
        entry("alpha", true),
    ];
    sort_entries(&mut entries);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["alpha", "Beta", "gamma", "Alpha", "beta"]);
}

#[test]
fn format_perms_masks_file_type_bits_to_rwx_style() {
    assert_eq!(format_perms(0o100644), "rw-r--r--");
    assert_eq!(format_perms(0o40755), "rwxr-xr-x");
    assert_eq!(format_perms(0o100750), "rwxr-x---");
    assert_eq!(format_perms(0o100777), "rwxrwxrwx");
    assert_eq!(format_perms(0), "---------");
    // setuid/setgid/sticky are not part of the nine-character display
    assert_eq!(format_perms(0o104755), "rwxr-xr-x");
}

#[test]
fn format_timestamp_formats_utc_minutes() {
    assert_eq!(format_timestamp(0), "1970-01-01 00:00");
    assert_eq!(format_timestamp(86_399), "1970-01-01 23:59");
    assert_eq!(format_timestamp(86_400), "1970-01-02 00:00");
    assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13");
    // Past the year 2100, where naive date math goes wrong
    assert_eq!(format_timestamp(u32::MAX), "2106-02-07 06:28");
}

#[test]
fn transfer_registry_cancels_by_id_and_forgets_finished_transfers() {
    let mut registry = TransferRegistry::default();
    let token = registry.register(7);
    assert!(!token.is_cancelled());
    assert!(!registry.cancel(99), "unknown id must be a no-op");

    assert!(registry.cancel(7));
    assert!(token.is_cancelled());

    registry.finish(7);
    assert!(!registry.cancel(7), "finished transfer must be forgotten");
}

#[test]
fn transfer_registry_cancel_all_aborts_every_live_transfer() {
    let mut registry = TransferRegistry::default();
    let first = registry.register(1);
    let second = registry.register(2);
    registry.cancel_all();
    assert!(first.is_cancelled());
    assert!(second.is_cancelled());
    assert!(registry.active.is_empty());
}

#[tokio::test]
async fn failed_session_open_replies_error_and_reopens_on_every_command() {
    let opens = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<SftpCommand>(16);
    let task = tokio::spawn({
        let opens = Arc::clone(&opens);
        run_sftp_task(rx, move || {
            opens.fetch_add(1, Ordering::SeqCst);
            async { Err::<SftpSession, String>("subsystem unavailable".to_string()) }
        })
    });

    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Cd {
        path: ".".into(),
        reply,
    })
    .await
    .unwrap();
    let err = expect_err(reply_rx).await;
    assert!(err.contains("subsystem unavailable"), "got: {err}");

    let (progress, _keep) = mpsc::channel(4);
    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Download {
        transfer_id: 1,
        remote_path: "/remote".into(),
        local_path: "/local".into(),
        progress,
        reply,
    })
    .await
    .unwrap();
    let err = expect_err(reply_rx).await;
    assert!(err.contains("subsystem unavailable"), "got: {err}");

    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Mkdir {
        path: "/tmp/x".into(),
        reply,
    })
    .await
    .unwrap();
    expect_err(reply_rx).await;

    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Rename {
        from: "a".into(),
        to: "b".into(),
        reply,
    })
    .await
    .unwrap();
    expect_err(reply_rx).await;

    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Delete {
        path: "a".into(),
        reply,
    })
    .await
    .unwrap();
    expect_err(reply_rx).await;

    let (progress, _keep) = mpsc::channel(4);
    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Upload {
        transfer_id: 2,
        local_path: "/local".into(),
        remote_path: "/remote".into(),
        progress,
        reply,
    })
    .await
    .unwrap();
    expect_err(reply_rx).await;

    assert_eq!(
        opens.load(Ordering::SeqCst),
        6,
        "every command after a failure must retry the lazy session open"
    );

    // Cancel needs no session — and no reply.
    tx.send(SftpCommand::Cancel { transfer_id: 1 })
        .await
        .unwrap();
    drop(tx);
    timeout(Duration::from_secs(5), task)
        .await
        .expect("task must exit when all senders drop")
        .expect("task panicked");
    assert_eq!(
        opens.load(Ordering::SeqCst),
        6,
        "Cancel and channel close must not open a session"
    );
}

#[tokio::test]
async fn task_exits_only_when_the_last_sender_drops() {
    let (tx, rx) = mpsc::channel::<SftpCommand>(4);
    let tx2 = tx.clone();
    let mut task = tokio::spawn(run_sftp_task(rx, || async {
        Err::<SftpSession, String>("no server".to_string())
    }));

    drop(tx);
    let exited_early = timeout(Duration::from_millis(100), &mut task).await.is_ok();
    assert!(
        !exited_early,
        "task must keep running while any sender is alive"
    );

    drop(tx2);
    timeout(Duration::from_secs(5), task)
        .await
        .expect("task must exit once the last sender drops")
        .expect("task panicked");
}
