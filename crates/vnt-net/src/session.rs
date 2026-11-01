//! Managed remote sessions. Application lifetime is independent of socket lifetime.
//! Local IPC continues to use FramedChannel directly; both remote host entrypoints
//! share this listener, authentication, lane routing and recovery implementation.
use crate::{tcp::FramedChannel, traffic::TrafficCounters};
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{io::DuplexStream, net::TcpStream, sync::mpsc, task::JoinSet, time::Instant};
use tokio_util::sync::CancellationToken;
use vnt_proto::{auth, control::ControlMessage as Msg, pb};

pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncStream for T {}
pub type BoxStream = Box<dyn AsyncStream>;
type Wire = FramedChannel<BoxStream>;
pub type OpenStream = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<BoxStream>> + Send>>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub enum Dialer {
    Tcp(SocketAddr),
    Overlay(crate::overlay::Peer),
    Quic(crate::quic::Peer),
    Proxy(OpenStream),
}
impl Dialer {
    async fn connect(&self) -> io::Result<Wire> {
        match self {
            Self::Tcp(address) => {
                let stream = TcpStream::connect(address).await?;
                stream.set_nodelay(true)?;
                Ok(Wire::new(Box::new(stream)))
            }
            Self::Overlay(peer) => Ok(Wire::new(peer.open_stream().await?)),
            Self::Quic(peer) => Ok(Wire::new(peer.open_stream().await?)),
            Self::Proxy(open) => Ok(Wire::new(open().await?)),
        }
    }
}
pub type SessionChannel = FramedChannel<DuplexStream>;
const RECOVERY: Duration = Duration::from_secs(30);
const HANDSHAKE: Duration = Duration::from_secs(5);
const CONTROL: usize = 0;
const FILES: usize = 1;
const MEDIA: usize = 2;
const RESUME: i32 = pb::SessionOperation::SessionResume as i32;
const CHALLENGE: i32 = pb::SessionOperation::SessionChallenge as i32;
const PROOF: i32 = pb::SessionOperation::SessionProof as i32;
const ATTACHED: i32 = pb::SessionOperation::SessionAttached as i32;
const REQUEST: i32 = pb::SessionOperation::SessionGrantRequest as i32;
const GRANT: i32 = pb::SessionOperation::SessionGrant as i32;
const BIND: i32 = pb::SessionOperation::SessionBind as i32;
const STATUS: i32 = pb::SessionOperation::SessionStatus as i32;
const SYNC: i32 = pb::SessionOperation::SessionSync as i32;
const SYNC_ACK: i32 = pb::SessionOperation::SessionSyncAck as i32;
const SYNC_COMMIT: i32 = pb::SessionOperation::SessionSyncCommit as i32;

fn error(s: impl Into<String>) -> io::Error {
    io::Error::other(s.into())
}

async fn writable(sender: Option<mpsc::Sender<Msg>>) -> bool {
    match sender {
        Some(sender) => sender.reserve_owned().await.is_ok(),
        None => std::future::pending().await,
    }
}
fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|e| error(e.to_string()))?;
    Ok(bytes)
}
fn packet(op: i32, kind: usize, session_id: u32) -> pb::SessionPacket {
    pb::SessionPacket {
        op,
        kind: kind as i32,
        session_id,
        ..Default::default()
    }
}
async fn receive<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    channel: &mut FramedChannel<S>,
) -> io::Result<Msg> {
    channel
        .recv()
        .await?
        .ok_or_else(|| error("connection closed"))
}
fn lane(msg: &Msg, tcp_media: bool) -> usize {
    vnt_proto::session::channel_for(
        msg,
        if tcp_media {
            vnt_proto::Transport::Tcp
        } else {
            vnt_proto::Transport::Hybrid
        },
    ) as usize
}
fn allowed(msg: &Msg, kind: usize, tcp_media: bool) -> bool {
    pb::ChannelKind::try_from(kind as i32).is_ok_and(|channel| {
        vnt_proto::session::allowed_on(
            msg,
            channel,
            if tcp_media {
                vnt_proto::Transport::Tcp
            } else {
                vnt_proto::Transport::Hybrid
            },
        )
    })
}

struct Delivery {
    generation: Option<u64>,
    msg: Msg,
}
pub(crate) struct SessionReceiver {
    lanes: [mpsc::Receiver<Delivery>; 3],
    generations: [Arc<AtomicU64>; 3],
}
impl SessionReceiver {
    pub(crate) fn into_lanes(self) -> [MessageLane; 3] {
        let [control, files, media] = self.lanes;
        let [cg, fg, mg] = self.generations;
        [
            MessageLane {
                receiver: control,
                generation: cg,
            },
            MessageLane {
                receiver: files,
                generation: fg,
            },
            MessageLane {
                receiver: media,
                generation: mg,
            },
        ]
    }
    pub async fn recv(&mut self) -> Option<Msg> {
        let [control, files, media] = &mut self.lanes;
        loop {
            let (kind, delivery) = tokio::select! {
                biased;
                message=control.recv() => (CONTROL, message),
                message=media.recv() => (MEDIA, message),
                message=files.recv() => (FILES, message),
            };
            let delivery = delivery?;
            if delivery
                .generation
                .is_none_or(|g| self.generations[kind].load(Ordering::Acquire) == g)
            {
                return Some(delivery.msg);
            }
        }
    }
}

/// Independently drained lanes for an in-process screen worker. Keeping the
/// queues separate prevents a blocked video sink from parking input or files.
pub struct MessageLane {
    receiver: mpsc::Receiver<Delivery>,
    generation: Arc<AtomicU64>,
}
impl MessageLane {
    pub async fn recv(&mut self) -> Option<Msg> {
        loop {
            let delivery = self.receiver.recv().await?;
            if delivery
                .generation
                .is_none_or(|g| self.generation.load(Ordering::Acquire) == g)
            {
                return Some(delivery.msg);
            }
        }
    }
}

