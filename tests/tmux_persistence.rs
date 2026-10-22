//! End-to-end tmux persistence against a real sshd (doc 04 §Tmux
//! session persistence).
//!
//! Needs the `common` sshd fixture and `tmux` on the remote host (the
//! tests skip without either).
//!
//! What is actually proven here:
//!
//! 1. `tmux_enabled` + a session name makes `SSHTransport::connect` exec
//!    `tmux new -A -s wayterm-<name>` instead of a plain shell — the shell
//!    we get back reports `TMUX` set and `tmux display-message -p '#S'`
//!    printing that exact session.
//! 2. Disconnecting the whole transport (closing the channel, dropping the
//!    `SshConnection`) does **not** kill the remote tmux session, and a
//!    reconnect over a brand-new connection re-attaches to it. The proof is
//!    the tmux **session id** (`#{session_id}`, monotonically increasing per
//!    created session) plus a session-scoped environment variable written
//!    before the disconnect: a fresh session would get a new id and no
//!    variable.
//! 3. With tmux disabled, the shell is not inside tmux and the
//!    `wayterm-<name>` session does not exist — the off-path.
//! 4. Doc ??10's "run vim, kill the network, reconnect, vim is still there",
//!    through the tab's reconnect path minus GTK: the whole connection is
//!    killed under a live transport, its `ConnectionLost` starts the
//!    auto-reconnect driver from the resilience registry, the driver redials
//!    through a real `ConnectionSupervisor`, and a program running in the
//!    tmux pane is the same process afterwards.
//!
//! Tests share one sshd and one tmux server, so the harness runs them with
//! `--test-threads=1`.

mod common;

use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use common::*;
use wayterm::store::model::{SessionConfig, SessionType, TMUX_NAME_PREFIX};
use wayterm::transport::connection::SshConnection;
use wayterm::transport::factory::create_transport;
use wayterm::transport::reconnect::{ConnectFn, ConnectFuture};
use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor};
use wayterm::transport::types::{Transport, TransportEvent, TransportState, TransportType};
use wayterm::ui::tab_resilience::{self, ReconnectAction};

fn skip(reason: &str) {
    println!("Skipping tmux persistence test: {reason}");
}

/// A transport over `connection`, optionally with tmux persistence enabled
/// through the real [`create_transport`] factory — the same path the UI
/// takes, so this test covers the dialog's stored fields as well as the
/// exec'd command.
fn ssh_transport(
    config: &SessionConfig,
    connection: Arc<SshConnection>,
) -> (Box<dyn Transport>, mpsc::Receiver<TransportEvent>) {
    let (tx, rx) = mpsc::channel(256);
    let transport =
        create_transport(config, Some(connection), tx, 24, 80).expect("create_transport");
    (transport, rx)
}

/// A plain-shell transport, used for probing the remote host out of band
/// (`tmux has-session`, `command -v tmux`) so assertions never depend on the
/// tmux session's own echo.
async fn probe_transport(f: &Fixture) -> (Box<dyn Transport>, mpsc::Receiver<TransportEvent>) {
    let connection = connect_password(f).await;
    let config = SessionConfig {
        name: "tmux-probe".to_string(),
        session_type: SessionType::SSH,
        ..Default::default()
    };
    let (mut transport, rx) = ssh_transport(&config, connection);
    transport.connect().await.expect("probe connect failed");
    (transport, rx)
}

/// How long [`run`] keeps draining output after the needle appears. An
/// interactive shell echoes the command line before running it, so a needle
/// that is part of the command text is satisfied by the echo; draining until
/// the remote goes quiet captures the actual result too.
const SETTLE: Duration = Duration::from_millis(750);

/// Run `cmd` in `transport` and return its output.
///
/// Returns once `needle` has been seen and the remote has then been quiet
/// for [`SETTLE`]. Callers must therefore pick a needle that appears in the
/// *result*, not in the echoed command line — split the marker into shell
/// string halves when the command has to contain it.
async fn run(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
    cmd: &str,
    needle: &str,
) -> String {
    transport
        .write(format!("{cmd}\n").as_bytes())
        .await
        .expect("write failed");
    timeout(Duration::from_secs(20), async {
        let mut output = String::new();
        let mut needle_seen = false;
        loop {
            let chunk = match tokio::time::timeout(SETTLE, rx.recv()).await {
                Ok(Some(TransportEvent::DataReceived(bytes))) => bytes,
                Ok(Some(TransportEvent::ConnectionLost)) if needle_seen => break,
                Ok(Some(TransportEvent::ConnectionLost)) => {
                    panic!("connection lost while waiting for '{needle}'")
                }
                Ok(Some(_)) => continue,
                // Quiet: either the needle never came or the command is done.
                Ok(None) | Err(_) => break,
            };
            output.push_str(&String::from_utf8_lossy(&chunk));
            if output.contains(needle) {
                needle_seen = true;
            }
        }
        assert!(
            output.contains(needle),
            "timed out waiting for '{needle}' in output of `{cmd}`; got:\n{output}"
        );
        output
    })
    .await
    .unwrap_or_else(|_| panic!("`{cmd}` produced no terminal output within the deadline"))
}

