use super::*;

fn transport(config: &SessionConfig) -> (LocalTransport, mpsc::Receiver<TransportEvent>) {
    let (tx, rx) = mpsc::channel(256);
    (LocalTransport::new(config, tx, 24, 80), rx)
}

fn sh_config() -> SessionConfig {
    SessionConfig {
        local_shell: Some("/bin/sh".into()),
        ..SessionConfig::default()
    }
}

/// Collect output until it contains `needle`.
async fn output_until(rx: &mut mpsc::Receiver<TransportEvent>, needle: &str) -> String {
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !seen.contains(needle) {
            if let Some(TransportEvent::DataReceived(data)) = rx.recv().await {
                seen.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {needle:?} in output: {seen:?}"));
    seen
}

#[test]
fn shell_selection_prefers_session_then_env_then_bin_sh() {
    let env = Some(OsString::from("/usr/bin/zsh"));
    assert_eq!(
        select_shell(Some("/bin/bash"), env.clone()),
        Path::new("/bin/bash")
    );
    assert_eq!(select_shell(None, env.clone()), Path::new("/usr/bin/zsh"));
    assert_eq!(select_shell(Some(""), env), Path::new("/usr/bin/zsh"));
    assert_eq!(
        select_shell(None, Some(OsString::new())),
        Path::new("/bin/sh")
    );
    assert_eq!(select_shell(None, None), Path::new("/bin/sh"));
}

#[test]
fn bare_shell_names_resolve_through_path() {
    assert!(resolve_shell(Path::new("sh")).unwrap().is_absolute());
    assert!(matches!(
        resolve_shell(Path::new("/nonexistent/shell")),
        Err(TransportError::ConnectionFailed(_))
    ));
}

#[test]
fn child_env_declares_the_terminal() {
    let env = child_env();
    let has = |entry: &str| env.iter().any(|e| e.as_bytes() == entry.as_bytes());
    assert!(has("TERM=xterm-256color"));
    assert!(has("COLORTERM=truecolor"));
    assert_eq!(
        env.iter()
            .filter(|e| e.as_bytes().starts_with(b"TERM="))
            .count(),
        1
    );
}

#[test]
fn identity_is_the_shell() {
    let (t, _rx) = transport(&sh_config());
    assert_eq!(t.description(), "/bin/sh");
    assert!(matches!(t.transport_type(), TransportType::LocalPTY));
    assert_eq!(t.state(), TransportState::Disconnected);
}

#[tokio::test]
async fn working_dir_becomes_the_shells_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let config = SessionConfig {
        local_working_dir: Some(dir.path().to_string_lossy().into_owned()),
        ..sh_config()
    };
    let (mut t, mut rx) = transport(&config);
    t.connect().await.unwrap();
    // The echoed command line also shows the markers, around `$(…)`.
    t.write(b"echo \"cwd=$(pwd -P).\"\n").await.unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    output_until(&mut rx, &format!("cwd={}.", canonical.display())).await;
    t.disconnect().await.unwrap();
}

#[tokio::test]
async fn the_shell_inherits_no_descriptors_beyond_its_terminal() {
    // A descriptor without close-on-exec, at a number nothing else
    // uses: exactly what a racing fork or a careless library leaves.
    const PLANTED: libc::c_int = 777;
    // SAFETY: duplicating our own stderr onto an unused number.
    assert_eq!(unsafe { libc::dup2(2, PLANTED) }, PLANTED);
    let (mut t, mut rx) = transport(&sh_config());
    t.connect().await.unwrap();
    // The marker is assembled by the shell, so the echoed command line
    // cannot satisfy the wait by itself.
    t.write(b"echo \"fd777=$(test -e /proc/$$/fd/777 && echo open || echo closed).\"\n")
        .await
        .unwrap();
    let seen = output_until(&mut rx, "fd777=closed.").await;
    t.disconnect().await.unwrap();
    // SAFETY: closing the descriptor planted above.
    unsafe { libc::close(PLANTED) };
    assert!(!seen.contains("fd777=open."), "{seen:?}");
}

#[tokio::test]
async fn a_missing_working_dir_fails_the_connect() {
    let config = SessionConfig {
        local_working_dir: Some("/nonexistent/wayterm-dir".into()),
        ..sh_config()
    };
    let (mut t, _rx) = transport(&config);
    assert!(matches!(
        t.connect().await,
        Err(TransportError::ConnectionFailed(_))
    ));
    assert_eq!(t.state(), TransportState::Disconnected);
}

#[tokio::test]
async fn state_moves_through_connecting_connected_disconnected() {
    let (mut t, mut rx) = transport(&sh_config());
    t.connect().await.unwrap();
    assert_eq!(t.state(), TransportState::Connected);
    assert!(matches!(
        t.connect().await,
        Err(TransportError::ChannelError(_))
    ));
    t.disconnect().await.unwrap();
    assert_eq!(t.state(), TransportState::Disconnected);

    let mut states = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let TransportEvent::StateChanged(state) = event {
            states.push(state);
        }
    }
    assert_eq!(
        states,
        [
            TransportState::Connecting,
            TransportState::Connected,
            TransportState::Disconnected
        ]
    );
}

#[tokio::test]
async fn disconnect_is_idempotent_and_ends_io() {
    let (mut t, _rx) = transport(&sh_config());
    t.disconnect().await.unwrap();
    t.connect().await.unwrap();
    t.disconnect().await.unwrap();
    t.disconnect().await.unwrap();
    assert_eq!(t.state(), TransportState::Disconnected);
    assert!(matches!(
        t.write(b"x").await,
        Err(TransportError::Disconnected)
    ));
    assert!(matches!(
        t.resize(30, 100).await,
        Err(TransportError::Disconnected)
    ));
}
