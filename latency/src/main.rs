#![feature(async_fn_in_trait)]
#![feature(associated_type_defaults)]

use std::{net::SocketAddr, time::Duration};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
mod args;
mod protocol;
mod tcp;
mod traits;
mod utils;
#[tokio::main(flavor = "current_thread")]
async fn main() {
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LatencyMsg {
    id: u64,
    seq: u64,
    timestamp: u64,
}

pub trait Latency {
    async fn send(&mut self, buf: &[u8]) -> Result<usize>;
    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize>;
}

mod socket_kind {
    pub struct Server {}
    pub struct Client {}
}

mod conn_state {
    pub struct Connected {}
    pub struct Disconnected {}
}

pub struct UdpLatency<Kind> {
    inner: tokio::net::UdpSocket,
    peer: Option<std::net::SocketAddr>,
    kind: std::marker::PhantomData<Kind>,
}
impl UdpLatency<socket_kind::Server> {
    pub fn new(local: &str) -> Self {
        let local: SocketAddr = local.parse().unwrap();

        let socket =
            common::UDPSocket::new(None, Some((local.ip(), local.port())))
                .unwrap();

        let inner = tokio::net::UdpSocket::from_std(
            socket.get_ref().try_clone().unwrap().into(),
        )
        .unwrap();

        Self {
            inner,
            peer: None,
            kind: std::marker::PhantomData,
        }
    }
}

impl UdpLatency<socket_kind::Client> {
    pub fn new(remote: &str) -> Self {
        let remote = remote.parse().unwrap();
        let socket = common::UDPSocket::new(None, None).unwrap();
        let inner = tokio::net::UdpSocket::from_std(
            socket.get_ref().try_clone().unwrap().into(),
        )
        .unwrap();

        Self {
            inner,
            peer: Some(remote),
            kind: std::marker::PhantomData,
        }
    }
}

impl<Kind> Latency for UdpLatency<Kind> {
    async fn send(&mut self, buf: &[u8]) -> Result<usize> {
        let len = self.inner.send_to(buf, self.peer.unwrap()).await?;
        Ok(len)
    }

    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {
        let (len, recv_addr) = self.inner.recv_from(buf).await?;
        if self.peer.is_some() {
            if recv_addr != self.peer.unwrap() {
                return Err(anyhow::anyhow!("recv from wrong addr"));
            }
        } else {
            self.peer = Some(recv_addr);
        }

        Ok(len)
    }
}

struct ClientCtx {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    fut: tokio::task::JoinHandle<()>,
}

impl ClientCtx {
    fn new(
        stop: tokio::sync::oneshot::Sender<()>,
        fut: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            stop: Some(stop),
            fut,
        }
    }
}

struct CtrlServer {
    quinn: common::QuicServer,
    clients: Vec<ClientCtx>,
}

impl CtrlServer {
    fn new(lst_addr: SocketAddr) -> Self {
        let quinn =
            common::QuicServer::new((lst_addr.ip(), lst_addr.port())).unwrap();
        Self {
            quinn,
            clients: Vec::new(),
        }
    }

    async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                Some(connecting) = self.quinn.server.accept() => {
                    let conn = connecting.await?;
                    let (tx, rx) = conn.open_bi().await?;
                    self.handle_client(tx, rx).await?;
                }
            _ = tokio::signal::ctrl_c() => {
                    for client in self.clients.iter_mut() {
                        client.stop.take().unwrap().send(()).unwrap();
                    }
                    println!("Ctrl-C received, shutting down");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_client(
        &mut self,
        tx: quinn::SendStream,
        rx: quinn::RecvStream,
    ) -> Result<()> {
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let mut client = Client::new(tx, rx, stop_rx, 1000, 1000, 1000);
        let fut = tokio::spawn(async move {
            client.run().await;
        });
        self.clients.push(ClientCtx::new(stop, fut));
        Ok(())
    }
}

mod side {
    pub struct Server {}
    pub struct Client {}
}

struct Client {
    ctx: Vec<ConnCtx>,
    timeout: u64,
    interval: u64,
    packet_size: u64,
    tx_ctrl: quinn::SendStream,
    rx_ctrl: quinn::RecvStream,
    stop: tokio::sync::oneshot::Receiver<()>,
    id: u64,
}

impl Client {
    fn new(
        tx_ctrl: quinn::SendStream,
        rx_ctrl: quinn::RecvStream,
        stop: tokio::sync::oneshot::Receiver<()>,
        timeout: u64,
        interval: u64,
        packet_size: u64,
    ) -> Self {
        Self {
            ctx: Vec::new(),
            timeout,
            interval,
            packet_size,
            tx_ctrl,
            rx_ctrl,
            stop,
            id: 0,
        }
    }

