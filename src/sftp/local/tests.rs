use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;
use crate::sftp::SftpClient;

async fn answer<T>(rx: oneshot::Receiver<Result<T, String>>) -> Result<T, String> {
    timeout(Duration::from_secs(5), rx)
        .await
        .expect("reply timed out")
        .expect("task exited without replying")
}

fn text_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn list(client: &SftpClient, path: &Path) -> Result<Vec<DirEntry>, String> {
    let (reply, rx) = oneshot::channel();
    let path = text_of(path);
    client
        .request(SftpCommand::Cd { path, reply })
        .await
        .unwrap();
    answer(rx).await
}

async fn names(client: &SftpClient, path: &Path) -> Vec<String> {
    let entries = list(client, path).await.expect("a listing");
    entries.into_iter().map(|entry| entry.name).collect()
}

/// `Upload` when `upload`, else `Download`: either way `from` is copied to `to`.
async fn copy(client: &SftpClient, upload: bool, from: &Path, to: &Path) -> Result<u64, String> {
    let (progress, _samples) = mpsc::channel(64);
    let (reply, rx) = oneshot::channel();
    let transfer_id = client.next_transfer_id();
    let cmd = if upload {
        SftpCommand::Upload {
            transfer_id,
            local_path: text_of(from),
            remote_path: text_of(to),
            progress,
            reply,
        }
    } else {
        SftpCommand::Download {
            transfer_id,
            remote_path: text_of(from),
            local_path: text_of(to),
            progress,
            reply,
        }
    };
    client.request(cmd).await.unwrap();
    answer(rx).await
}

#[tokio::test]
async fn a_listing_puts_directories_first_and_follows_links() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::create_dir(dir.join("zeta")).unwrap();
    std::fs::write(dir.join("Beta.txt"), b"12345").unwrap();
    std::fs::write(dir.join("alpha.txt"), b"").unwrap();
    std::os::unix::fs::symlink(dir.join("zeta"), dir.join("link")).unwrap();
    std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("dangling")).unwrap();
    std::fs::set_permissions(dir.join("Beta.txt"), PermissionsExt::from_mode(0o640)).unwrap();

    let client = SftpClient::local();
    let entries = list(&client, dir).await.unwrap();
    let shown: Vec<(&str, bool)> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.is_dir))
        .collect();
    assert_eq!(
        shown,
        [
            ("link", true),
            ("zeta", true),
            ("alpha.txt", false),
            ("Beta.txt", false),
            ("dangling", false),
        ]
    );
    let beta = entries.iter().find(|e| e.name == "Beta.txt").unwrap();
    assert_eq!(beta.size, 5);
    assert_eq!(beta.permissions, "rw-r-----");
    assert_eq!(beta.modified.len(), "2026-01-01 00:00".len());

    let err = list(&client, &dir.join("missing")).await.unwrap_err();
    assert!(err.contains("No such file"), "{err}");
}

#[tokio::test]
async fn folders_are_made_renamed_and_deleted() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    let client = SftpClient::local();
    let simple = |cmd: fn(String, String, oneshot::Sender<Result<(), String>>) -> SftpCommand,
                  a: &str,
                  b: &str| {
        let (reply, rx) = oneshot::channel();
        let cmd = cmd(text_of(&dir.join(a)), text_of(&dir.join(b)), reply);
        let client = client.clone();
        async move {
            client.request(cmd).await.unwrap();
            answer(rx).await
        }
    };
    let mkdir = |path, _, reply| SftpCommand::Mkdir { path, reply };
    let rename = |from, to, reply| SftpCommand::Rename { from, to, reply };
    let delete = |path, _, reply| SftpCommand::Delete { path, reply };

    simple(mkdir, "one", "").await.unwrap();
    simple(mkdir, "one", "").await.unwrap_err();
    std::fs::write(dir.join("one/file"), b"kept").unwrap();
    std::fs::write(dir.join("other"), b"other").unwrap();

    let err = simple(rename, "one/file", "other").await.unwrap_err();
    assert!(err.ends_with("already exists"), "{err}");
    assert_eq!(std::fs::read(dir.join("other")).unwrap(), b"other");
    simple(rename, "one", "two").await.unwrap();
    assert_eq!(names(&client, dir).await, ["two", "other"]);

    simple(delete, "two", "").await.unwrap_err();
    simple(delete, "two/file", "").await.unwrap();
    simple(delete, "two", "").await.unwrap();
    // A link goes, not what it points to.
    std::os::unix::fs::symlink(dir.join("other"), dir.join("link")).unwrap();
    simple(delete, "link", "").await.unwrap();
    assert_eq!(names(&client, dir).await, ["other"]);
}

