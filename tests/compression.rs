//! SSH compression against an in-process russh server (doc 04
//! §Compression). Needs no sshd.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::compression::{self, Name};
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, ChannelMsg, Preferred};
use tokio::net::TcpListener;
use tokio::time::timeout;

use wayterm::transport::supervisor::{ConnectParams, ConnectionSupervisor, HopAuth};

/// Accepts any password and session channel, and echoes channel data.
#[derive(Default)]
struct Echo {
    channels: Vec<Channel<Msg>>,
}

impl server::Handler for Echo {
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

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())
    }
}

/// A server that accepts only the `offered` compression methods.
async fn start_server(offered: &'static [Name]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = server::Config {
        preferred: Preferred {
            compression: offered.into(),
            ..Preferred::default()
        },
        ..server::Config::default()
    };
    config.keys.push(common::host_key());
    let config = Arc::new(config);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, stream, Echo::default()).await {
                    let _ = session.await;
                }
            });
        }
    });
    addr
}

fn params(addr: SocketAddr) -> ConnectParams {
    ConnectParams {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "u".to_string(),
    }
}

fn auth(compression: bool) -> HopAuth {
    HopAuth {
        compression,
        ..common::password_auth("any")
    }
}

/// Sends a compressible payload over a session channel, reads it back,
/// and returns what compression saved on the way in.
async fn echo_round_trip(
    supervisor: &ConnectionSupervisor,
    addr: SocketAddr,
    compression: bool,
) -> Option<u8> {
    let conn = supervisor
        .get_or_connect(&params(addr), &auth(compression))
        .await
        .expect("connect");
    let mut channel = conn.open_session_channel().await.expect("open a channel");
    let payload = "the same line, over and over\n".repeat(4096).into_bytes();
    channel.data(&payload[..]).await.expect("send");
    let mut echoed = Vec::new();
    timeout(Duration::from_secs(10), async {
        while echoed.len() < payload.len() {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => echoed.extend_from_slice(&data),
                Some(_) => {}
                None => panic!("the channel closed after {} bytes", echoed.len()),
            }
        }
    })
    .await
    .expect("the echo arrives");
    assert_eq!(echoed, payload);
    conn.traffic().savings_percent()
}

/// A server that only speaks delayed zlib proves the offer reaches the
/// wire: a compressing session connects and carries data, an
/// uncompressed one finds no common method.
#[tokio::test]
async fn a_compressing_session_negotiates_delayed_zlib() {
    let addr = start_server(&[compression::ZLIB_LEGACY]).await;
    let supervisor = ConnectionSupervisor::new();
    let saved = echo_round_trip(&supervisor, addr, true).await;
    assert!(
        saved.is_some_and(|p| p > 50),
        "repeated lines compress: {saved:?}"
    );

    let refused = supervisor.get_or_connect(&params(addr), &auth(false)).await;
    assert!(
        refused.is_err(),
        "without compression there is no common method"
    );
}

/// A server without zlib still takes a compressing session, uncompressed,
/// and the measured saving says so.
#[tokio::test]
async fn a_server_without_zlib_connects_uncompressed() {
    let addr = start_server(&[compression::NONE]).await;
    let supervisor = ConnectionSupervisor::new();
    let saved = echo_round_trip(&supervisor, addr, true).await;
    assert!(
        saved.is_some_and(|p| p <= 5),
        "nothing compressed: {saved:?}"
    );
}

/// A session that never offered compression shows no saving.
#[tokio::test]
async fn an_uncompressed_session_reports_no_saving() {
    let addr = start_server(&[compression::NONE]).await;
    let supervisor = ConnectionSupervisor::new();
    assert_eq!(echo_round_trip(&supervisor, addr, false).await, None);
}
