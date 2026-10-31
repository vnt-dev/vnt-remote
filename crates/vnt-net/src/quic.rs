//! Password-pinned direct QUIC. This endpoint never uses the VNT overlay.
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    io::{self, IoSliceMut},
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use quinn::{AsyncUdpSocket, Connection, Endpoint, Runtime};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    datagram::Datagram,
    session::{BoxStream, Dialer, SessionChannel, SessionListener},
};

const ALPN: &[u8] = b"vnt-direct-quic/1";
const DEFAULT_PASSWORD: &str = "vnt-desk/direct-quic/passwordless/v1";
const SALT: &[u8] = b"vnt-desk/direct-quic/tls-key/v1";
const NAME: &str = "vnt-direct.invalid";
const HANDSHAKE: Duration = Duration::from_secs(8);

fn err(error: impl fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn identity(password: &str) -> io::Result<(rcgen::KeyPair, Vec<u8>)> {
    use ring::signature::KeyPair;
    let password = if password.trim().is_empty() {
        DEFAULT_PASSWORD
    } else {
        password
    };
    let mut seed = [0; 32];
    let params = argon2::Params::new(19456, 2, 1, Some(32)).map_err(err)?;
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), SALT, &mut seed)
        .map_err(err)?;
    let key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).map_err(err)?;
    let public = key.public_key().as_ref().to_vec();
    // RFC 8410 PKCS#8 OneAsymmetricKey, containing the original Ed25519 seed.
    // ring's generator is deliberately not used: the identity is deterministic.
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&seed);
    seed.fill(0);
    let key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(der),
        &rcgen::PKCS_ED25519,
    )
    .map_err(err)?;
    Ok((key, public))
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.datagram_receive_buffer_size(Some(16 * 1024 * 1024));
    config.datagram_send_buffer_size(2 * 1024 * 1024);
    config.max_concurrent_bidi_streams(16u32.into());
    config.max_concurrent_uni_streams(0u32.into());
    config.keep_alive_interval(Some(Duration::from_secs(2)));
    config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    Arc::new(config)
}

pub type Password = Arc<dyn Fn() -> io::Result<String> + Send + Sync>;
pub type ValidateBind = Arc<dyn Fn(u32, &[u8], SocketAddr) -> bool + Send + Sync>;

fn server_config(password: &str) -> io::Result<quinn::ServerConfig> {
    let (key, _) = identity(password)?;
    let certificate = rcgen::CertificateParams::new(vec![NAME.into()])
        .map_err(err)?
        .self_signed(&key)
        .map_err(err)?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(err)?
    .with_no_client_auth()
    .with_single_cert(
        vec![certificate.der().clone()],
        PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    )
    .map_err(err)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(err)?,
    ));
    config.transport_config(transport());
    // The session protocol currently binds recovery to the peer IP/address.
    config.migration(false);
    Ok(config)
}

