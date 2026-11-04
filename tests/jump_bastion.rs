//! A jumped session's hold on its bastion, through the app's own dial
//! (`dial_with_jump`) against an in-process russh server that plays both
//! hops (doc 04 §Jump hosts). Needs no sshd.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::Channel;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use wayterm::store::model::SessionConfig;
use wayterm::transport::jump::dial_with_jump;
use wayterm::transport::supervisor::ConnectionSupervisor;

/// Comfortably past the supervisor's 250 ms release poll.
const RELEASE_WINDOW: Duration = Duration::from_secs(1);

/// Accepts any password and session channel, and bridges direct-tcpip
/// channels to the address they name.
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

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let address = format!("{host_to_connect}:{port_to_connect}");
        reply.accept().await;
        tokio::spawn(async move {
            let mut outbound = TcpStream::connect(address).await.unwrap();
            let mut inbound = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        });
        Ok(())
    }
}

/// The server's port, and a count of its live SSH sessions.
async fn start_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = server::Config::default();
    config.keys.push(common::host_key());
    let config = Arc::new(config);
    let live = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&live);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            let count = Arc::clone(&count);
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, stream, Server::default()).await {
                    let _ = session.await;
                }
                count.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (port, live)
}

fn session(id: i64, jump_session_id: Option<i64>, port: u16) -> SessionConfig {
    SessionConfig {
        id,
        name: format!("session-{id}"),
        host: Some("127.0.0.1".to_string()),
        port: Some(port),
        username: Some("user".to_string()),
        jump_session_id,
        ..SessionConfig::default()
    }
}

#[tokio::test]
async fn a_jumped_session_holds_its_bastion_until_it_is_dropped() {
    let (port, live) = start_server().await;
    let supervisor = ConnectionSupervisor::new();
    let auth = common::password_auth("any");

    let target = dial_with_jump(
        &supervisor,
        &session(1, None, port),
        &auth,
        &session(2, Some(1), port),
        &auth,
    )
    .await
    .expect("jump dial failed");

    tokio::time::sleep(RELEASE_WINDOW).await;
    target
        .open_session_channel()
        .await
        .expect("the target must outlive the bastion's release poll");
    assert_eq!(live.load(Ordering::SeqCst), 2, "bastion and target");

    drop(target);
    timeout(Duration::from_secs(10), async {
        while live.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("dropping the target must release the bastion");
}
