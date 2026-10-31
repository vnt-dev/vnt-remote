//! Media packets use either native UDP or the VNT application's QUIC datagrams.
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

pub enum Datagram {
    Udp(tokio::net::UdpSocket),
    Shared(crate::quic::SharedUdp),
    Quic(Arc<crate::quic::Media>),
    Overlay(Arc<OverlayDatagrams>),
    Peer {
        hub: Arc<OverlayDatagrams>,
        peer: SocketAddr,
        packets: Mutex<tokio::sync::broadcast::Receiver<Vec<u8>>>,
    },
    Proxy {
        reader: Mutex<FramedRead<tokio::net::tcp::OwnedReadHalf, LengthDelimitedCodec>>,
        writer: Box<Mutex<FramedWrite<tokio::net::tcp::OwnedWriteHalf, LengthDelimitedCodec>>>,
        local: SocketAddr,
        peer: SocketAddr,
    },
}
pub struct OverlayDatagrams {
    pub(crate) connections: RwLock<HashMap<IpAddr, quinn::Connection>>,
    pub(crate) outbound: RwLock<HashMap<IpAddr, quinn::Connection>>,
    tx: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    rx: Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
    sequence: AtomicU64,
    subscribers: std::sync::Mutex<HashMap<IpAddr, tokio::sync::broadcast::Sender<Vec<u8>>>>,
    pub(crate) local: std::sync::RwLock<SocketAddr>,
}
impl Default for OverlayDatagrams {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(1024);
        Self {
            subscribers: Default::default(),
            connections: RwLock::new(HashMap::new()),
            outbound: RwLock::new(HashMap::new()),
            tx,
            rx: Mutex::new(rx),
            sequence: AtomicU64::new(1),
            local: std::sync::RwLock::new("0.0.0.0:29875".parse().unwrap()),
        }
    }
}
impl OverlayDatagrams {
    pub fn subscribe(&self, peer: IpAddr) -> tokio::sync::broadcast::Receiver<Vec<u8>> {
        self.subscribers
            .lock()
            .unwrap()
            .entry(peer)
            .or_insert_with(|| tokio::sync::broadcast::channel(1024).0)
            .subscribe()
    }
    pub fn set_local(&self, local: SocketAddr) {
        *self.local.write().unwrap() = local;
    }
    pub async fn register(
        self: &Arc<Self>,
        connection: quinn::Connection,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let ip = connection.remote_address().ip();
        self.connections
            .write()
            .await
            .insert(ip, connection.clone());
        let this = self.clone();
        tokio::spawn(async move {
            struct Partial {
                at: Instant,
                parts: Vec<Option<Vec<u8>>>,
                bytes: usize,
            }
            let mut packets = HashMap::<u64, Partial>::new();
            loop {
                let data = tokio::select! { _ = cancel.cancelled() => break, result = connection.read_datagram() => match result { Ok(data) => data, Err(_) => break } };
                if data.len() < 12 {
                    continue;
                }
                let id = u64::from_be_bytes(data[..8].try_into().unwrap());
                let part = u16::from_be_bytes(data[8..10].try_into().unwrap()) as usize;
                let count = u16::from_be_bytes(data[10..12].try_into().unwrap()) as usize;
                if count == 0 || count > 64 || part >= count {
                    continue;
                }
                packets.retain(|_, value| value.at.elapsed() < Duration::from_millis(150));
                if packets.len() >= 128 && !packets.contains_key(&id) {
                    continue;
                }
                let packet = packets.entry(id).or_insert_with(|| Partial {
                    at: Instant::now(),
                    parts: vec![None; count],
                    bytes: 0,
                });
                if packet.parts.len() != count || packet.parts[part].is_some() {
                    continue;
                }
                packet.bytes += data.len() - 12;
                if packet.bytes > 65536 {
                    packets.remove(&id);
                    continue;
                }
                packet.parts[part] = Some(data[12..].to_vec());
                if packet.parts.iter().all(Option::is_some) {
                    let packet = packets.remove(&id).unwrap();
                    let mut data = Vec::with_capacity(packet.bytes);
                    for part in packet.parts {
                        data.extend(part.unwrap());
                    }
                    if let Some(subscriber) = this.subscribers.lock().unwrap().get(&ip) {
                        let _ = subscriber.send(data.clone());
                    }
                    let _ = this.tx.try_send((data, SocketAddr::new(ip, 29875)));
                }
            }
            let mut connections = this.connections.write().await;
            if connections
                .get(&ip)
                .is_some_and(|c| c.stable_id() == connection.stable_id())
            {
                connections.remove(&ip);
            }
        });
    }
}
impl Datagram {
    /// Leave room for the application QUIC headers and VNT encryption envelope.
    pub fn packet_limit(&self) -> usize {
        if let Self::Quic(media) = self {
            return media.packet_limit();
        }
        if matches!(self, Self::Udp(_) | Self::Shared(_)) {
            1423
        } else {
            1100
        }
    }

