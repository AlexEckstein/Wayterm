//! Where the tab's shell is, asked of the remote over the shared
//! connection for shells that do not report it with OSC 7 (doc 07
//! §Following the shell). Each probe runs one `sh` with its script on
//! stdin, so the login shell (fish, bash, zsh) never parses the script.
//!
//! A tmux tab asks tmux for its session's active pane. A plain tab walks
//! up from the probe to this connection's SSH server process and takes its
//! one child on a terminal that is not running tmux: the tab's shell. The
//! answer is the working directory of that terminal's foreground process
//! (the shell itself at a prompt), read from `/proc`, so plain tabs need a
//! Linux remote. Two plain tabs on one connection cannot be told apart and
//! get no answer.

use std::sync::{Arc, Weak};
use std::time::Duration;

use russh::ChannelMsg;
use tokio::sync::oneshot;

use crate::transport::connection::SshConnection;
use crate::transport::ssh::shell_quote;

/// Prefixes the answer, since login-shell startup files may print too.
const MARKER: &str = "wayterm-cwd:";
/// A slower probe is dropped and the sidebar stays where it is.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// More output than this is not a path.
const MAX_OUTPUT: usize = 64 * 1024;

/// The plain-shell walk. Children come from `/proc/<pid>/task/<pid>/children`,
/// or a `PPid` scan where the kernel lacks it; a terminal whose shell,
/// foreground process, or child is tmux belongs to a tmux tab.
const SHELL_SCRIPT: &str = r#"p=$$
while :; do
  c=$(cat /proc/$p/comm 2>/dev/null) || exit 0
  case $c in sshd*|dropbear) break ;; esac
  p=$(sed -n 's/^PPid:[[:space:]]*//p' /proc/$p/status 2>/dev/null)
  [ -n "$p" ] && [ "$p" -gt 1 ] || exit 0
done
kids=$(cat /proc/$p/task/$p/children 2>/dev/null)
[ -n "$kids" ] || kids=$(grep -ls "^PPid:[[:space:]]*$p\$" /proc/[0-9]*/status | cut -d/ -f3)
shell=
for k in $kids; do
  case $(readlink /proc/$k/fd/0 2>/dev/null) in /dev/pts/*|/dev/tty*) ;; *) continue ;; esac
  stat=$(cat /proc/$k/stat 2>/dev/null) || continue
  set -- ${stat##*)}
  t=$6
  for q in $k $t $(cat /proc/$k/task/$k/children 2>/dev/null); do
    case $(cat /proc/$q/comm 2>/dev/null) in tmux*) continue 2 ;; esac
  done
  [ -z "$shell" ] || exit 0
  shell=$k fg=$t
done
[ -n "$shell" ] || exit 0
dir=$(readlink /proc/$fg/cwd 2>/dev/null) || dir=$(readlink /proc/$shell/cwd 2>/dev/null) || exit 0
printf 'wayterm-cwd:%s\n' "$dir"
"#;

/// Asks the remote where one tab's shell is. Holds the connection weakly,
/// so a probe never keeps a released connection alive.
#[derive(Clone, Debug)]
pub struct ShellCwdProbe {
    /// Weak, so a probe never keeps a released connection alive.
    connection: Weak<SshConnection>,
    /// The tab's tmux session (`wayterm-<name>`), or `None` for a plain shell.
    tmux_session: Option<String>,
}

impl ShellCwdProbe {
    /// A probe on `connection`; `tmux_session` for a tmux tab.
    pub fn new(connection: &Arc<SshConnection>, tmux_session: Option<String>) -> Self {
        Self {
            connection: Arc::downgrade(connection),
            tmux_session,
        }
    }

    /// Run on the current tokio runtime; the reply is the directory, or
    /// `None` when the remote could not say.
    pub fn spawn(&self) -> oneshot::Receiver<Option<String>> {
        let (reply, reply_rx) = oneshot::channel();
        let probe = self.clone();
        tokio::spawn(async move {
            let _ = reply.send(probe.run().await);
        });
        reply_rx
    }

    /// Ask once; `None` when the remote gives no answer.
    pub async fn run(&self) -> Option<String> {
        let connection = self.connection.upgrade()?;
        let script = probe_script(self.tmux_session.as_deref());
        let output = tokio::time::timeout(PROBE_TIMEOUT, run_script(&connection, &script))
            .await
            .ok()??;
        parse_probe_output(&output)
    }
}

/// The script `sh` reads: tmux's view of the session's active pane, or the
/// plain-shell walk.
pub fn probe_script(tmux_session: Option<&str>) -> String {
    match tmux_session {
        // `=` matches the name exactly; the trailing `:` picks the
        // session's current window, and with it the active pane.
        Some(name) => format!(
            "tmux display-message -p -t {} '#{{pane_current_path}}' 2>/dev/null | sed 's/^/{MARKER}/'\n",
            shell_quote(&format!("={}:", tmux_target_name(name)))
        ),
        None => SHELL_SCRIPT.to_string(),
    }
}

/// tmux turns `:` and `.` in a session name into `_`, `new -A -s` included,
/// so the exact-match target has to as well.
fn tmux_target_name(name: &str) -> String {
    name.replace([':', '.'], "_")
}

/// The directory in the probe's output: the last marked line, if it holds
/// an absolute path.
pub fn parse_probe_output(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let path = text
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(MARKER))?;
    path.starts_with('/').then(|| path.to_string())
}

/// Run `sh` with `script` on stdin over a fresh session channel and
/// collect its stdout.
async fn run_script(connection: &SshConnection, script: &str) -> Option<Vec<u8>> {
    let mut channel = connection.open_session_channel().await.ok()?;
    channel.exec(true, "sh").await.ok()?;
    channel.data(script.as_bytes()).await.ok()?;
    channel.eof().await.ok()?;
    let mut output = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                output.extend_from_slice(&data);
                if output.len() > MAX_OUTPUT {
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

#[cfg(test)]
mod tests;