pub(crate) struct Endpoint {
    pub incoming: SessionReceiver,
    pub outgoing: Outgoing,
    pub traffic: TrafficCounters,
}
impl Endpoint {
    pub async fn send(&self, msg: Msg) -> io::Result<()> {
        self.outgoing.send(msg).await
    }
}
#[derive(Clone)]
pub(crate) struct Outgoing {
    local: Option<([mpsc::Sender<Delivery>; 3], TrafficCounters)>,
    lanes: [mpsc::Sender<Msg>; 3],
    tcp_media: Arc<AtomicBool>,
    events: mpsc::Sender<Event>,
}
impl Outgoing {
    pub async fn send(&self, msg: Msg) -> io::Result<()> {
        if let Some((lanes, traffic)) = &self.local {
            let kind = lane(&msg, true);
            if let Msg::VideoFrame(frame) = &msg {
                traffic.video_sent(frame.data.len());
            }
            return lanes[kind]
                .send(Delivery {
                    generation: None,
                    msg,
                })
                .await
                .map_err(|_| error("local session ended"));
        }
        if let Msg::Hello(h) = &msg {
            self.tcp_media.store(h.transport == 2, Ordering::Release);
        }
        if vnt_proto::screen::is_video(&msg) {
            // Preserve encoded references. This producer owns only the media
            // lane; control/file producers remain independent while it waits.
            return self.lanes[MEDIA]
                .send(msg)
                .await
                .map_err(|_| error("session ended"));
        }
        if matches!(msg, Msg::AudioFrame(_)) {
            return match self.lanes[MEDIA].try_send(msg) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
                Err(_) => Err(error("session ended")),
            };
        }
        // Configuration enters the supervisor through its priority inbox, then
        // joins the media writer. A blocked media socket cannot block control producers.
        let priority = vnt_proto::screen::is_config(&msg) || matches!(&msg, Msg::AudioState(_));
        let destination = if priority {
            CONTROL
        } else {
            lane(&msg, self.tcp_media.load(Ordering::Acquire))
        };
        if destination == CONTROL {
            return match self.lanes[CONTROL].try_send(msg) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(msg)) => self
                    .events
                    .send(Event::ControlPressure(msg))
                    .await
                    .map_err(|_| error("session ended")),
                Err(_) => Err(error("session ended")),
            };
        }
        self.lanes[destination]
            .send(msg)
            .await
            .map_err(|_| error("session ended"))
    }
}

/// In-process workers retain independent bounded control, file and media lanes.
/// No network authentication, recovery task or byte-stream copy is introduced.
pub fn local_pair() -> (SessionChannel, SessionChannel) {
    fn lanes() -> ([mpsc::Sender<Delivery>; 3], [mpsc::Receiver<Delivery>; 3]) {
        let (c, cr) = mpsc::channel(64);
        let (f, fr) = mpsc::channel(8);
        let (m, mr) = mpsc::channel(2);
        ([c, f, m], [cr, fr, mr])
    }
    fn endpoint(
        incoming: [mpsc::Receiver<Delivery>; 3],
        outgoing: [mpsc::Sender<Delivery>; 3],
    ) -> Endpoint {
        let (c, _) = mpsc::channel(1);
        let (f, _) = mpsc::channel(1);
        let (m, _) = mpsc::channel(1);
        let (events, _) = mpsc::channel(1);
        let traffic = TrafficCounters::default();
        Endpoint {
            incoming: SessionReceiver {
                lanes: incoming,
                generations: std::array::from_fn(|_| Arc::new(AtomicU64::new(0))),
            },
            outgoing: Outgoing {
                local: Some((outgoing, traffic.clone())),
                lanes: [c, f, m],
                tcp_media: Arc::new(AtomicBool::new(true)),
                events,
            },
            traffic,
        }
    }
    let (a, ar) = lanes();
    let (b, br) = lanes();
    (
        SessionChannel::managed(endpoint(ar, b)),
        SessionChannel::managed(endpoint(br, a)),
    )
}

struct Grant {
    token: [u8; 32],
    generation: u64,
    expires: Instant,
}
struct Record {
    secret: [u8; 32],
    ip: IpAddr,
    sender: mpsc::Sender<Event>,
    deadline: Mutex<Option<Instant>>,
    grants: Mutex<HashMap<usize, Grant>>,
    closed: AtomicBool,
    quic: Option<Arc<crate::quic::Media>>,
}
#[derive(Clone, Default)]
pub struct SessionListener {
    cancellation: Option<CancellationToken>,
    quic: Option<Arc<crate::quic::Media>>,
    quic_initial: Option<Arc<AtomicBool>>,
    records: Arc<Mutex<HashMap<u32, Arc<Record>>>>,
}
impl SessionListener {
    pub fn scoped(cancellation: CancellationToken) -> Self {
        Self {
            records: Default::default(),
            cancellation: Some(cancellation),
            quic: None,
            quic_initial: None,
        }
    }
    pub(crate) fn with_quic(&self, media: Arc<crate::quic::Media>) -> Self {
        Self {
            quic: Some(media),
            quic_initial: Some(Arc::new(AtomicBool::new(false))),
            ..self.clone()
        }
    }
    /// Dispatch a remote socket. Only first-time Hello returns an application session.
    pub async fn accept(
        &self,
        stream: TcpStream,
        peer: SocketAddr,
    ) -> io::Result<Option<SessionChannel>> {
        stream.set_nodelay(true)?;
        self.accept_stream(Box::new(stream), peer).await
    }
    pub async fn accept_stream(
        &self,
        stream: BoxStream,
        peer: SocketAddr,
    ) -> io::Result<Option<SessionChannel>> {
        let mut wire = Wire::new(stream);
        let first = tokio::time::timeout(HANDSHAKE, receive(&mut wire))
            .await
            .map_err(|_| error("handshake timeout"))??;
        if let Msg::Hello(ref hello) = first {
            if (hello.transport == 4) != self.quic.is_some() {
                return Err(error("QUIC transport does not match physical connection"));
            }
            if self
                .quic_initial
                .as_ref()
                .is_some_and(|claimed| claimed.swap(true, Ordering::AcqRel))
            {
                return Err(error("QUIC connection already has a session"));
            }
            let endpoint = start(
                wire,
                Some((self.clone(), peer.ip())),
                None,
                hello.transport == 2,
                Some(first),
            );
            return Ok(Some(SessionChannel::managed(endpoint)));
        }
        let Msg::SessionPacket(request) = first else {
            return Err(error("expected Hello or session attachment"));
        };
        let kind = request.kind as usize;
        if kind > MEDIA
            || !matches!(request.op, RESUME | BIND)
            || (request.op == RESUME) != (kind == CONTROL)
        {
            return Err(error("invalid attachment role"));
        }
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&request.session_id)
            .cloned();
        let Some(record) = record else {
            wire.send(&Msg::Denied(pb::Denied {
                reason: "会话不存在或已过期，请重新连接".into(),
            }))
            .await?;
            return Ok(None);
        };
        if record.quic.is_some() != self.quic.is_some()
            || record.closed.load(Ordering::Acquire)
            || record.ip != peer.ip()
            || record
                .deadline
                .lock()
                .unwrap()
                .is_some_and(|d| Instant::now() >= d)
        {
            return Err(error("session unavailable"));
        }
        if kind == CONTROL {
            if request.attempt.len() != 16 {
                return Err(error("invalid recovery attempt"));
            }
            let nonce = random::<32>()?;
            let mut challenge = packet(CHALLENGE, CONTROL, request.session_id);
            challenge.nonce = nonce.to_vec();
            challenge.attempt = request.attempt.clone();
            let result = tokio::time::timeout(HANDSHAKE, async {
                wire.send(&Msg::SessionPacket(challenge)).await?;
                let Msg::SessionPacket(proof) = receive(&mut wire).await? else {
                    return Err(error("expected recovery proof"));
                };
                if proof.op != PROOF
                    || proof.kind != 0
                    || proof.session_id != request.session_id
                    || proof.attempt != request.attempt
                    || !auth::verify_session(
                        &record.secret,
                        request.session_id,
                        &request.attempt,
                        &nonce,
                        &proof.credential,
                    )
                {
                    return Err(error("invalid recovery proof"));
                }
                Ok(())
            })
            .await
            .map_err(|_| error("recovery proof timeout"))?;
            result?;
        } else {
            let mut grants = record.grants.lock().unwrap();
            let grant = grants
                .get(&kind)
                .ok_or_else(|| error("missing binding grant"))?;
            if grant.expires <= Instant::now()
                || grant.generation != request.generation
                || !auth::equal_token(&grant.token, &request.credential)
            {
                return Err(error("invalid binding grant"));
            }
            grants.remove(&kind);
        }
        record
            .sender
            .send(Event::Attach {
                kind,
                generation: request.generation,
                quic: self.quic.clone(),
                wire,
            })
            .await
            .map_err(|_| error("session ended"))?;
        Ok(None)
    }
}

