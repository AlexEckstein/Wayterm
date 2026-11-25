//! A host's figures for the status bar, from one long-lived script: its
//! uptime once, then CPU, memory and bandwidth every [`INTERVAL`] and its
//! filesystems every [`disks::INTERVAL`]. An SSH tab runs it on one exec
//! channel of the server's connection (doc 04 §Server figures), a local
//! shell under this machine's `sh` (doc 06 §Local shell); both read its
//! lines through the same [`Parser`].

use std::sync::Weak;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::transport::bandwidth::{self, Counters};
use crate::transport::connection::SshConnection;
use crate::transport::disks::{self, Collector, Disks};
use crate::transport::load::{self, LoadSample};

/// How often CPU, memory and bandwidth are sampled.
pub const INTERVAL: Duration = Duration::from_secs(2);
const UPTIME: &str = "wayterm-uptime:";
/// A longer unterminated line is dropped.
const MAX_LINE: usize = 4096;

/// Every figure's line is marked, since login-shell startup files may
/// print too, and is written whole by one `printf`, so lines from the
/// background `df` jobs never break into another's.
///
/// The uptime comes first, from `/proc/uptime` on Linux, else from
/// `kern.boottime` (macOS and the BSDs: `{ sec = N, usec = M } …` or a
/// bare `N`); the rest needs Linux's `/proc`. Each tick prints the core
/// count, load averages, memory and swap (KiB) and the aggregate `cpu`
/// times, then the default route's interface counters (IPv4's, else
/// IPv6's). Kernels before 3.14 have no `MemAvailable`; free plus buffers
/// plus cache stands in.
///
/// Every `{disk_ticks}` ticks it prints the mount table and starts two
/// background `df` jobs, one for the local filesystems and one for the
/// network (and FUSE) ones, each given its mount points so a hung mount of
/// one kind never stalls the other. A job still running at the next round
/// is not started again: the round says it is late instead, and the other
/// figures carry on.
const SCRIPT: &str = r#"if [ -r /proc/uptime ]; then
  read -r up _ < /proc/uptime
  printf 'wayterm-uptime:%s\n' "${up%%.*}"
else
  b=$(sysctl -n kern.boottime 2>/dev/null)
  case $b in *"sec = "*) b=${b#*sec = }; b=${b%%,*} ;; esac
  case $b in
    ''|*[!0-9]*) ;;
    *) printf 'wayterm-uptime:%s\n' $(( $(date +%s) - b )) ;;
  esac
fi
[ -r /proc/stat ] && [ -r /proc/meminfo ] && [ -r /proc/loadavg ] || exit 0
n=$(grep -c '^cpu[0-9]' /proc/stat)
i=
if [ -r /proc/net/dev ] && [ -r /proc/net/route ]; then
  while read -r f d _; do
    [ "$d" = 00000000 ] && { i=$f; break; }
  done < /proc/net/route
fi
if [ -r /proc/net/dev ] && [ -z "$i" ] && [ -r /proc/net/ipv6_route ]; then
  while read -r d p _ _ _ _ _ _ _ f; do
    [ "$d$p" = 0000000000000000000000000000000000 ] && [ "$f" != lo ] &&
      { i=$f; break; }
  done < /proc/net/ipv6_route
