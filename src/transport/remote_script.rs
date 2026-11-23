//! Short scripts run on the remote over their own session channel of a
//! shared connection: `sh` reads the script on stdin, so the login shell
//! (fish, bash, zsh) never parses it.

use russh::ChannelMsg;

use crate::transport::connection::SshConnection;

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