#[derive(Debug)]
struct PinnedKey(Vec<u8>);
impl ServerCertVerifier for PinnedKey {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let (_, cert) = x509_parser::parse_x509_certificate(cert.as_ref()).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        let key = cert.public_key();
        if key.algorithm.algorithm.to_id_string() != "1.3.101.112"
            || key.subject_public_key.data.as_ref() != self.0.as_slice()
        {
            return Err(rustls::Error::General(
                "QUIC 密码或服务端证书校验失败".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            msg,
            cert,
            sig,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            msg,
            cert,
            sig,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn client_config(password: &str) -> io::Result<quinn::ClientConfig> {
    let (_, public) = identity(password)?;
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(err)?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(PinnedKey(public)))
    .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.enable_early_data = false;
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(err)?,
    ));
    config.transport_config(transport());
    Ok(config)
}

/// One per logical session, updated only after authenticated control recovery.
pub struct Media {
    connection: watch::Sender<Connection>,
    local: SocketAddr,
    peer: SocketAddr,
    sequence: AtomicU64,
    receiver: tokio::sync::Mutex<MediaReceiver>,
}
#[derive(Default)]
struct MediaReceiver {
    connection: usize,
    partial: HashMap<u64, PartialPacket>,
}
struct PartialPacket {
    at: Instant,
    parts: Vec<Option<Bytes>>,
    bytes: usize,
}
impl Media {
    fn new(connection: Connection, local: SocketAddr, peer: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            connection: watch::channel(connection).0,
            local,
            peer,
            sequence: AtomicU64::new(0),
            receiver: Default::default(),
        })
    }
    pub(crate) fn replace(&self, other: &Self) {
        self.connection
            .send_replace(other.connection.borrow().clone());
    }
    pub fn packet_limit(&self) -> usize {
        self.connection
            .borrow()
            .max_datagram_size()
            .unwrap_or(1100)
            .saturating_sub(12)
            .min(1423)
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }
    pub async fn send(&self, packet: &[u8], target: SocketAddr) -> io::Result<usize> {
        if target != self.peer {
            return Err(err("incorrect QUIC media peer"));
        }
        let connection = self.connection.borrow().clone();
        if connection.close_reason().is_some() {
            return Ok(packet.len());
        }
        let mtu = connection
            .max_datagram_size()
            .ok_or_else(|| err("QUIC DATAGRAM unavailable"))?
            .saturating_sub(12);
        if mtu < 64 || packet.is_empty() || packet.len() > 65536 {
            return Err(err("invalid QUIC media packet size"));
        }
        let count = packet.len().div_ceil(mtu);
        if count > 64 {
            return Err(err("too many QUIC media fragments"));
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        for (index, part) in packet.chunks(mtu).enumerate() {
            let mut data = Vec::with_capacity(12 + part.len());
            data.extend_from_slice(&sequence.to_be_bytes());
            data.extend_from_slice(&(index as u16).to_be_bytes());
            data.extend_from_slice(&(count as u16).to_be_bytes());
            data.extend_from_slice(part);
            match connection.send_datagram(Bytes::from(data)) {
                Ok(()) => {}
                Err(quinn::SendDatagramError::ConnectionLost(_)) => return Ok(packet.len()),
                Err(e) => return Err(err(e)),
            }
        }
        Ok(packet.len())
    }
    pub async fn recv(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut receiver = self.receiver.lock().await;
        let mut changes = self.connection.subscribe();
        loop {
            let connection = changes.borrow_and_update().clone();
            if receiver.connection != connection.stable_id() {
                receiver.connection = connection.stable_id();
                receiver.partial.clear();
            }
            let packet = tokio::select! {
                biased;
                result = changes.changed() => { result.map_err(err)?; continue; }
                result = connection.read_datagram() => match result {
                    Ok(packet) => packet,
                    Err(_) => { changes.changed().await.map_err(err)?; continue; }
                }
            };
            if changes.has_changed().unwrap_or(true) {
                continue;
            }
            if packet.len() < 12 {
                continue;
            }
            let sequence = u64::from_be_bytes(packet[..8].try_into().unwrap());
            let index = u16::from_be_bytes(packet[8..10].try_into().unwrap()) as usize;
            let count = u16::from_be_bytes(packet[10..12].try_into().unwrap()) as usize;
            if count == 0 || count > 64 || index >= count {
                continue;
            }
            let packet = packet.slice(12..);
            if count == 1 {
                if packet.len() > buffer.len() {
                    return Err(err("QUIC media buffer too small"));
                }
                buffer[..packet.len()].copy_from_slice(&packet);
                return Ok((packet.len(), self.peer));
            }
            receiver
                .partial
                .retain(|_, p| p.at.elapsed() < Duration::from_millis(150));
            if receiver.partial.len() >= 128 && !receiver.partial.contains_key(&sequence) {
                continue;
            }
            let partial = receiver
                .partial
                .entry(sequence)
                .or_insert_with(|| PartialPacket {
                    at: Instant::now(),
                    parts: vec![None; count],
                    bytes: 0,
                });
            if partial.parts.len() != count || partial.parts[index].is_some() {
                continue;
            }
            partial.bytes += packet.len();
            if partial.bytes > buffer.len().min(65536) {
                receiver.partial.remove(&sequence);
                continue;
            }
            partial.parts[index] = Some(packet);
            if partial.parts.iter().all(Option::is_some) {
                let partial = receiver.partial.remove(&sequence).unwrap();
                let mut offset = 0;
                for packet in partial.parts.into_iter().flatten() {
                    buffer[offset..offset + packet.len()].copy_from_slice(&packet);
                    offset += packet.len();
                }
                return Ok((offset, self.peer));
            }
        }
    }
}