    async fn run(&mut self) {
        let mut snd_timer =
            tokio::time::interval(Duration::from_millis(self.interval));
        let packet_size = self.packet_size;
        assert!(packet_size <= u16::MAX as u64);
        let mut sndbuf = [0u8; u16::MAX as usize];
        let mut recvbuf = [0u8; u16::MAX as usize];

        loop {
            tokio::select! {
                _ = snd_timer.tick() => {
                    let sndbuf = sndbuf[..packet_size as usize].to_vec();
                    for ctx in self.ctx.iter_mut() {
                        ctx.bidi.write_all(&sndbuf).await.unwrap();
                    }
                }
                Ok(Some(len)) = self.rx_ctrl.read(&mut recvbuf) => {
                    let recvbuf = recvbuf[..len].to_vec();
                    self.handle_ctrl_msg(&recvbuf).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(self.timeout)) => {
                    for ctx in self.ctx.iter_mut() {
                        ctx.stop.take().unwrap().send(()).unwrap();
                    }
                    break;
                }
                _ = &mut self.stop => {
                    for ctx in self.ctx.iter_mut() {
                        ctx.stop.take().unwrap().send(()).unwrap();
                    }
                    break;
                }

            }
        }
    }

    async fn handle_ctrl_msg(&mut self, msg: &[u8]) {
        todo!()
    }

    async fn handshake(
        &mut self,
        mut complete: tokio::sync::oneshot::Receiver<()>,
        timeout: Duration,
        interval: Duration,
    ) -> Result<()> {
        let handshake =
            protocol::ClientMessage::Handshake(protocol::ClientHandshake {
                id: self.id as u64,
                protocol: protocol::TestType::Udp as u64,
            });
        let mut handshake_timer = tokio::time::interval(interval);

        loop {
            tokio::select! {
                _ = &mut complete => {
                    break Ok(());
                },
                _ = handshake_timer.tick() => {
                    let msg = bincode::serialize(&handshake).unwrap();
                    for ctx in self.ctx.iter_mut() {
                        ctx.bidi.write_all(&msg).await.unwrap();
                    }
                },

                _ = tokio::time::sleep(timeout) => {
                    return Err(anyhow!("handshake timeout"));
                }
            }
        }
    }
}

#[derive(Debug)]
struct ConnCtx<State = conn_state::Disconnected> {
    bidi: tokio::io::DuplexStream,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    state: std::marker::PhantomData<State>,
}

impl ConnCtx<conn_state::Disconnected> {
    fn new(
        bidi: tokio::io::DuplexStream,
        stop: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        Self {
            bidi,
            stop: Some(stop),
            state: std::marker::PhantomData,
        }
    }
}

struct ConnRunner<T: Latency> {
    bidi: tokio::io::DuplexStream,
    stop: tokio::sync::oneshot::Receiver<()>,
    socket: T,
}

impl<T: Latency> ConnRunner<T> {
    fn new(
        socket: T,
        bidi: tokio::io::DuplexStream,
        stop: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        Self { bidi, socket, stop }
    }

    async fn run(&mut self) {
        let mut sndbuf = [0u8; u16::MAX as usize];
        let mut recvbuf = [0u8; u16::MAX as usize];

        loop {
            tokio::select! {
                len = self.bidi.read(&mut sndbuf) => {
                    let sndsndbuf = sndbuf[..len.unwrap()].to_vec();
                    self.socket.send(&sndsndbuf).await.unwrap();
                }
                len = self.socket.recv(&mut recvbuf) => {
                    let recvbuf = recvbuf[..len.unwrap()].to_vec();
                    self.bidi.write_all(&recvbuf).await.unwrap();
                }
                _ = &mut self.stop => {
                    break;
                }
            }
        }
    }
}

mod test {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn test_server() -> ConnCtx {
        let (p1, mut p2) = tokio::io::duplex(1024);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let socket = UdpLatency::<socket_kind::Server>::new("127.0.0.1:12345");
        let mut runner = ConnRunner::new(socket, p1, rx);
        let ctx = ConnCtx::new(p2, tx);
        tokio::spawn(async move {
            runner.run().await;
        });
        ctx
    }
    async fn test_client() -> ConnCtx {
        let (p1, mut p2) = tokio::io::duplex(1024);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let socket = UdpLatency::<socket_kind::Client>::new("127.0.0.1:12345");
        let mut runner = ConnRunner::new(socket, p1, rx);
        let ctx = ConnCtx::new(p2, tx);
        tokio::spawn(async move {
            runner.run().await;
        });
        ctx
    }

    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn test() {
        let mut client_ctx = test_client().await;
        let mut server_ctx = test_server().await;

        let mut sndbuf = [0u8; u16::MAX as usize];
        let mut recvbuf = [0u8; u16::MAX as usize];

        let mut snd_timer = tokio::time::interval(Duration::from_millis(1000));
        let timeout = Duration::from_millis(10000);

        loop {
            tokio::select! {
                _ = snd_timer.tick() => {
                    let sndbuf = "hello".as_bytes().to_vec();
                    client_ctx.bidi.write_all(&sndbuf).await.unwrap();
                }
                Ok(len) = server_ctx.bidi.read(&mut recvbuf) => {
                    let recvbuf = recvbuf[..len].to_vec();
                    server_ctx.bidi.write_all(&recvbuf).await.unwrap();
                    println!("recv: {:?}", recvbuf);
                }
            }
        }
    }
}