#[tokio::test]
async fn upload_and_download_copy_a_file_as_cp_does() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    let mode = |name: &str| {
        let permissions = std::fs::metadata(dir.join(name)).unwrap().permissions();
        permissions.mode() & 0o777
    };
    let body = vec![7u8; 100_000];
    std::fs::write(dir.join("script"), &body).unwrap();
    // Owner bits only, so no umask changes what a copy gets.
    std::fs::set_permissions(dir.join("script"), PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    let client = SftpClient::local();

    for (upload, target) in [(true, "sub/up"), (false, "sub/down")] {
        let copied = copy(&client, upload, &dir.join("script"), &dir.join(target)).await;
        assert_eq!(copied, Ok(100_000));
        assert_eq!(std::fs::read(dir.join(target)).unwrap(), body);
        assert_eq!(mode(target), 0o700, "a new file gets the source's mode");
    }

    std::fs::write(dir.join("old"), b"a longer body than the new one").unwrap();
    std::fs::set_permissions(dir.join("old"), PermissionsExt::from_mode(0o600)).unwrap();
    std::fs::write(dir.join("new"), b"short").unwrap();
    std::fs::set_permissions(dir.join("new"), PermissionsExt::from_mode(0o400)).unwrap();
    let copied = copy(&client, true, &dir.join("new"), &dir.join("old")).await;
    assert_eq!(copied, Ok(5));
    assert_eq!(std::fs::read(dir.join("old")).unwrap(), b"short");
    assert_eq!(mode("old"), 0o600, "a replaced file keeps its own mode");
}

/// A directory, and a FIFO, which an ordinary open would wait on forever.
#[tokio::test]
async fn only_regular_files_are_copied() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::write(dir.join("file"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(dir.join("pipe"))
        .status()
        .expect("mkfifo");
    assert!(made.success());
    let client = SftpClient::local();

    for source in ["sub", "pipe"] {
        let err = copy(&client, true, &dir.join(source), &dir.join("copy")).await;
        assert!(
            err.unwrap_err().ends_with("is not a regular file"),
            "{source}"
        );
        assert!(!dir.join("copy").exists());
    }
    for target in ["sub", "pipe"] {
        let err = copy(&client, false, &dir.join("file"), &dir.join(target)).await;
        assert!(
            err.unwrap_err().ends_with("is not a regular file"),
            "{target}"
        );
    }
}

/// Creating the target first would empty the file being read.
#[tokio::test]
async fn a_file_is_never_copied_onto_itself() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::write(dir.join("notes"), b"precious").unwrap();
    std::os::unix::fs::symlink(dir.join("notes"), dir.join("alias")).unwrap();
    std::fs::hard_link(dir.join("notes"), dir.join("twin")).unwrap();
    let client = SftpClient::local();

    for target in ["notes", "alias", "twin"] {
        let err = copy(&client, true, &dir.join("notes"), &dir.join(target)).await;
        assert!(err.unwrap_err().ends_with("are the same file"), "{target}");
        assert_eq!(std::fs::read(dir.join("notes")).unwrap(), b"precious");
    }
}

#[tokio::test]
async fn shutdown_ends_the_task_unanswered() {
    let root = tempfile::tempdir().unwrap();
    let client = SftpClient::local();
    list(&client, root.path()).await.unwrap();
    client.shutdown();

    let (reply, rx) = oneshot::channel();
    let path = text_of(root.path());
    // The task may already be gone, and its channel closed.
    let _ = client.request(SftpCommand::Cd { path, reply }).await;
    let gone = timeout(Duration::from_secs(5), rx).await.expect("no end");
    assert!(gone.is_err(), "a stopped task answered");
}
