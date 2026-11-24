//! Short scripts run on the remote over their own session channel of a
//! shared connection: `sh` reads the script on stdin, so the login shell
//! (fish, bash, zsh) never parses it.

use std::sync::Weak;

use russh::ChannelMsg;
use tokio::sync::mpsc;

use crate::transport::connection::SshConnection;

/// A longer unterminated line from a streaming script is dropped.
const MAX_LINE: usize = 4096;

/// `sh`'s stdout for `script`, cut off past `max_output` bytes; `None` when
/// the channel could not run it.
pub(crate) async fn run_script(
    connection: &SshConnection,
    script: &str,
    max_output: usize,
) -> Option<Vec<u8>> {
    let mut channel = connection.open_session_channel().await.ok()?;
    channel.exec(true, "sh").await.ok()?;
    channel.data(script.as_bytes()).await.ok()?;
    channel.eof().await.ok()?;
    let mut output = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                output.extend_from_slice(&data);
                if output.len() > max_output {
                    break;
                }
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    let _ = channel.close().await;
    Some(output)
}

/// Run the long-lived `script`, passing each output line through `parse`
/// and what it returns to `out`, until `out`'s receiver is dropped or the
/// script ends. The connection is held only while the channel opens, so a
/// stream never keeps a released connection alive.
pub(crate) async fn stream<T>(
    connection: Weak<SshConnection>,
    script: &str,
    mut parse: impl FnMut(&str) -> Option<T>,
    out: mpsc::Sender<T>,
) {
    let Some(connection) = connection.upgrade() else {
        return;
    };
    let Ok(mut channel) = connection.open_session_channel().await else {
        return;
    };
    drop(connection);
    let started = async {
        channel.exec(true, "sh").await?;
        channel.data(script.as_bytes()).await?;
        channel.eof().await
    };
    if started.await.is_err() {
        return;
    }
    let mut pending = Vec::new();
    'read: loop {
        let msg = tokio::select! {
            _ = out.closed() => break,
            msg = channel.wait() => msg,
        };
        match msg {
            Some(ChannelMsg::Data { data }) => {
                pending.extend_from_slice(&data);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    if let Some(item) = parse(&String::from_utf8_lossy(&line))
                        && out.send(item).await.is_err()
                    {
                        break 'read;
                    }
                }
                if pending.len() > MAX_LINE {
                    pending.clear();
                }
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    let _ = channel.close().await;
}