    pub async fn bind(address: impl tokio::net::ToSocketAddrs) -> io::Result<Self> {
        Ok(Self::Udp(tokio::net::UdpSocket::bind(address).await?))
    }
    pub fn proxy(stream: tokio::net::TcpStream, local: SocketAddr, peer: SocketAddr) -> Self {
        let (read, write) = stream.into_split();
        let codec = || {
            LengthDelimitedCodec::builder()
                .max_frame_length(65536)
                .new_codec()
        };
        Self::Proxy {
            reader: Mutex::new(FramedRead::new(read, codec())),
            writer: Box::new(Mutex::new(FramedWrite::new(write, codec()))),
            local,
            peer,
        }
    }
    pub fn overlay_peer(hub: Arc<OverlayDatagrams>, peer: SocketAddr) -> Self {
        let packets = Mutex::new(hub.subscribe(peer.ip()));
        Self::Peer { hub, peer, packets }
    }
    pub fn overlay(hub: Arc<OverlayDatagrams>) -> Self {
        Self::Overlay(hub)
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Udp(socket) => socket.local_addr(),
            Self::Shared(socket) => socket.local_addr(),
            Self::Quic(media) => Ok(media.local_addr()),
            Self::Overlay(hub) | Self::Peer { hub, .. } => Ok(*hub.local.read().unwrap()),
            Self::Proxy { local, .. } => Ok(*local),
        }
    }
    pub fn configure_send_buffer(&self, size: usize) -> io::Result<()> {
        match self {
            Self::Udp(socket) => socket2::SockRef::from(socket).set_send_buffer_size(size),
            _ => Ok(()),
        }
    }
    pub fn configure_recv_buffer(&self, size: usize) -> io::Result<()> {
        match self {
            Self::Udp(socket) => socket2::SockRef::from(socket).set_recv_buffer_size(size),
            _ => Ok(()),
        }
    }
    pub async fn send_to(&self, packet: &[u8], target: SocketAddr) -> io::Result<usize> {
        match self {
            Self::Udp(socket) => socket.send_to(packet, target).await,
            Self::Shared(socket) => socket.send(packet, target).await,
            Self::Quic(media) => media.send(packet, target).await,
            Self::Proxy { writer, peer, .. } => {
                if target.ip() != peer.ip() {
                    return Err(io::Error::other("incorrect media peer"));
                }
                writer
                    .lock()
                    .await
                    .send(Bytes::copy_from_slice(packet))
                    .await?;
                Ok(packet.len())
            }
            Self::Overlay(hub) | Self::Peer { hub, .. } => {
                if packet.len() > 65536 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "media packet too large",
                    ));
                }
                let connection = hub
                    .connections
                    .read()
                    .await
                    .get(&target.ip())
                    .cloned()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotConnected, "VNT media peer offline")
                    })?;
                let mtu = connection
                    .max_datagram_size()
                    .ok_or_else(|| io::Error::other("VNT datagrams unavailable"))?
                    .saturating_sub(12)
                    .min(1100);
                if mtu < 64 {
                    return Err(io::Error::other("VNT datagram MTU too small"));
                }
                let count = packet.len().div_ceil(mtu);
                let id = hub.sequence.fetch_add(1, Ordering::Relaxed);
                for (index, part) in packet.chunks(mtu).enumerate() {
                    let mut data = Vec::with_capacity(12 + part.len());
                    data.extend(id.to_be_bytes());
                    data.extend((index as u16).to_be_bytes());
                    data.extend((count as u16).to_be_bytes());
                    data.extend(part);
                    connection
                        .send_datagram(Bytes::from(data))
                        .map_err(io::Error::other)?;
                }
                Ok(packet.len())
            }
        }
    }
    pub async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(socket) => socket.recv_from(buffer).await,
            Self::Shared(socket) => socket.recv(buffer).await,
            Self::Quic(media) => media.recv(buffer).await,
            Self::Peer { packets, peer, .. } => {
                let mut packets = packets.lock().await;
                let packet = loop {
                    match packets.recv().await {
                        Ok(packet) => break packet,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return Err(io::Error::other("VNT media closed")),
                    }
                };
                if packet.len() > buffer.len() {
                    return Err(io::Error::other("media buffer too small"));
                }
                buffer[..packet.len()].copy_from_slice(&packet);
                Ok((packet.len(), *peer))
            }
            Self::Proxy { reader, peer, .. } => {
                let packet = reader
                    .lock()
                    .await
                    .next()
                    .await
                    .ok_or_else(|| io::Error::other("local media closed"))??;
                if packet.len() > buffer.len() {
                    return Err(io::Error::other("media buffer too small"));
                }
                buffer[..packet.len()].copy_from_slice(&packet);
                Ok((packet.len(), *peer))
            }
            Self::Overlay(hub) => {
                let (packet, peer) =
                    hub.rx.lock().await.recv().await.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "VNT media closed")
                    })?;
                if packet.len() > buffer.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "media buffer too small",
                    ));
                }
                buffer[..packet.len()].copy_from_slice(&packet);
                Ok((packet.len(), peer))
            }
        }
    }
}
