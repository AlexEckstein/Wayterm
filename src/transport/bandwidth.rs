//! The traffic on a host's default-route interface, sampled every
//! [`INTERVAL`]: over a script on the server for SSH tabs (doc 04 §Server
//! bandwidth), from this machine's `/proc` for local shells (doc 06
//! §Local shell).

use std::sync::Weak;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::transport::connection::SshConnection;
use crate::transport::remote_script;

pub const INTERVAL: Duration = Duration::from_secs(2);
/// Prefixes each sample, since login-shell startup files may print too.
const MARKER: &str = "wayterm-net:";

/// Finds the default route's interface, IPv4 first and then IPv6, then
/// prints its received and sent byte counters every `{interval}` seconds
/// until its output closes.
const SAMPLE_SCRIPT: &str = r#"[ -r /proc/net/dev ] || exit 0
i=
if [ -r /proc/net/route ]; then
  while read -r n d _; do
    [ "$d" = 00000000 ] && { i=$n; break; }
  done < /proc/net/route
fi
if [ -z "$i" ] && [ -r /proc/net/ipv6_route ]; then
  while read -r d p _ _ _ _ _ _ _ n; do
    [ "$d$p" = 0000000000000000000000000000000000 ] && [ "$n" != lo ] &&
      { i=$n; break; }
  done < /proc/net/ipv6_route
fi
[ -n "$i" ] || exit 0
while :; do
  c=
  while read -r l; do
    case $l in "$i:"*) c=${l#*:}; break ;; esac
  done < /proc/net/dev
  [ -n "$c" ] || exit 0
  set -- $c
  printf 'wayterm-net:%s %s %s\n' "$i" "$1" "$9" || exit 0
  sleep {interval}
done
"#;

fn sample_script() -> String {
    SAMPLE_SCRIPT.replace("{interval}", &INTERVAL.as_secs().to_string())
}

/// One interface's byte counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Counters {
    pub interface: String,
    pub rx: u64,
    pub tx: u64,
}

/// Bytes per second over the last interval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rate {
    pub interface: String,
    pub rx_per_sec: u64,
    pub tx_per_sec: u64,
}

/// Turns consecutive samples into a [`Rate`].
#[derive(Debug, Default)]
pub struct RateMeter {
    last: Option<(Counters, Instant)>,
}

impl RateMeter {
    /// The rate since the previous sample: `None` for the first, after the
    /// interface changed, or when a counter went backwards (the interface
    /// was reset).
    pub fn add(&mut self, counters: Counters, at: Instant) -> Option<Rate> {
        let rate = self.last.as_ref().and_then(|(last, then)| {
            let millis = at.checked_duration_since(*then)?.as_millis() as u64;
            if millis == 0 || last.interface != counters.interface {
                return None;
            }
            let per_sec = |now: u64, before: u64| Some(now.checked_sub(before)? * 1000 / millis);
            Some(Rate {
                interface: counters.interface.clone(),
                rx_per_sec: per_sec(counters.rx, last.rx)?,
                tx_per_sec: per_sec(counters.tx, last.tx)?,
            })
        });
        self.last = Some((counters, at));
        rate
    }
}

/// A sample line from the script: `wayterm-net:<interface> <rx> <tx>`.
pub fn parse_sample(line: &str) -> Option<Counters> {
    let mut fields = line.trim().strip_prefix(MARKER)?.split_whitespace();
    let counters = Counters {
        interface: fields.next()?.to_string(),
        rx: fields.next()?.parse().ok()?,
        tx: fields.next()?.parse().ok()?,
    };
    fields.next().is_none().then_some(counters)
}

/// The interface of `/proc/net/route`'s default route.
pub fn default_route_interface(route: &str) -> Option<String> {
    route.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let interface = fields.next()?;
        (fields.next()? == "00000000").then(|| interface.to_string())
    })
}

/// The interface of `/proc/net/ipv6_route`'s default route (`::/0`),
/// skipping the kernel's unreachable defaults, which are always on `lo`.
pub fn default_ipv6_route_interface(route: &str) -> Option<String> {
    const ANY: &str = "00000000000000000000000000000000";
    route.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [dest, prefix, .., interface] = fields.as_slice() else {
            return None;
        };
        (fields.len() == 10 && *dest == ANY && *prefix == "00" && *interface != "lo")
            .then(|| interface.to_string())
    })
}

/// `interface`'s counters in `/proc/net/dev`: received bytes is the first
/// field after the colon, sent bytes the ninth.
pub fn interface_counters(dev: &str, interface: &str) -> Option<Counters> {
    dev.lines().find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        if name.trim() != interface {
            return None;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        Some(Counters {
            interface: interface.to_string(),
            rx: fields.first()?.parse().ok()?,
            tx: fields.get(8)?.parse().ok()?,
        })
    })
}

/// This machine's default-route interface counters.
pub fn local_counters() -> Option<Counters> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let interface = read("/proc/net/route")
        .and_then(|route| default_route_interface(&route))
        .or_else(|| {
            read("/proc/net/ipv6_route").and_then(|route| default_ipv6_route_interface(&route))
        })?;
    interface_counters(&read("/proc/net/dev")?, &interface)
}

/// Send the server's samples to `samples` until its receiver is dropped or
/// the server stops (`remote_script::stream`).
pub async fn stream_server(connection: Weak<SshConnection>, samples: mpsc::Sender<Counters>) {
    remote_script::stream(connection, &sample_script(), parse_sample, samples).await;
}

#[cfg(test)]
mod tests;