/// TCP and QUIC share the existing managed-session machinery, not their sockets.
#[derive(Clone)]
pub struct Peer(Arc<Client>);
struct Client {
    endpoint: Endpoint,
    address: SocketAddr,
    media: Arc<Media>,
    dialing: tokio::sync::Mutex<()>,
}
impl Peer {
    pub async fn open_stream(&self) -> io::Result<BoxStream> {
        let _guard = self.0.dialing.lock().await;
        let old = self.0.media.connection.borrow().clone();
        let connection = if old.close_reason().is_none() {
            old
        } else {
            let connection = dial(&self.0.endpoint, self.0.address).await?;
            self.0.media.connection.send_replace(connection.clone());
            connection
        };
        let (send, recv) = connection.open_bi().await.map_err(err)?;
        Ok(Box::new(tokio::io::join(recv, send)))
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"session closed");
    }
}
async fn dial(endpoint: &Endpoint, address: SocketAddr) -> io::Result<Connection> {
    let connection = tokio::time::timeout(HANDSHAKE, endpoint.connect(address, NAME).map_err(err)?)
        .await
        .map_err(|_| err("QUIC 连接超时"))?
        .map_err(|e| err(format!("QUIC 握手失败（请检查访问密码和服务端协议）：{e}")))?;
    if connection.max_datagram_size().is_none() {
        return Err(err("QUIC 对端不支持 DATAGRAM"));
    }
    Ok(connection)
}
pub async fn connect(
    address: SocketAddr,
    password: String,
) -> io::Result<(SessionChannel, Datagram)> {
    let config = tokio::task::spawn_blocking(move || client_config(&password))
        .await
        .map_err(err)??;
    let local = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let mut endpoint = Endpoint::client(local.parse().unwrap())?;
    endpoint.set_default_client_config(config);
    let connection = dial(&endpoint, address).await?;
    let media = Media::new(connection, endpoint.local_addr()?, address);
    let peer = Peer(Arc::new(Client {
        endpoint,
        address,
        media: media.clone(),
        dialing: Default::default(),
    }));
    let channel = crate::session::connect_with(Dialer::Quic(peer)).await?;
    Ok((channel, Datagram::Quic(media)))
}