fi
disks() {
  g=$1
  set --
  while read -r _ m k _; do
    case $k in
      {skipped}) continue ;;
      {network}) [ "$g" = net ] || continue ;;
      *) [ "$g" = local ] || continue ;;
    esac
    case $m in
      *\\*)
        m=$(printf '%s\n' "$m" | sed 's/\\\([0-7][0-7][0-7]\)/\\0\1/g')
        m=$(printf '%b' "$m") ;;
    esac
    set -- "$@" "$m"
  done < /proc/mounts
  if [ $# -gt 0 ]; then
    df -P -k -- "$@" 2>/dev/null | while IFS= read -r l; do
      printf 'wayterm-df:%s %s\n' "$g" "$l" || exit 0
    done
  fi
  printf 'wayterm-disks:%s\n' "$g"
}
t=0 lp= np=
while :; do
  if [ "$t" -le 0 ] && [ -r /proc/mounts ]; then
    t={disk_ticks}
    while read -r l; do
      printf 'wayterm-mount:%s\n' "$l" || exit 0
    done < /proc/mounts
    printf 'wayterm-mounts\n' || exit 0
    if [ -n "$lp" ] && kill -0 "$lp" 2>/dev/null; then
      printf 'wayterm-disks-late:local\n' || exit 0
    else
      disks local & lp=$!
    fi
    if [ -n "$np" ] && kill -0 "$np" 2>/dev/null; then
      printf 'wayterm-disks-late:net\n' || exit 0
    else
      disks net & np=$!
    fi
  fi
  t=$((t - 1))
  read -r _ c < /proc/stat
  read -r l1 l5 l15 _ < /proc/loadavg
  mt= ma= mf=0 mb=0 mc=0 st=0 sf=0
  while read -r k v _; do
    case $k in
      MemTotal:) mt=$v ;;
      MemFree:) mf=$v ;;
      MemAvailable:) ma=$v ;;
      Buffers:) mb=$v ;;
      Cached:) mc=$v ;;
      SwapTotal:) st=$v ;;
      SwapFree:) sf=$v; break ;;
    esac
  done < /proc/meminfo
  [ -n "$ma" ] || ma=$((mf + mb + mc))
  printf 'wayterm-load:%s %s %s %s %s %s %s %s %s\n' \
    "$n" "$l1" "$l5" "$l15" "$mt" "$ma" "$st" "$sf" "$c" || exit 0
  if [ -n "$i" ]; then
    x=
    while read -r l; do
      case $l in "$i:"*) x=${l#*:}; break ;; esac
    done < /proc/net/dev
    if [ -n "$x" ]; then
      set -- $x
      printf 'wayterm-net:%s %s %s\n' "$i" "$1" "$9" || exit 0
    fi
  fi
  sleep {interval}
done
"#;

/// The script, sleeping `interval` (as `sleep` takes it) between ticks and
/// reading the disks every `disk_ticks` ticks.
fn render(interval: &str, disk_ticks: u64) -> String {
    SCRIPT
        .replace("{interval}", interval)
        .replace("{disk_ticks}", &disk_ticks.to_string())
        .replace("{skipped}", &disks::SKIPPED_TYPES.join("|"))
        .replace("{network}", &disks::NETWORK_TYPES.join("|"))
}

fn script() -> String {
    render(
        &INTERVAL.as_secs().to_string(),
        disks::INTERVAL.as_secs() / INTERVAL.as_secs(),
    )
}

/// One figure from the script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Sample {
    /// Seconds since boot.
    Uptime(u64),
    Load(LoadSample),
    Net(Counters),
    Disks(Disks),
}

/// Turns the script's lines into [`Sample`]s.
#[derive(Debug, Default)]
pub struct Parser {
    disks: Collector,
}

impl Parser {
    pub fn line(&mut self, line: &str) -> Option<Sample> {
        if let Some(secs) = parse_uptime(line) {
            return Some(Sample::Uptime(secs));
        }
        if let Some(sample) = load::parse_sample(line) {
            return Some(Sample::Load(sample));
        }
        if let Some(counters) = bandwidth::parse_sample(line) {
            return Some(Sample::Net(counters));
        }
        self.disks.line(line).map(Sample::Disks)
    }
}

/// `wayterm-uptime:<seconds>`.
pub fn parse_uptime(line: &str) -> Option<u64> {
    line.trim().strip_prefix(UPTIME)?.parse().ok()
}

/// Send the server's figures to `samples` until its receiver is dropped or
/// the script ends. `sh` reads the script on stdin, so the login shell
/// never parses it. The connection is held only while the channel opens,
/// so the stream never keeps a released connection alive.
pub async fn stream_server(connection: Weak<SshConnection>, samples: mpsc::Sender<Sample>) {
    let Some(connection) = connection.upgrade() else {
        return;
    };
    let Ok(mut channel) = connection.open_session_channel().await else {
        return;
    };
    drop(connection);
    let script = script();
    let started = async {
        channel.exec(true, "sh").await?;
        channel.data(script.as_bytes()).await?;
        channel.eof().await
    };
    if started.await.is_ok() {
        read_samples(channel.make_reader(), &samples).await;
    }
    let _ = channel.close().await;
}

/// Send this machine's figures to `samples` until its receiver is dropped
/// or the script ends; the same script runs under this machine's `sh`.
pub async fn stream_local(samples: mpsc::Sender<Sample>) {
    let spawned = tokio::process::Command::new("sh")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut sh) = spawned else { return };
    let (Some(mut stdin), Some(stdout)) = (sh.stdin.take(), sh.stdout.take()) else {
        return;
    };
    if stdin.write_all(script().as_bytes()).await.is_err() {
        return;
    }
    drop(stdin);
    read_samples(stdout, &samples).await;
}

/// Parse `output`'s lines into `samples` until either ends.
async fn read_samples(mut output: impl AsyncRead + Unpin, samples: &mpsc::Sender<Sample>) {
    let mut parser = Parser::default();
    let mut pending = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = tokio::select! {
            _ = samples.closed() => return,
            read = output.read(&mut chunk) => read,
        };
        let Ok(n @ 1..) = read else { return };
        pending.extend_from_slice(&chunk[..n]);
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            if let Some(sample) = parser.line(&String::from_utf8_lossy(&line))
                && samples.send(sample).await.is_err()
            {
                return;
            }
        }
        if pending.len() > MAX_LINE {
            pending.clear();
        }
    }
}

#[cfg(test)]
mod tests;