pub async fn connect(address: SocketAddr) -> io::Result<SessionChannel> {
    connect_with(Dialer::Tcp(address)).await
}
pub async fn connect_with(dialer: Dialer) -> io::Result<SessionChannel> {
    let wire = dialer.connect().await?;
    Ok(SessionChannel::managed(start(
        wire,
        None,
        Some(dialer),
        false,
        None,
    )))
}

enum Event {
    ControlPressure(Msg),
    Answered(u32),
    Incoming {
        kind: usize,
        generation: u64,
        msg: Msg,
    },
    Down {
        kind: usize,
        generation: u64,
    },
    Attach {
        kind: usize,
        generation: u64,
        quic: Option<Arc<crate::quic::Media>>,
        wire: Wire,
    },
    RetryFailed {
        kind: usize,
        terminal: bool,
    },
}
struct Link {
    tx: mpsc::Sender<Msg>,
    cancel: CancellationToken,
    generation: u64,
}
struct Supervisor {
    server: Option<(SessionListener, IpAddr)>,
    address: Option<Dialer>,
    id: u32,
    secret: [u8; 32],
    record: Option<Arc<Record>>,
    tcp_media: Arc<AtomicBool>,
    traffic: TrafficCounters,
    events: mpsc::Sender<Event>,
    app: [mpsc::Sender<Delivery>; 3],
    delivered_generations: [Arc<AtomicU64>; 3],
    links: [Option<Link>; 3],
    generations: [u64; 3],
    lost: [Option<Instant>; 3],
    next_retry: [Instant; 3],
    attempts: [usize; 3],
    retrying: [bool; 3],
    pending: [Option<Msg>; 3],
    requests: VecDeque<Msg>,
    awaiting: HashMap<u32, Msg>,
    snapshots: HashMap<u8, Msg>,
    snapshot_revision: u64,
    sync_revision: u64,
    ready_image: Option<(u32, u64)>,
    media_config: VecDeque<Msg>,
    screen_snapshots: std::collections::BTreeMap<(u32, u8), Msg>,
    screen_waiting: std::collections::HashSet<u32>,
    open_screens: Option<std::collections::HashSet<u32>>,
    media_needs_keyframe: bool,
    tasks: JoinSet<()>,
    last_control_rx: Instant,
    last_heartbeat: Instant,
    last_app_status: Instant,
}
fn start(
    wire: Wire,
    server: Option<(SessionListener, IpAddr)>,
    address: Option<Dialer>,
    tcp: bool,
    first: Option<Msg>,
) -> Endpoint {
    let (control_tx, control_rx) = mpsc::channel(128);
    let (file_tx, file_rx) = mpsc::channel(8);
    let (media_tx, media_rx) = mpsc::channel(4);
    let delivered_generations: [Arc<AtomicU64>; 3] =
        std::array::from_fn(|_| Arc::new(AtomicU64::new(0)));
    let incoming = SessionReceiver {
        lanes: [control_rx, file_rx, media_rx],
        generations: delivered_generations.clone(),
    };
    let app = [control_tx, file_tx, media_tx];
    let capacities = [128, 8, 4];
    let queues: Vec<_> = capacities.into_iter().map(mpsc::channel).collect();
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for (tx, rx) in queues {
        senders.push(tx);
        receivers.push(rx);
    }
    let tcp_media = Arc::new(AtomicBool::new(tcp));
    let (events, rx) = mpsc::channel(64);
    let outgoing = Outgoing {
        local: None,
        lanes: senders.try_into().ok().unwrap(),
        tcp_media: tcp_media.clone(),
        events: events.clone(),
    };
    let traffic = TrafficCounters::default();
    let now = Instant::now();
    let mut supervisor = Supervisor {
        server,
        address,
        id: 0,
        secret: [0; 32],
        record: None,
        tcp_media,
        traffic: traffic.clone(),
        events,
        app,
        delivered_generations,
        links: [None, None, None],
        generations: [0; 3],
        lost: [None; 3],
        next_retry: [now; 3],
        attempts: [0; 3],
        retrying: [false; 3],
        pending: [None, None, None],
        requests: VecDeque::new(),
        awaiting: HashMap::new(),
        snapshots: HashMap::new(),
        snapshot_revision: 0,
        sync_revision: 0,
        ready_image: None,
        media_config: VecDeque::new(),
        screen_snapshots: Default::default(),
        screen_waiting: Default::default(),
        open_screens: None,
        media_needs_keyframe: true,
        tasks: JoinSet::new(),
        last_control_rx: now,
        last_heartbeat: now,
        last_app_status: now,
    };
    supervisor.install(CONTROL, 0, wire);
    if let Some(msg) = first {
        let _ = supervisor.deliver(msg);
    }
    tokio::spawn(async move {
        supervisor.run(rx, receivers.try_into().ok().unwrap()).await;
    });
    Endpoint {
        incoming,
        outgoing,
        traffic,
    }
}
impl Supervisor {
    fn install(&mut self, kind: usize, generation: u64, mut wire: Wire) {
        if let Some(old) = self.links[kind].take() {
            old.cancel.cancel();
        }
        self.generations[kind] = generation;
        self.delivered_generations[kind].store(generation, Ordering::Release);
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel::<Msg>(if kind == MEDIA { 4 } else { 8 });
        wire.use_traffic(self.traffic.clone());
        let (mut reader, mut writer) = wire.into_split();
        let events = self.events.clone();
        let stop = cancel.clone();
        let application = self.app[kind].clone();
        let tcp_media = self.tcp_media.load(Ordering::Acquire);
        self.tasks.spawn(async move {
            loop {
                let result = tokio::select! { _=stop.cancelled()=>return, result=reader.recv()=>result };
                match result {
                    Ok(Some(msg)) => {
                        if kind == CONTROL {
                            if events.send(Event::Incoming { kind, generation, msg }).await.is_err() { return; }
                        } else {
                            if !allowed(&msg, kind, tcp_media) { break; }
                            if let Msg::ClipboardReply(reply) = &msg {
                                if events.send(Event::Answered(reply.request_id)).await.is_err() { return; }
                            }
                            let delivery = Delivery { generation: Some(generation), msg };
                            tokio::select! { _=stop.cancelled()=>return, result=application.send(delivery)=>if result.is_err() {return;} }
                        }
                    }
                    _ => break,
                }
            }
            let _ = events.send(Event::Down { kind, generation }).await;
        });
        let events = self.events.clone();
        let stop = cancel.clone();
        self.tasks.spawn(async move {
            loop {
                let msg = tokio::select! { _=stop.cancelled()=>return, msg=rx.recv()=>match msg { Some(m)=>m, None=>return } };
                let result = tokio::select! { _=stop.cancelled()=>return, result=tokio::time::timeout(Duration::from_secs(5), writer.send(&msg))=>result };
                if !matches!(result, Ok(Ok(()))) { break; }
            }
            let _ = events.send(Event::Down { kind, generation }).await;
        });
        self.links[kind] = Some(Link {
            tx,
            cancel,
            generation,
        });
        self.lost[kind] = None;
        self.retrying[kind] = false;
        self.attempts[kind] = 0;
    }
    fn deliver(&self, msg: Msg) -> io::Result<()> {
        self.app[CONTROL]
            .try_send(Delivery {
                generation: None,
                msg,
            })
            .map_err(|_| error("application control queue congested"))
    }
    fn notify(&self, kind: usize, available: bool) -> io::Result<()> {
        self.deliver(Msg::TransportState(pb::TransportState {
            kind: kind as i32,
            available,
            generation: self.generations[kind],
            state_revision: self.sync_revision,
            synchronizing: false,
        }))
    }
    fn down(&mut self, kind: usize) -> io::Result<()> {
        let Some(link) = self.links[kind].take() else {
            return Ok(());
        };
        link.cancel.cancel();
        self.lost[kind].get_or_insert(Instant::now());
        self.next_retry[kind] = Instant::now();
        self.retrying[kind] = false;
        self.pending[kind] = None;
        if kind == CONTROL {
            if let Some(r) = &self.record {
                *r.deadline.lock().unwrap() = self.lost[CONTROL].map(|at| at + RECOVERY);
                r.grants.lock().unwrap().clear();
            }
        } else {
            if kind == FILES {
                self.ready_image = None;
                self.awaiting.clear();
                self.requests.clear();
            }
            if self.server.is_some() && self.links[CONTROL].is_some() {
                let mut p = packet(STATUS, kind, self.id);
                p.generation = self.generations[kind];
                self.control(Msg::SessionPacket(p))?;
            }
        }
        self.notify(kind, false)
    }
    fn control(&mut self, msg: Msg) -> io::Result<()> {
        let Some(link) = &self.links[CONTROL] else {
            return Ok(());
        };
        if link.tx.try_send(msg).is_err() {
            self.down(CONTROL)?;
        }
        Ok(())
    }
    fn snapshot(&mut self, msg: &Msg) {
        if let Msg::ScreensState(state) = msg {
            let open: std::collections::HashSet<_> =
                state.screens.iter().map(|s| s.stream_id).collect();
            self.screen_snapshots.retain(|(id, _), _| open.contains(id));
            self.screen_waiting.retain(|id| open.contains(id));
            self.media_config
                .retain(|m| !matches!(m,Msg::ScreenPacket(p) if !open.contains(&p.stream_id)));
            self.open_screens = Some(open);
        }
        if let Msg::ScreenPacket(packet) = msg {
            let kind = match vnt_proto::screen::unwrap(packet) {
                Some(Msg::StreamReset(_)) => 2,
                Some(Msg::DesktopState(_)) => 1,
                _ => return,
            };
            let key = (packet.stream_id, kind);
            if self.screen_snapshots.get(&key) != Some(msg) {
                self.screen_snapshots.insert(key, msg.clone());
                self.snapshot_revision = self.snapshot_revision.saturating_add(1);
            }
            return;
        }
        let key = match msg {
            Msg::ScreensState(_) => 5,
            Msg::DesktopState(_) => 1,
            Msg::StreamReset(_) => 2,
            Msg::AudioState(_) => 3,
            Msg::ClipboardOffer(_) | Msg::ClipboardText(_) | Msg::ClipboardImage(_) => 4,
            _ => return,
        };
        if self.snapshots.get(&key) != Some(msg) {
            self.snapshots.insert(key, msg.clone());
            self.snapshot_revision = self.snapshot_revision.saturating_add(1);
        }
    }
    fn replay(&mut self, kind: usize) -> io::Result<()> {
        let keys: &[u8] = if kind == CONTROL {
            &[1, 4, 5]
        } else if kind == MEDIA {
            &[2, 3]
        } else {
            &[4]
        };
        for key in keys {
            if let Some(msg) = self.snapshots.get(key).cloned() {
                let target = lane(&msg, self.tcp_media.load(Ordering::Acquire));
                if let Some(link) = &self.links[target] {
                    link.tx
                        .try_send(msg)
                        .map_err(|_| error("snapshot queue congested"))?;
                }
            }
        }
        let tcp = self.tcp_media.load(Ordering::Acquire);
        for msg in self.screen_snapshots.values().cloned().collect::<Vec<_>>() {
            let target = lane(&msg, tcp);
            if target == kind || (kind == CONTROL && !tcp) {
                if let Some(link) = &self.links[target] {
                    link.tx
                        .try_send(msg)
                        .map_err(|_| error("screen snapshot queue congested"))?;
                }
            }
        }
        if kind == CONTROL && !self.tcp_media.load(Ordering::Acquire) {
            for key in [2, 3] {
                if let Some(msg) = self.snapshots.get(&key).cloned() {
                    self.control(msg)?;
                }
            }
        }
        Ok(())
    }
    async fn outgoing(&mut self, mut msg: Msg, kind: usize) -> io::Result<bool> {
        if let Msg::SessionPacket(p) = &msg {
            if p.op == SYNC_ACK
                && self.address.is_some()
                && p.session_id == self.id
                && p.generation == self.generations[CONTROL]
            {
                if self.links[CONTROL].is_some() {
                    self.control(msg)?;
                }
                return Ok(true);
            }
            return Err(error("application attempted transport protocol message"));
        }
        if let Msg::ScreenPacket(packet) = &msg {
            if self
                .open_screens
                .as_ref()
                .is_some_and(|open| !open.contains(&packet.stream_id))
            {
                return Ok(true);
            }
            if vnt_proto::screen::is_config(&msg) {
                self.snapshot(&msg);
                self.screen_waiting.insert(packet.stream_id);
                if self.tcp_media.load(Ordering::Acquire) {
                    self.media_config.retain(
                        |old| !matches!(old,Msg::ScreenPacket(p) if p.stream_id==packet.stream_id),
                    );
                    self.media_config.push_back(msg);
                    return Ok(true);
                }
            } else if let Some(Msg::VideoFrame(frame)) = vnt_proto::screen::unwrap(packet) {
                if self.screen_waiting.contains(&packet.stream_id) && !frame.keyframe {
                    return Ok(true);
                }
                if frame.keyframe {
                    self.screen_waiting.remove(&packet.stream_id);
                }
            }
        }
        if self.tcp_media.load(Ordering::Acquire)
            && matches!(&msg, Msg::StreamReset(_) | Msg::AudioState(_))
        {
            self.snapshot(&msg);
            if matches!(&msg, Msg::StreamReset(_)) {
                self.media_needs_keyframe = true;
                self.pending[MEDIA] = None;
                self.deliver(Msg::RequestKeyframe)?;
            }
            self.media_config
                .retain(|previous| previous.msg_type() != msg.msg_type());
            self.media_config.push_back(msg);
            return Ok(true);
        }
        if let Msg::VideoFrame(frame) = &msg {
            if self.media_needs_keyframe && !frame.keyframe {
                return Ok(true);
            }
            if frame.keyframe {
                self.media_needs_keyframe = false;
            }
        }
        if let Msg::Welcome(w) = &mut msg {
            if let Some((listener, ip)) = &self.server {
                self.secret = random()?;
                self.id = w.session_id;
                w.session_token = self.secret.to_vec();
                let record = Arc::new(Record {
                    secret: self.secret,
                    ip: *ip,
                    sender: self.events.clone(),
                    deadline: Mutex::new(None),
                    grants: Mutex::new(HashMap::new()),
                    closed: AtomicBool::new(false),
                    quic: listener.quic.clone(),
                });
                if listener
                    .records
                    .lock()
                    .unwrap()
                    .insert(self.id, record.clone())
                    .is_some()
                {
                    return Err(error("duplicate session id"));
                }
                self.record = Some(record);
                self.lost[FILES] = Some(Instant::now());
                if self.tcp_media.load(Ordering::Acquire) {
                    self.lost[MEDIA] = Some(Instant::now());
                }
            }
        }
        if is_paste(&msg) && (self.lost[CONTROL].is_some() || self.links[CONTROL].is_none()) {
            return Ok(true); // A paste must never wait for channel recovery.
        }
        if matches!(msg, Msg::TransportState(_) | Msg::ResetInput(_)) {
            return Err(error("local event sent to remote"));
        }
        if let Msg::ClipboardImageReady(ready) = &mut msg {
            ready.generation = self.generations[FILES];
        }
        if let Msg::ClipboardImage(header) = &msg {
            if matches!(&self.pending[FILES], Some(Msg::ClipboardImageChunk(chunk)) if chunk.image_id != header.image_id)
            {
                self.pending[FILES] = None;
            }
        }
        if let Msg::ClipboardRequest(request) = &msg {
            if request.request_id != 0 {
                if self.awaiting.len() >= 128 && !self.awaiting.contains_key(&request.request_id) {
                    return Err(error("too many pending file operations"));
                }
                self.awaiting.insert(request.request_id, msg.clone());
            }
        }
        self.snapshot(&msg);
        if kind == CONTROL && self.lost[CONTROL].is_some() {
            if matches!(&msg, Msg::ClipboardRequest(_)) {
                if self.requests.len() >= 128 {
                    return Err(error("too many suspended file requests"));
                }
                self.requests.push_back(msg);
            }
            return Ok(true);
        }
        if self.links[kind].is_none() {
            if kind == FILES && self.generations[FILES] == 0 {
                self.pending[kind] = Some(msg);
                return Ok(true);
            }
            if kind == CONTROL && matches!(msg, Msg::ClipboardRequest(_)) {
                if self.requests.len() >= 128 {
                    return Err(error("too many suspended file requests"));
                }
                self.requests.push_back(msg);
            }
            return Ok(true);
        }
        if let Msg::ClipboardImageChunk(chunk) = &msg {
            if !matches!(self.snapshots.get(&4), Some(Msg::ClipboardImage(header)) if header.image_id == chunk.image_id)
            {
                return Ok(true);
            }
            if self.ready_image != Some((chunk.image_id, self.generations[FILES])) {
                self.pending[kind] = Some(msg);
                return Ok(true);
            }
        }
        if let Msg::ClipboardImageCancel(cancel) = &msg {
            if self.ready_image.is_some_and(|r| r.0 == cancel.image_id) {
                self.ready_image = None;
            }
            if matches!(&self.pending[FILES], Some(Msg::ClipboardImageChunk(c)) if c.image_id == cancel.image_id)
            {
                self.pending[FILES] = None;
            }
        }
        let ending = matches!(msg, Msg::SessionEnd(_) | Msg::Denied(_));
        match self.links[kind].as_ref().unwrap().tx.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(msg)) => self.pending[kind] = Some(msg),
            Err(mpsc::error::TrySendError::Closed(_)) => self.down(kind)?,
        }
        if ending {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Ok(false);
        }
        Ok(true)
    }
    async fn incoming(&mut self, kind: usize, msg: Msg) -> io::Result<()> {
        if !allowed(&msg, kind, self.tcp_media.load(Ordering::Acquire)) {
            return Err(error("message on wrong channel"));
        }
        if self.id != 0
            && matches!(
                &msg,
                Msg::Hello(_) | Msg::Welcome(_) | Msg::AuthChallenge(_) | Msg::AuthResponse(_)
            )
        {
            return Err(error("handshake message after authentication"));
        }
        if kind == CONTROL {
            self.last_control_rx = Instant::now();
        }
        if self.lost[CONTROL].is_some() && (matches!(&msg, Msg::InputEvent(_)) || is_paste(&msg)) {
            return Ok(());
        }
        if let Msg::SessionPacket(p) = msg {
            return self.protocol(p).await;
        }
        if let Msg::ClipboardReply(reply) = &msg {
            self.awaiting.remove(&reply.request_id);
            self.requests.retain(
                |m| !matches!(m, Msg::ClipboardRequest(r) if r.request_id == reply.request_id),
            );
        }
        if let Msg::ClipboardImageReady(ready) = &msg {
            if ready.generation == self.generations[FILES] {
                self.ready_image = Some((ready.image_id, ready.generation));
            }
            return Ok(());
        }
        if matches!(
            &msg,
            Msg::ClipboardText(_) | Msg::ClipboardOffer(_) | Msg::ClipboardImage(_)
        ) {
            self.snapshots.remove(&4);
        }
        if let Msg::Welcome(w) = &msg {
            if self.address.is_some() {
                if w.session_token.len() != 32 {
                    return Err(error("missing session recovery token"));
                }
                self.id = w.session_id;
                self.secret.copy_from_slice(&w.session_token);
                self.lost[FILES] = Some(Instant::now());
                if self.tcp_media.load(Ordering::Acquire) {
                    self.lost[MEDIA] = Some(Instant::now());
                }
            }
        }
        if matches!(&msg, Msg::InputEvent(_)) || is_paste(&msg) {
            return self.app[CONTROL]
                .try_send(Delivery {
                    generation: Some(self.generations[CONTROL]),
                    msg,
                })
                .map_err(|_| error("application input queue congested"));
        }
        self.deliver(msg)
    }
    async fn protocol(&mut self, p: pb::SessionPacket) -> io::Result<()> {
        let kind = p.kind as usize;
        if p.session_id != self.id || kind > MEDIA {
            return Err(error("invalid session packet"));
        }
        match p.op {
            REQUEST if self.server.is_some() && kind != CONTROL => {
                if kind == MEDIA && !self.tcp_media.load(Ordering::Acquire) {
                    return Err(error("media TCP disabled"));
                }
                let token = random::<32>()?;
                let generation = self.generations[kind] + 1;
                self.record.as_ref().unwrap().grants.lock().unwrap().insert(
                    kind,
                    Grant {
                        token,
                        generation,
                        expires: Instant::now() + Duration::from_secs(10),
                    },
                );
                let mut grant = packet(GRANT, kind, self.id);
                grant.generation = generation;
                grant.credential = token.to_vec();
                self.control(Msg::SessionPacket(grant))?;
            }
            GRANT if self.address.is_some() && kind != CONTROL => {
                if p.credential.len() != 32 || !self.retrying[kind] {
                    return Err(error("unexpected binding grant"));
                }
                let address = self.address.clone().unwrap();
                let events = self.events.clone();
                self.tasks.spawn(async move {
                    let result = tokio::time::timeout(HANDSHAKE, async {
                        let mut wire = address.connect().await?;
                        let mut bind = p.clone();
                        bind.op = BIND;
                        wire.send(&Msg::SessionPacket(bind)).await?;
                        match receive(&mut wire).await? {
                            Msg::SessionPacket(ack)
                                if ack.op == ATTACHED
                                    && ack.kind == p.kind
                                    && ack.session_id == p.session_id
                                    && ack.generation == p.generation =>
                            {
                                Ok(wire)
                            }
                            _ => Err(error("invalid binding acknowledgement")),
                        }
                    })
                    .await;
                    let event = match result {
                        Ok(Ok(wire)) => Event::Attach {
                            kind,
                            generation: p.generation,
                            quic: None,
                            wire,
                        },
                        _ => Event::RetryFailed {
                            kind,
                            terminal: false,
                        },
                    };
                    let _ = events.send(event).await;
                });
            }
            SYNC if self.address.is_some()
                && kind == CONTROL
                && p.generation == self.generations[CONTROL] =>
            {
                self.sync_revision = p.state_revision;
                self.replay(CONTROL)?;
                self.deliver(Msg::TransportState(pb::TransportState {
                    kind: 0,
                    available: false,
                    generation: self.generations[CONTROL],
                    state_revision: p.state_revision,
                    synchronizing: true,
                }))?;
            }
            SYNC_ACK
                if self.server.is_some()
                    && kind == CONTROL
                    && p.generation == self.generations[CONTROL] =>
            {
                if self.lost[CONTROL].is_some() && p.state_revision == self.sync_revision {
                    if self.snapshot_revision != self.sync_revision {
                        self.synchronize()?;
                    } else {
                        let mut commit = packet(SYNC_COMMIT, CONTROL, self.id);
                        commit.generation = self.generations[CONTROL];
                        commit.state_revision = self.sync_revision;
                        self.control(Msg::SessionPacket(commit))?;
                        if self.links[CONTROL].is_some() {
                            self.recovered()?;
                            self.notify(CONTROL, true)?;
                        }
                    }
                }
            }
            SYNC_COMMIT
                if self.address.is_some()
                    && kind == CONTROL
                    && p.generation == self.generations[CONTROL]
                    && p.state_revision == self.sync_revision =>
            {
                self.recovered()?;
                self.notify(CONTROL, true)?;
            }
            STATUS if self.address.is_some() && kind != CONTROL => {
                if p.generation == self.generations[kind] {
                    self.down(kind)?;
                }
            }
            STATUS if kind == CONTROL => {} // transport heartbeat
            _ => return Err(error("unexpected session packet")),
        }
        Ok(())
    }
    fn retry_control(&mut self) {
        let address = self.address.clone().unwrap();
        let id = self.id;
        let secret = self.secret;
        let events = self.events.clone();
        self.retrying[CONTROL] = true;
        self.tasks.spawn(async move {
            let result = tokio::time::timeout(HANDSHAKE, async {
                let mut wire = address.connect().await?;
                let attempt = random::<16>()?;
                let mut request = packet(RESUME, CONTROL, id);
                request.attempt = attempt.to_vec();
                wire.send(&Msg::SessionPacket(request)).await?;
                let challenge = match receive(&mut wire).await? {
                    Msg::Denied(_) => return Ok(None),
                    Msg::SessionPacket(p)
                        if p.op == CHALLENGE
                            && p.session_id == id
                            && p.attempt == attempt
                            && p.nonce.len() == 32 =>
                    {
                        p
                    }
                    _ => return Err(error("invalid recovery challenge")),
                };
                let mut proof = packet(PROOF, CONTROL, id);
                proof.attempt = attempt.to_vec();
                proof.credential = auth::session_response(&secret, id, &attempt, &challenge.nonce);
                wire.send(&Msg::SessionPacket(proof)).await?;
                match receive(&mut wire).await? {
                    Msg::SessionPacket(ack)
                        if ack.op == ATTACHED && ack.kind == 0 && ack.session_id == id =>
                    {
                        Ok(Some((wire, ack.generation)))
                    }
                    Msg::Denied(_) => Ok(None),
                    _ => Err(error("invalid resume acknowledgement")),
                }
            })
            .await;
            let event = match result {
                Ok(Ok(Some((wire, generation)))) => Event::Attach {
                    kind: CONTROL,
                    generation,
                    quic: None,
                    wire,
                },
                Ok(Ok(None)) => Event::RetryFailed {
                    kind: CONTROL,
                    terminal: true,
                },
                _ => Event::RetryFailed {
                    kind: CONTROL,
                    terminal: false,
                },
            };
            let _ = events.send(event).await;
        });
    }
    async fn attached(
        &mut self,
        kind: usize,
        mut generation: u64,
        mut wire: Wire,
        quic: Option<Arc<crate::quic::Media>>,
    ) -> io::Result<()> {
        if let Some(record) = &self.record {
            if record
                .deadline
                .lock()
                .unwrap()
                .is_some_and(|d| Instant::now() >= d)
            {
                return Err(error("session recovery expired"));
            }
        }
        if self.server.is_some() {
            if kind != CONTROL && (self.links[CONTROL].is_none() || self.lost[CONTROL].is_some()) {
                return Ok(());
            }
            if kind == CONTROL {
                generation = self.generations[CONTROL] + 1;
            } else if generation <= self.generations[kind] {
                return Ok(());
            }
            let mut ack = packet(ATTACHED, kind, self.id);
            ack.generation = generation;
            tokio::time::timeout(HANDSHAKE, wire.send(&Msg::SessionPacket(ack)))
                .await
                .map_err(|_| error("attachment ack timeout"))??;
        } else if generation <= self.generations[kind] {
            return Ok(());
        }
        if kind == CONTROL && self.links[CONTROL].is_some() {
            self.down(CONTROL)?;
        }
        if kind == CONTROL {
            if let (Some(record), Some(quic)) = (&self.record, quic) {
                if let Some(media) = &record.quic {
                    media.replace(&quic);
                }
            }
        }
        let disconnected_at = self.lost[CONTROL].unwrap_or_else(Instant::now);
        self.install(kind, generation, wire);
        if kind == MEDIA {
            self.media_needs_keyframe = true;
            self.pending[MEDIA] = None;
            self.media_config.clear();
            self.screen_waiting
                .extend(self.screen_snapshots.keys().map(|(id, _)| *id));
        }
        if kind == CONTROL {
            self.lost[CONTROL] = Some(disconnected_at);
            self.last_control_rx = Instant::now();
            if self.server.is_some() {
                self.synchronize()?;
            }
        } else {
            self.replay(kind)?;
            self.notify(kind, true)?;
            if kind == MEDIA {
                self.deliver(Msg::RequestKeyframe)?;
            }
        }
        Ok(())
    }
    fn synchronize(&mut self) -> io::Result<()> {
        self.replay(CONTROL)?;
        self.sync_revision = self.snapshot_revision;
        let mut sync = packet(SYNC, CONTROL, self.id);
        sync.generation = self.generations[CONTROL];
        sync.state_revision = self.sync_revision;
        self.control(Msg::SessionPacket(sync))
    }
    fn recovered(&mut self) -> io::Result<()> {
        if self.lost[CONTROL].is_none() {
            return Ok(());
        }
        self.lost[CONTROL] = None;
        self.attempts[CONTROL] = 0;
        // Reconcile only unanswered logical requests, retaining their original IDs.
        // The clipboard actor deduplicates these; input and old socket queues are never replayed.
        for request in self.awaiting.values() {
            if let Msg::ClipboardRequest(r) = request {
                if !self.requests.iter().any(|m| matches!(m, Msg::ClipboardRequest(other) if other.request_id == r.request_id)) { self.requests.push_back(request.clone()); }
            }
        }
        if let Some(record) = &self.record {
            *record.deadline.lock().unwrap() = None;
        }
        for kind in [FILES, MEDIA] {
            if self.lost[kind].is_some() {
                self.lost[kind] = Some(Instant::now());
                self.retrying[kind] = false;
                self.next_retry[kind] = Instant::now();
            }
        }
        self.deliver(Msg::RequestKeyframe)?;
        Ok(())
    }
    async fn tick(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if now.duration_since(self.last_app_status) >= Duration::from_secs(2) {
            if self.lost[CONTROL].is_some() {
                self.notify(CONTROL, false)?;
                let _ = self.deliver(Msg::Ping(pb::Ping { timestamp_ms: 0 }));
            }
            if self.media_needs_keyframe && self.links[MEDIA].is_some() {
                let _ = self.deliver(Msg::RequestKeyframe);
            }
            self.last_app_status = now;
        }
        if self.id != 0 {
            if self.links[CONTROL].is_some()
                && now.duration_since(self.last_control_rx) >= Duration::from_secs(10)
            {
                self.down(CONTROL)?;
            }
            for kind in 0..3 {
                if kind != CONTROL
                    && (self.links[CONTROL].is_none() || self.lost[CONTROL].is_some())
                {
                    continue;
                }
                if self.lost[kind].is_some_and(|at| now.duration_since(at) >= RECOVERY) {
                    return Err(error("通道重连超过 30 秒，会话已结束"));
                }
            }
            if self.links[CONTROL].is_some()
                && now.duration_since(self.last_heartbeat) >= Duration::from_secs(2)
            {
                self.control(Msg::SessionPacket(packet(STATUS, CONTROL, self.id)))?;
                self.last_heartbeat = now;
            }
            if self.address.is_some() {
                for kind in 0..3 {
                    if self.links[kind].is_some()
                        || self.lost[kind].is_none()
                        || self.retrying[kind]
                        || now < self.next_retry[kind]
                    {
                        continue;
                    }
                    if kind != CONTROL
                        && (self.links[CONTROL].is_none() || self.lost[CONTROL].is_some())
                    {
                        continue;
                    }
                    let delay = [0, 500, 1000, 2000, 4000][self.attempts[kind].min(4)];
                    self.next_retry[kind] = now + Duration::from_millis(delay);
                    self.attempts[kind] += 1;
                    if kind == CONTROL {
                        self.retry_control();
                    } else {
                        self.control(Msg::SessionPacket(packet(REQUEST, kind, self.id)))?;
                        self.retrying[kind] = true;
                    }
                }
            }
        }
        self.flush_pending().await
    }

    async fn flush_pending(&mut self) -> io::Result<()> {
        if let Some(link) = &self.links[MEDIA] {
            while let Some(config) = self.media_config.pop_front() {
                match link.tx.try_send(config) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(config)) => {
                        self.media_config.push_front(config);
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(config)) => {
                        self.media_config.push_front(config);
                        break;
                    }
                }
            }
        }
        for kind in 0..3 {
            if kind == MEDIA && !self.media_config.is_empty() {
                continue;
            }
            if let Some(msg) = self.pending[kind].take() {
                if !self.outgoing(msg, kind).await? {
                    return Err(error("session closed"));
                }
            }
        }
        if self.links[CONTROL].is_some() && self.lost[CONTROL].is_none() {
            while let Some(request) = self.requests.pop_front() {
                if self.pending[CONTROL].is_some() {
                    self.requests.push_front(request);
                    break;
                }
                self.outgoing(request, CONTROL).await?;
            }
        }
        Ok(())
    }
    async fn run(
        &mut self,
        mut events: mpsc::Receiver<Event>,
        [mut control, mut files, mut media]: [mpsc::Receiver<Msg>; 3],
    ) {
        let lifetime = self
            .server
            .as_ref()
            .and_then(|(listener, _)| listener.cancellation.clone())
            .or_else(|| match &self.address {
                Some(Dialer::Overlay(peer)) => Some(peer.cancel.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let result: io::Result<()> = async {
            loop {
                // A full writer queue used to resume only on the 20 ms timer.
                // Register with each blocked lane's semaphore so small queues
                // propagate backpressure without adding a polling interval.
                let [control_ready, files_ready, media_ready] = std::array::from_fn(|kind| {
                    let blocked = self.pending[kind].is_some()
                        || (kind == MEDIA && !self.media_config.is_empty());
                    // Image chunks may be waiting for the peer's Ready message,
                    // not writer capacity. A writable socket must not spin then.
                    let awaiting_image = matches!(&self.pending[kind], Some(Msg::ClipboardImageChunk(chunk))
                        if self.ready_image != Some((chunk.image_id, self.generations[FILES])));
                    self.links[kind].as_ref().filter(|_| blocked && !awaiting_image).map(|link| link.tx.clone())
                });
                tokio::select! {
                    biased;
                    _ = lifetime.cancelled() => return Err(error("VNT 服务端连接已停止")),
                    _=self.app[CONTROL].closed()=>{
                        // Handshake rejection and normal shutdown enqueue their
                        // final reply immediately before dropping the endpoint.
                        // Preserve that reply instead of letting the biased
                        // closed branch discard it and start peer recovery.
                        while let Ok(message) = control.try_recv() {
                            if matches!(&message, Msg::Denied(_) | Msg::SessionEnd(_)) {
                                self.outgoing(message, CONTROL).await?;
                                break;
                            }
                        }
                        return Ok(());
                    },
                    event=events.recv()=>match event {
                        Some(Event::ControlPressure(msg)) => {
                            if self.id == 0 { return Err(error("initial handshake congested")); }
                            self.down(CONTROL)?;
                            if !matches!(&msg, Msg::SessionPacket(_)) { self.outgoing(msg, CONTROL).await?; }
                        }
                        Some(Event::Answered(id)) => {
                            self.awaiting.remove(&id);
                            self.requests.retain(|m| !matches!(m, Msg::ClipboardRequest(r) if r.request_id == id));
                        }
                        Some(Event::Incoming { kind, generation, msg }) if self.links[kind].as_ref().is_some_and(|l| l.generation == generation) => self.incoming(kind,msg).await?,
                        Some(Event::Down { kind, generation }) if self.links[kind].as_ref().is_some_and(|l| l.generation == generation) => self.down(kind)?,
                        Some(Event::Attach { kind, generation, wire, quic }) => {
                            if kind == MEDIA { while media.try_recv().is_ok() {} }
                            if let Err(e) = self.attached(kind,generation,wire,quic).await { log::debug!("attachment failed: {e}"); }
                        },
                        Some(Event::RetryFailed { kind, terminal }) => { if terminal { return Err(error("会话已过期，请重新连接")); } self.retrying[kind] = false;
                            let delay = [0, 500, 1000, 2000, 4000][self.attempts[kind].saturating_sub(1).min(4)];
                            self.next_retry[kind] = Instant::now() + Duration::from_millis(delay);
                        },
                        None=>return Ok(()), _=>{},
                    },
                    msg=control.recv(), if self.pending[CONTROL].is_none()=>match msg {
                        Some(m)=>{
                            if self.tcp_media.load(Ordering::Acquire) && matches!(&m, Msg::StreamReset(_)) { while media.try_recv().is_ok() {} }
                            if !self.outgoing(m,CONTROL).await? {return Ok(());}
                        }, None=>return Ok(())
                    },
                    ready=writable(control_ready)=>{
                        if ready { self.flush_pending().await?; } else { self.down(CONTROL)?; }
                    },
                    ready=writable(files_ready)=>{
                        if ready { self.flush_pending().await?; } else { self.down(FILES)?; }
                    },
                    ready=writable(media_ready)=>{
                        if ready { self.flush_pending().await?; } else { self.down(MEDIA)?; }
                    },
                    _=tick.tick()=>self.tick().await?,
                    msg=files.recv(), if self.pending[FILES].is_none()=>match msg { Some(m)=>{self.outgoing(m,FILES).await?;}, None=>return Ok(()) },
                    msg=media.recv(), if self.pending[MEDIA].is_none() && self.media_config.is_empty()=>match msg { Some(m)=>{self.outgoing(m,MEDIA).await?;}, None=>return Ok(()) },
                    Some(_)=self.tasks.join_next(), if !self.tasks.is_empty()=>{},
                }
            }
        }.await;
        if let Err(e) = result {
            let _ = self.deliver(Msg::SessionEnd(pb::SessionEnd {
                reason: e.to_string(),
            }));
        }
        if let Some(record) = &self.record {
            record.closed.store(true, Ordering::Release);
            record.grants.lock().unwrap().clear();
        }
        if let Some((listener, _)) = &self.server {
            listener.records.lock().unwrap().remove(&self.id);
        }
        for link in self.links.iter_mut().filter_map(Option::take) {
            link.cancel.cancel();
        }
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

fn is_paste(message:&Msg)->bool {
    matches!(message,Msg::ClipboardPaste(_)) || matches!(message,Msg::ScreenPacket(p) if vnt_proto::screen::kind(p)==Some(vnt_proto::control::MessageType::ClipboardPaste))
}