/// Native UDP compatibility side of the single physical socket.
pub struct SharedUdp {
    socket: Arc<dyn AsyncUdpSocket>,
    packets: tokio::sync::Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
}
impl SharedUdp {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
    pub async fn send(&self, packet: &[u8], target: SocketAddr) -> io::Result<usize> {
        let mut poller = self.socket.clone().create_io_poller();
        let transmit = quinn::udp::Transmit {
            destination: target,
            ecn: None,
            contents: packet,
            segment_size: None,
            src_ip: None,
        };
        loop {
            match self.socket.try_send(&transmit) {
                Ok(()) => return Ok(packet.len()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx)).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    pub async fn recv(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (packet, peer) = self
            .packets
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| err("UDP listener closed"))?;
        if packet.len() > buffer.len() {
            return Err(err("UDP buffer too small"));
        }
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok((packet.len(), peer))
    }
}

struct DemuxState {
    buffer: Vec<u8>,
    pending: VecDeque<(Vec<u8>, quinn::udp::RecvMeta)>,
}
struct DemuxSocket {
    socket: Arc<dyn AsyncUdpSocket>,
    state: Mutex<DemuxState>,
    native: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    validate: ValidateBind,
}
impl fmt::Debug for DemuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DirectQuicDemux")
    }
}
impl AsyncUdpSocket for DemuxSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        self.socket.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit<'_>) -> io::Result<()> {
        self.socket.try_send(transmit)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
    fn max_transmit_segments(&self) -> usize {
        self.socket.max_transmit_segments()
    }
    fn may_fragment(&self) -> bool {
        self.socket.may_fragment()
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.state.lock().unwrap();
        for _ in 0..64 {
            if let Some((packet, metadata)) = state.pending.pop_front() {
                if packet.len() > bufs[0].len() {
                    return Poll::Ready(Err(err("QUIC receive buffer too small")));
                }
                bufs[0][..packet.len()].copy_from_slice(&packet);
                meta[0] = metadata;
                return Poll::Ready(Ok(1));
            }
            let mut metadata = [quinn::udp::RecvMeta::default()];
            let mut buffer = [IoSliceMut::new(&mut state.buffer)];
            match self.socket.poll_recv(cx, &mut buffer, &mut metadata) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(0)) => continue,
                Poll::Ready(Ok(_)) => {}
            }
            let metadata = metadata[0];
            // Split GRO batches before classification, preserving per-packet metadata.
            let stride = metadata.stride.max(1);
            for offset in (0..metadata.len).step_by(stride) {
                let packet = state.buffer[offset..(offset + stride).min(metadata.len)].to_vec();
                let native = vnt_proto::control::decode_udp_bind_frame(&packet)
                    .is_some_and(|(id, token)| (self.validate)(id, &token, metadata.addr));
                if native {
                    let _ = self.native.try_send((packet, metadata.addr));
                } else {
                    let len = packet.len();
                    state.pending.push_back((
                        packet,
                        quinn::udp::RecvMeta {
                            len,
                            stride: len,
                            ..metadata
                        },
                    ));
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

pub struct Accepted {
    pub channel: SessionChannel,
    pub peer: SocketAddr,
    pub media: Arc<Datagram>,
}
pub struct Listener {
    endpoint: Endpoint,
    password: Password,
    cached: Arc<tokio::sync::Mutex<(String, Arc<quinn::ServerConfig>)>>,
    sessions: SessionListener,
    tasks: tokio::task::JoinSet<()>,
    sender: mpsc::Sender<Accepted>,
    accepted: mpsc::Receiver<Accepted>,
    cancel: CancellationToken,
}
impl Listener {
    pub async fn new(
        socket: tokio::net::UdpSocket,
        initial: String,
        password: Password,
        validate: ValidateBind,
    ) -> io::Result<(Self, Datagram)> {
        let value = initial.clone();
        let config = Arc::new(
            tokio::task::spawn_blocking(move || server_config(&value))
                .await
                .map_err(err)??,
        );
        for result in [
            socket2::SockRef::from(&socket).set_send_buffer_size(16 * 1024 * 1024),
            socket2::SockRef::from(&socket).set_recv_buffer_size(16 * 1024 * 1024),
        ] {
            if let Err(error) = result {
                log::warn!("direct UDP socket buffer: {error}");
            }
        }
        let raw = socket.into_std()?;
        let socket = quinn::TokioRuntime.wrap_udp_socket(raw)?;
        let (native, packets) = mpsc::channel(128);
        let shared = Datagram::Shared(SharedUdp {
            socket: socket.clone(),
            packets: tokio::sync::Mutex::new(packets),
        });
        let demux = Arc::new(DemuxSocket {
            socket,
            native,
            validate,
            state: Mutex::new(DemuxState {
                buffer: vec![0; 65536],
                pending: VecDeque::new(),
            }),
        });
        let endpoint = Endpoint::new_with_abstract_socket(
            Default::default(),
            Some((*config).clone()),
            demux,
            Arc::new(quinn::TokioRuntime),
        )?;
        let cancel = CancellationToken::new();
        let (sender, accepted) = mpsc::channel(16);
        Ok((
            Self {
                endpoint,
                password,
                cached: Arc::new(tokio::sync::Mutex::new((initial, config))),
                sessions: SessionListener::scoped(cancel.clone()),
                tasks: Default::default(),
                sender,
                accepted,
                cancel,
            },
            shared,
        ))
    }
    pub async fn accept(&mut self) -> Option<Accepted> {
        loop {
            tokio::select! {
                result = self.accepted.recv() => return result,
                Some(_) = self.tasks.join_next(), if !self.tasks.is_empty() => {},
                incoming = self.endpoint.accept() => {
                    let incoming = incoming?;
                    // Bound unauthenticated handshakes as well as established connections.
                    if self.tasks.len() >= 64 { incoming.refuse(); continue; }
                    let password = self.password.clone();
                    let cached = self.cached.clone();
                    let sessions = self.sessions.clone();
                    let sender = self.sender.clone();
                    let cancel = self.cancel.clone();
                    let local = self.endpoint.local_addr().ok()?;
                    self.tasks.spawn(async move {
                        // Keep accept cancellation-safe when the host selects TCP or shutdown.
                        // Disk reads and the KDF run off the async executor and are serialized.
                        let mut cache = cached.lock().await;
                        let configured = match tokio::task::spawn_blocking(move || password()).await {
                            Ok(Ok(value)) => value,
                            _ => { incoming.refuse(); return; }
                        };
                        if cache.0 != configured {
                            let value = configured.clone();
                            match tokio::task::spawn_blocking(move || server_config(&value)).await {
                                Ok(Ok(config)) => *cache = (configured, Arc::new(config)),
                                _ => { incoming.refuse(); return; }
                            }
                        }
                        let config = cache.1.clone();
                        drop(cache);
                        let connection = match incoming.accept_with(config) {
                            Ok(connecting) => match tokio::time::timeout(HANDSHAKE, connecting).await {
                                Ok(Ok(connection)) if connection.max_datagram_size().is_some() => connection,
                                _ => return,
                            },
                            Err(_) => return,
                        };
                        let peer = connection.remote_address();
                        let media = Media::new(connection.clone(), local, peer);
                        let sessions = sessions.with_quic(media.clone());
                        let mut streams = tokio::task::JoinSet::new();
                        loop {
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                Some(_) = streams.join_next(), if !streams.is_empty() => {},
                                stream = connection.accept_bi() => {
                                    let Ok((send, recv)) = stream else { break; };
                                    if streams.len() >= 16 { continue; }
                                    let sessions = sessions.clone();
                                    let sender = sender.clone();
                                    let media = media.clone();
                                    streams.spawn(async move {
                                        if let Ok(Some(channel)) = sessions.accept_stream(Box::new(tokio::io::join(recv, send)), peer).await {
                                            let _ = sender.send(Accepted { channel, peer, media: Arc::new(Datagram::Quic(media)) }).await;
                                        }
                                    });
                                }
                            }
                        }
                    });
                }
            }
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.endpoint.close(0u32.into(), b"host stopped");
        self.tasks.abort_all();
    }
}

#[cfg(test)]
#[path = "quic_tests.rs"]
mod tests;
