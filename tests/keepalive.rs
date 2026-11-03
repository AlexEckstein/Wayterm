//! Keepalives against an in-process russh server behind a proxy that can go
//! silent the way a dropped Wi-Fi link does: no bytes either way, no FIN, no
//! RST (doc 04 §Keepalives). Needs no sshd.

mod common;

use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::PrivateKey;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelMsg};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth, Keepalive};

const FAST: Keepalive = Keepalive {
    interval: Some(Duration::from_millis(200)),
    max: 2,
};

/// Accepts any password and any session channel, and keeps the channels.
#[derive(Default)]
struct Server {
    channels: Vec<Channel<Msg>>,
}

impl server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.push(channel);
        reply.accept().await;
        Ok(())
    }
}

/// A throwaway host key, generated per run so none is ever committed.
fn host_key() -> PrivateKey {
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut seed))
        .expect("read /dev/urandom");
    PrivateKey::from(Ed25519Keypair::from_seed(&seed))
}

async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = server::Config::default();
    config.keys.push(host_key());
    let config = Arc::new(config);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, stream, Server::default()).await {
                    let _ = session.await;
                }
            });
        }
    });
    addr
}

/// Forwards to the server until [`Proxy::go_silent`]; from then on every
/// connection already accepted stays open but passes nothing. Later
/// connections are forwarded again.
struct Proxy {
    port: u16,
    live: Arc<Mutex<CancellationToken>>,
}

impl Proxy {
    async fn start(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let live = Arc::new(Mutex::new(CancellationToken::new()));
        let tokens = Arc::clone(&live);
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let silence = tokens.lock().unwrap().clone();
                tokio::spawn(async move {
                    let mut outbound = TcpStream::connect(target).await.unwrap();
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => return,
                        () = silence.cancelled() => {}
                    }
                    let _held_open = (inbound, outbound);
                    std::future::pending::<()>().await;
                });
            }
        });
        Self { port, live }
    }

    fn go_silent(&self) {
        let mut live = self.live.lock().unwrap();
        live.cancel();
        *live = CancellationToken::new();
    }
}

fn params(proxy: &Proxy) -> ConnectParams {
    ConnectParams {
        host: "127.0.0.1".into(),
        port: proxy.port,
        username: "user".into(),
    }
}

fn auth(keepalive: Keepalive) -> HopAuth {
    HopAuth {
        keepalive,
        ..common::password_auth("any")
    }
}

#[tokio::test]
async fn a_silent_link_closes_the_connection_and_the_pool_redials() {
    let proxy = Proxy::start(start_server().await).await;
    let supervisor = ConnectionSupervisor::new();
    let auth = auth(FAST);

    let dead = supervisor
        .get_or_connect(&params(&proxy), &auth)
        .await
        .expect("connect through the proxy");
    let mut channel = dead.open_session_channel().await.expect("open a channel");
    proxy.go_silent();

    // A session channel ending without an exit status is what the reader
    // reports as `ConnectionLost`.
    let ended = timeout(Duration::from_secs(5), async {
        while let Some(msg) = channel.wait().await {
            assert!(
                !matches!(
                    msg,
                    ChannelMsg::ExitStatus { .. } | ChannelMsg::ExitSignal { .. }
                ),
                "a dead link must not look like a session end: {msg:?}"
            );
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "keepalives must end the channel on a silent link"
    );
    assert!(dead.is_closed());

    // Held, as by another tab: the pool must still dial fresh.
    let fresh = supervisor
        .get_or_connect(&params(&proxy), &auth)
        .await
        .expect("redial through the proxy");
    assert!(!Arc::ptr_eq(&dead, &fresh));
    fresh
        .open_session_channel()
        .await
        .expect("the fresh connection carries channels");
}

#[tokio::test]
async fn without_keepalives_a_silent_link_goes_unnoticed() {
    let proxy = Proxy::start(start_server().await).await;
    let supervisor = ConnectionSupervisor::new();
    let off = Keepalive {
        interval: None,
        ..FAST
    };

    let conn = supervisor
        .get_or_connect(&params(&proxy), &auth(off))
        .await
        .expect("connect through the proxy");
    proxy.go_silent();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!conn.is_closed());
}
