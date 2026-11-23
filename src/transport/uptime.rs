//! How long the server has been up, asked once per connection
//! (doc 04 §Server uptime).

use std::time::Duration;

use crate::transport::connection::SshConnection;
use crate::transport::remote_script::run_script;

/// Prefixes the answer, since login-shell startup files may print too.
const MARKER: &str = "wayterm-uptime:";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: usize = 4096;

/// Seconds since boot: `/proc/uptime` on Linux, `kern.boottime` on macOS
/// and the BSDs (`{ sec = N, usec = M } …`, or a bare `N`).
const UPTIME_SCRIPT: &str = r#"if [ -r /proc/uptime ]; then
  read up _ < /proc/uptime
  printf 'wayterm-uptime:%s\n' "${up%%.*}"
  exit 0
fi
b=$(sysctl -n kern.boottime 2>/dev/null)
case $b in *"sec = "*) b=${b#*sec = }; b=${b%%,*} ;; esac
case $b in ''|*[!0-9]*) exit 0 ;; esac
printf 'wayterm-uptime:%s\n' $(( $(date +%s) - b ))
"#;

/// The server's uptime in seconds, or `None` when it could not say in
/// time.
pub async fn server_uptime(connection: &SshConnection) -> Option<u64> {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        run_script(connection, UPTIME_SCRIPT, MAX_OUTPUT),
    )
    .await
    .ok()??;
    parse_uptime(&output)
}

/// The last marked line's seconds.
pub fn parse_uptime(output: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(output);
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix(MARKER))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests;
