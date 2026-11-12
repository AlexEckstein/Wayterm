use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;

async fn expect_err<T: std::fmt::Debug>(rx: oneshot::Receiver<Result<T, String>>) -> String {
    timeout(Duration::from_secs(5), rx)
        .await
        .expect("reply timed out")
        .expect("task exited without replying")
        .expect_err("expected an error reply")
}

#[tokio::test]
async fn failed_session_open_replies_error_and_reopens_on_every_command() {
    let opens = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<SftpCommand>(16);
    let task = tokio::spawn({
        let opens = Arc::clone(&opens);
        run_sftp_task(rx, CancellationToken::new(), move || {
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

/// A session that fails to open is logged with the opener's error.
#[tokio::test]
async fn a_failed_session_open_is_logged_as_a_warning() {
    let (buffer, guard) = crate::logging::tests::capture(None);
    let (tx, rx) = mpsc::channel::<SftpCommand>(4);
    let task = tokio::spawn(run_sftp_task(rx, CancellationToken::new(), || async {
        Err::<SftpSession, String>("subsystem unavailable".to_string())
    }));

    let (reply, reply_rx) = oneshot::channel();
    tx.send(SftpCommand::Cd {
        path: ".".into(),
        reply,
    })
    .await
    .unwrap();
    expect_err(reply_rx).await;
    drop(tx);
    timeout(Duration::from_secs(5), task)
        .await
        .expect("task must exit when all senders drop")
        .expect("task panicked");
    drop(guard);

    let output = buffer.contents();
    let line = output
        .lines()
        .find(|line| line.contains("SFTP session failed to open"))
        .unwrap_or_else(|| panic!("no failure line in: {output}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("subsystem unavailable"), "{line}");
}

#[tokio::test]
async fn task_exits_only_when_the_last_sender_drops() {
    let (tx, rx) = mpsc::channel::<SftpCommand>(4);
    let tx2 = tx.clone();
    let mut task = tokio::spawn(run_sftp_task(rx, CancellationToken::new(), || async {
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

#[tokio::test]
async fn shutdown_interrupts_a_stuck_command_and_discards_the_queue() {
    let opens = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<SftpCommand>(4);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn({
        let opens = Arc::clone(&opens);
        run_sftp_task(rx, shutdown.clone(), move || {
            opens.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<SftpSession, String>>()
        })
    });
    let (reply, stuck) = oneshot::channel();
    tx.send(SftpCommand::Cd {
        path: ".".into(),
        reply,
    })
    .await
    .unwrap();
    let (reply, queued) = oneshot::channel();
    tx.send(SftpCommand::Delete {
        path: "a".into(),
        reply,
    })
    .await
    .unwrap();
    while opens.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }

    shutdown.cancel();
    timeout(Duration::from_secs(5), task)
        .await
        .expect("task must exit on shutdown while a sender lives")
        .expect("task panicked");
    assert!(stuck.await.is_err(), "the stuck command must be dropped");
    assert!(queued.await.is_err(), "a queued command must not run");
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    drop(tx);
}