/// Unique per run, so a stale session never satisfies an assertion.
fn unique_suffix(tag: &str) -> String {
    format!("tmux-{tag}-{}", unique_tag())
}

/// Is `tmux` installed on the remote host? Probed over a plain shell so the
/// answer does not depend on the feature under test.
async fn remote_has_tmux(f: &Fixture) -> bool {
    let (mut transport, mut rx) = probe_transport(f).await;
    // The marker is assembled from two quoted halves so the shell's echo of
    // the command line never contains it verbatim: waiting for the token
    // text itself would match the echo and return before the value arrived.
    let output = run(
        &mut transport,
        &mut rx,
        "echo \"WAYTERM_TMUX\"\"PATH=$(command -v tmux || echo none)\"",
        "WAYTERM_TMUXPATH=",
    )
    .await;
    let _ = transport.disconnect().await;
    let resolved = output
        .rsplit_once("WAYTERM_TMUXPATH=")
        .expect("marker present")
        .1
        .split_whitespace()
        .next()
        .unwrap_or_default();
    println!("remote tmux: {resolved}");
    resolved.ends_with("tmux")
}

/// A `SessionConfig` with tmux persistence on, as the dialog would store it
/// for a session named `name` with the default `wayterm-<name>` remote name.
fn tmux_session_config(name: &str, suffix: &str) -> SessionConfig {
    SessionConfig {
        name: name.to_string(),
        session_type: SessionType::SSH,
        host: Some("127.0.0.1".to_string()),
        port: Some(22),
        username: Some("wayterm-test".to_string()),
        tmux_enabled: true,
        // Stored bare: `SSHTransport` prepends `wayterm-`.
        tmux_session_name: Some(suffix.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn tmux_session_survives_disconnect_and_reconnect_attaches_to_it() {
    let Some(f) = fixture() else { return };
    if !remote_has_tmux(&f).await {
        skip("tmux is not installed on the remote host");
        return;
    }

    let suffix = unique_suffix("persist");
    let remote_name = format!("{}{}", TMUX_NAME_PREFIX, suffix);
    let token = format!("wayterm-token-{suffix}");
    let config = tmux_session_config("tmux persist", &suffix);

    // --- First connect: the transport execs `tmux new -A -s wayterm-…` ----
    let first_session_id;
    {
        let connection = connect_password(&f).await;
        let (mut transport, mut rx) = ssh_transport(&config, connection);
        transport.connect().await.expect("connect failed");

        // Inside tmux: TMUX is set, and the shell reports our session name.
        let output = run(
            &mut transport,
            &mut rx,
            "echo \"WAYTERM_TMUX=[${TMUX:-none}]\"",
            "WAYTERM_TMUX=",
        )
        .await;
        assert!(
            !output.contains("WAYTERM_TMUX=[none]"),
            "the shell must be inside a tmux session, got: {output}"
        );

        let output = run(
            &mut transport,
            &mut rx,
            "tmux display-message -p '#{session_name}'",
            &remote_name,
        )
        .await;
        assert!(
            output.contains(&remote_name),
            "display-message must print {remote_name}, got: {output}"
        );

        // Session id is the identity proof: a *newly created* session gets a
        // fresh one. Session-scoped environment is the second proof.
        first_session_id = session_id(&mut transport, &mut rx).await;
        assert!(
            first_session_id.starts_with('$'),
            "session id should look like $N, got {first_session_id}"
        );

        set_session_env(&mut transport, &mut rx, &remote_name, &token).await;

        transport.disconnect().await.expect("disconnect failed");
        assert_eq!(transport.state(), TransportState::Disconnected);
        drop(transport);
    }
    // The `SshConnection` (and the whole TCP session) is gone with the block.

    // --- The tmux session outlived the transport --------------------------
    {
        let (mut transport, mut rx) = probe_transport(&f).await;
        assert!(
            run_ok(
                &mut transport,
                &mut rx,
                &format!("tmux has-session -t {remote_name}")
            )
            .await,
            "{remote_name} must survive the disconnect"
        );
        let _ = transport.disconnect().await;
    }

    // --- Reconnect over a brand-new connection: `-A` re-attaches ---------
    {
        let connection = connect_password(&f).await;
        let (mut transport, mut rx) = ssh_transport(&config, connection);
        transport.connect().await.expect("reconnect failed");

        let reattached_id = session_id(&mut transport, &mut rx).await;
        assert_eq!(
            reattached_id, first_session_id,
            "reconnect must re-attach to the persisted session, not create a new one"
        );

        let output = run(
            &mut transport,
            &mut rx,
            &format!("tmux show-environment -t {remote_name} WAYTERM_PROBE"),
            &token,
        )
        .await;
        assert!(
            output.contains(&token),
            "session state written before the disconnect must still be there, got: {output}"
        );

        transport.disconnect().await.expect("disconnect failed");
    }

    // --- Cleanup: leave no session behind on the shared test host --------
    let (mut transport, mut rx) = probe_transport(&f).await;
    run(
        &mut transport,
        &mut rx,
        &format!("tmux kill-session -t {remote_name} 2>/dev/null; echo WAYTERM_CLEANED"),
        "WAYTERM_CLEANED",
    )
    .await;
    let _ = transport.disconnect().await;
}

/// Run `cmd` on the remote and report whether it succeeded, as a `bool`.
///
/// Used for probes whose answer is the exit status (`tmux has-session`), where
/// there is no output to wait for. The marker is assembled from two quoted
/// halves so the shell's echo of the command never matches it; [`run`] drains
/// until the remote goes quiet, so the real result is always in `output`.
async fn run_ok(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
    cmd: &str,
) -> bool {
    let output = run(
        transport,
        rx,
        &format!("{cmd} && echo \"WAYTERM_\"\"TRUE\" || echo \"WAYTERM_\"\"FALSE\""),
        "WAYTERM_",
    )
    .await;
    // The echoed command line can only produce the first marker; the real
    // verdict is the last one the shell printed.
    output
        .rsplit("WAYTERM_")
        .next()
        .map(|tail| tail.starts_with("TRUE"))
        .unwrap_or(false)
}

/// Read `#{session_id}` out of the *current* tmux session.
async fn session_id(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
) -> String {
    let output = run(
        transport,
        rx,
        "echo \"WAYTERM_SID=$(tmux display-message -p '#{session_id}')\"",
        "WAYTERM_SID=",
    )
    .await;
    output
        .rsplit_once("WAYTERM_SID=")
        .expect("marker present")
        .1
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Store `token` in the current tmux session's environment — state that
/// exists only inside that one tmux session object.
async fn set_session_env(
    transport: &mut Box<dyn Transport>,
    rx: &mut mpsc::Receiver<TransportEvent>,
    remote_name: &str,
    token: &str,
) {
    let output = run(
        transport,
        rx,
        &format!(
            "tmux set-environment -t {remote_name} WAYTERM_PROBE {token} && echo WAYTERM_SET_OK"
        ),
        "WAYTERM_SET_OK",
    )
    .await;
    assert!(
        output.contains("WAYTERM_SET_OK"),
        "set-environment failed: {output}"
    );
}

#[tokio::test]
async fn tmux_disabled_leaves_the_shell_outside_tmux() {
    let Some(f) = fixture() else { return };
    if !remote_has_tmux(&f).await {
        skip("tmux is not installed on the remote host");
        return;
    }

    let suffix = unique_suffix("off");
    let remote_name = format!("{}{}", TMUX_NAME_PREFIX, suffix);
    // Deliberately enabled in the stored row but with no tmux switch turned
    // on by the caller: create_transport only calls set_tmux_enabled when
    // tmux_session_suffix() yields a name, and a disabled row yields None.
    let mut config = tmux_session_config("tmux off", &suffix);
    config.tmux_enabled = false;

    let connection = connect_password(&f).await;
    let (mut transport, mut rx) = ssh_transport(&config, connection);
    transport.connect().await.expect("connect failed");

    let output = run(
        &mut transport,
        &mut rx,
        "echo \"WAYTERM_TMUX=[${TMUX:-none}]\"",
        "WAYTERM_TMUX=",
    )
    .await;
    assert!(
        output.contains("WAYTERM_TMUX=[none]"),
        "with tmux disabled the shell must be a plain shell, got: {output}"
    );

    // …and no wayterm- session was created for it.
    let (mut probe, mut probe_rx) = probe_transport(&f).await;
    assert!(
        !run_ok(
            &mut probe,
            &mut probe_rx,
            &format!("tmux has-session -t {remote_name}")
        )
        .await,
        "no tmux session should exist for a session with the toggle off"
    );

    transport.disconnect().await.expect("disconnect failed");
    let _ = probe.disconnect().await;
}

/// A serial session with the tmux flag on stores it, but the flag is inert:
/// doc 04 scopes tmux to SSH, and a serial console is a device, not a shell
/// host session.
#[tokio::test]
async fn tmux_flag_on_a_serial_session_is_inert() {
    let config = SessionConfig {
        name: "console".to_string(),
        session_type: SessionType::Serial,
        serial_device: Some("/dev/ttyUSB0".to_string()),
        tmux_enabled: true,
        tmux_session_name: Some("console".to_string()),
        ..Default::default()
    };

    let (tx, _rx) = mpsc::channel::<TransportEvent>(1);
    let transport = create_transport(&config, None, tx, 24, 80)
        .expect("serial sessions get a SerialTransport (doc 06)");
    assert!(matches!(transport.transport_type(), TransportType::Serial));
    assert_eq!(transport.description(), "/dev/ttyUSB0@115200");
}

/// The tab's current transport and its event receiver; every connect
/// installs a fresh pair, as `finish_connect` does on the page.
type TabSlot = Arc<Mutex<Option<(Box<dyn Transport>, mpsc::Receiver<TransportEvent>)>>>;

/// The UI's redial minus GTK: a fresh connect through the supervisor (which
/// reuses a live pooled connection and dials when there is none) and a fresh
/// tmux transport over it.
fn supervisor_connect(
    f: &Fixture,
    supervisor: Arc<ConnectionSupervisor>,
    config: SessionConfig,
    slot: TabSlot,
) -> ConnectFn {
    let params = ConnectParams {
        host: f.host.clone(),
        port: f.port,
        username: f.user.clone(),
    };
    let password = f.password.clone();
    Arc::new(move || {
        let (supervisor, params, password) = (supervisor.clone(), params.clone(), password.clone());
        let (config, slot) = (config.clone(), slot.clone());
        Box::pin(async move {
            let connection = supervisor
                .get_or_connect(&params, &password_auth(&password))
                .await?;
            let (mut transport, rx) = ssh_transport(&config, connection);
            transport.connect().await?;
            *slot.lock().await = Some((transport, rx));
            Ok(())
        }) as ConnectFuture
    })
}

/// The first value printed after `WAYTERM_<key>=[` by `cmd` on a plain
/// probe shell (`cmd` must print the key in two quoted halves).
async fn probe_value(f: &Fixture, cmd: &str, key: &str) -> String {
    let (mut probe, mut rx) = probe_transport(f).await;
    let needle = format!("WAYTERM_{key}=[");
    let output = run(&mut probe, &mut rx, cmd, &needle).await;
    let _ = probe.disconnect().await;
    output
        .rsplit_once(&needle)
        .expect("marker present")
        .1
        .split(']')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The pid of the process whose command line carries `marker`, or "".
async fn marker_pid(f: &Fixture, marker: &str) -> String {
    // Split so that no shell on the probe side carries the marker itself.
    let (head, tail) = marker.split_at(marker.len() / 2);
    let cmd = format!("echo \"WAYTERM_\"\"PID=[$(pgrep -f '{head}''{tail}' | head -n1)]\"");
    probe_value(f, &cmd, "PID").await
}

/// `#{session_id}` of the named tmux session, read out of band.
async fn remote_session_id(f: &Fixture, remote_name: &str) -> String {
    let cmd = format!(
        "echo \"WAYTERM_\"\"SID=[$(tmux display-message -p -t {remote_name} '#{{session_id}}')]\""
    );
    probe_value(f, &cmd, "SID").await
}

/// The next state the reconnect driver published, skipping nothing.
async fn next_state(rx: &mut mpsc::UnboundedReceiver<TransportEvent>) -> TransportState {
    match timeout(Duration::from_secs(60), rx.recv()).await {
        Ok(Some(TransportEvent::StateChanged(state))) => state,
        other => panic!("expected a driver state, got {other:?}"),
    }
}

#[tokio::test]
async fn auto_reconnect_after_a_dropped_connection_keeps_the_program_running() {
    let Some(f) = fixture() else { return };
    if !remote_has_tmux(&f).await {
        skip("tmux is not installed on the remote host");
        return;
    }
    const TAB: i64 = 8_001;

    let suffix = unique_suffix("auto");
    let remote_name = format!("{}{}", TMUX_NAME_PREFIX, suffix);
    let marker = format!("wayterm-marker-{suffix}");
    let config = tmux_session_config("tmux auto", &suffix);
    let supervisor = Arc::new(ConnectionSupervisor::<SshConnection>::new());
    let slot = TabSlot::default();
    let connect = supervisor_connect(&f, supervisor.clone(), config, slot.clone());

    // --- Connect and register, as finish_connect does --------------------
    tab_resilience::reset_registry();
    tab_resilience::install_settings(true, true); // reconnect_on_drop on
    connect().await.expect("first connect failed");
    tab_resilience::register_connected_tab(TAB, connect.clone());
    tab_resilience::set_tab_state(TAB, TransportState::Connected);
    let (mut transport, mut rx) = slot.lock().await.take().expect("transport installed");
    let first_session_id = session_id(&mut transport, &mut rx).await;

    // --- Run a long-lived program in the pane (the stand-in for vim) -----
    transport
        .write(format!("sh -c 'sleep 3600; : {marker}'\n").as_bytes())
        .await
        .expect("write failed");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pid = marker_pid(&f, &marker).await;
    assert!(
        !pid.is_empty(),
        "the program must be running before the drop"
    );

    // --- Kill the whole connection under the live transport --------------
    let params = ConnectParams {
        host: f.host.clone(),
        port: f.port,
        username: f.user.clone(),
    };
    let dropped = supervisor
        .get_or_connect(&params, &password_auth(&f.password))
        .await
        .expect("the pooled connection");
    let dropped_weak: Weak<SshConnection> = Arc::downgrade(&dropped);
    dropped.disconnect().await.expect("disconnect failed");
    drop(dropped);

    // The reader notices: this is the event the tab's state drain acts on.
    let lost = timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Some(TransportEvent::ConnectionLost) | None => return,
                Some(_) => {}
            }
        }
    })
    .await;
    assert!(
        lost.is_ok(),
        "a killed connection must report ConnectionLost"
    );
    // The drain releases the dead transport (release_connection) …
    let _ = transport.disconnect().await;
    drop((transport, rx));
    // … and starts the driver per policy.
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    assert_eq!(
        tab_resilience::on_connection_lost(TAB, state_tx),
        Some(ReconnectAction::Started { tab_id: TAB })
    );
    assert_eq!(tab_resilience::tab_state(TAB), TransportState::Disconnected);

    // --- The driver reattaches on its own ---------------------------------
    let mut attempts = 0;
    loop {
        match next_state(&mut state_rx).await {
            TransportState::Reconnecting { attempt } => attempts = attempt,
            TransportState::Connected => break,
            other => panic!("unexpected driver state {other:?}"),
        }
    }
    assert!(attempts >= 1, "the driver must have made an attempt");
    assert!(
        dropped_weak.upgrade().is_none(),
        "the dead connection must be gone: the redial dialed fresh"
    );
    let (mut transport, mut rx) = slot
        .lock()
        .await
        .take()
        .expect("redial installed a transport");

    // tmux redraws the reattached pane, command line included.
    collect_echo(&mut rx, &marker).await;
    assert_eq!(
        remote_session_id(&f, &remote_name).await,
        first_session_id,
        "the driver must reattach to the same tmux session"
    );
    assert_eq!(
        marker_pid(&f, &marker).await,
        pid,
        "the program started before the drop must still be running"
    );

    // --- Cleanup ----------------------------------------------------------
    tab_resilience::teardown_tab(TAB);
    let _ = transport.disconnect().await;
    let (mut probe, mut probe_rx) = probe_transport(&f).await;
    run(
        &mut probe,
        &mut probe_rx,
        &format!("tmux kill-session -t {remote_name} 2>/dev/null; echo WAYTERM_CLEANED"),
        "WAYTERM_CLEANED",
    )
    .await;
    let _ = probe.disconnect().await;
}
