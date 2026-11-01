use vnt_net::datagram::Datagram as UdpSocket;
// One authenticated desktop session, with independently paced screen workers.
use crate::config::{AppConfig, CaptureTarget};
use futures_util::future::{poll_fn, BoxFuture};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::DuplexStream,
    sync::{mpsc, RwLock},
};
use tokio_util::sync::CancellationToken;
use vnt_net::tcp::ControlChannel;
use vnt_proto::{control::ControlMessage as Msg, pb, screen};

pub type Channel = ControlChannel<DuplexStream>;
pub struct Backend {
    pub channel: Channel,
    pub welcome: pb::Welcome,
    pub task: tokio::task::JoinHandle<()>,
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub type Open =
    Arc<dyn Fn(AppConfig, pb::Hello) -> BoxFuture<'static, Result<Backend, String>> + Send + Sync>;

pub async fn handshake(
    mut channel: Channel,
    hello: pb::Hello,
    password: &str,
    task: tokio::task::JoinHandle<()>,
) -> Result<Backend, String> {
    // The guard owns the worker even if authentication is cancelled or fails.
    let mut backend = Backend {
        channel: vnt_net::session::local_pair().0,
        welcome: pb::Welcome::default(),
        task,
    };
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        channel
            .send(&Msg::Hello(hello))
            .await
            .map_err(|e| e.to_string())?;
        vnt_net::auth::respond(&mut channel, password)
            .await
            .map_err(|e| e.to_string())?;
        match channel.recv().await.map_err(|e| e.to_string())? {
            Some(Msg::Welcome(welcome)) => Ok(welcome),
            Some(Msg::Denied(d)) => Err(d.reason),
            _ => Err("采集会话已关闭".into()),
        }
    })
    .await
    .map_err(|_| "采集会话建立超时".to_string())??;
    backend.channel = channel;
    backend.welcome = result;
    Ok(backend)
}

#[derive(Clone)]
pub struct Target {
    pub reference: CaptureTarget,
    pub config: AppConfig,
    pub info: pb::ScreenInfo,
}

pub fn targets(
    config: &AppConfig,
    monitors: &[vnt_capture::MonitorInfo],
) -> Result<Vec<Target>, String> {
    let selected = config.targets(monitors);
    if selected.is_empty() {
        return Err("请先选择采集屏幕或区域".into());
    }
    let mut targets = Vec::new();
    for (index, reference) in selected.into_iter().enumerate() {
        let mut cfg = config.clone();
        cfg.selected_targets = vec![reference.clone()];

        let (name, monitor, size) = match &reference {
            CaptureTarget::Monitor { name } => {
                let monitor = monitors.iter().find(|m| &m.name == name);
                (
                    monitor.map_or_else(
                        || format!("屏幕{}", index + 1),
                        |m| format!("屏幕{}", m.index + 1),
                    ),
                    monitor,
                    monitor.map(|m| (m.width, m.height)),
                )
            }
            CaptureTarget::Region { id } => {
                let region = config
                    .screen_regions
                    .iter()
                    .find(|r| &r.id == id)
                    .ok_or("所选屏幕区域不存在")?;

                (
                    region.name.clone(),
                    region.resolve(monitors).ok(),
                    Some((region.width, region.height)),
                )
            }
        };
        if let Some(monitor) = monitor {
            cfg.monitor_index = monitor.index;
        }
        let (width, height) = size.unwrap_or((0, 0));
        targets.push(Target {
            reference,
            config: cfg,
            info: pb::ScreenInfo {
                stream_id: index as u32 + 1,
                name,
                width,
                height,
                available: monitor.is_some(),
                reason: if monitor.is_some() {
                    String::new()
                } else {
                    "显示器或区域暂不可用".into()
                },
                generation: 1,
                ..Default::default()
            },
        });
    }
    Ok(targets)
}

/// Resolve the immutable device reference against live metadata immediately
/// before opening a worker. A stale display index must never choose a new screen.
pub fn resolve_backend(
    config: &mut AppConfig,
    monitors: &[vnt_capture::MonitorInfo],
) -> Result<(), String> {
    if let Some(target) = config.selected_targets.first().cloned() {
        match target {
            CaptureTarget::Monitor { name } => {
                config.monitor_index = monitors
                    .iter()
                    .find(|m| m.name == name)
                    .ok_or("所选显示器暂不可用")?
                    .index;
            }
            CaptureTarget::Region { id } => {
                let region = config
                    .screen_regions
                    .iter()
                    .find(|r| r.id == id)
                    .ok_or("所选区域不存在")?;
                config.monitor_index = region.resolve(monitors)?.index;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    fn contains(self, p: (i32, i32)) -> bool {
        let (x, y) = (i64::from(p.0), i64::from(p.1));
        x >= i64::from(self.x)
            && y >= i64::from(self.y)
            && x < i64::from(self.x) + i64::from(self.width)
            && y < i64::from(self.y) + i64::from(self.height)
    }
}

/// The baseline is independent of the viewed screen, so manual selection is
/// not undone by the next cursor sample (or movement within the same target).
#[derive(Default)]
pub struct CursorFollow {
    position: Option<(i32, i32)>,
    hit: Option<u32>,
}
impl CursorFollow {
    pub fn observe(
        &mut self,
        position: (i32, i32),
        active: u32,
        targets: &[(u32, bool, Rect)],
    ) -> Option<u32> {
        if self.position == Some(position) {
            return None;
        }
        let hit = targets
            .iter()
            .find(|(id, _, r)| *id == active && r.contains(position))
            .or_else(|| {
                targets
                    .iter()
                    .find(|(_, region, r)| *region && r.contains(position))
            })
            .or_else(|| targets.iter().find(|(_, _, r)| r.contains(position)))
            .map(|t| t.0);
        let changed = self.position.is_some() && hit != self.hit;
        self.position = Some(position);
        self.hit = hit;
        if changed {
            hit.filter(|id| *id != active)
        } else {
            None
        }
    }
    pub fn baseline(
        &mut self,
        position: Option<(i32, i32)>,
        active: u32,
        targets: &[(u32, bool, Rect)],
    ) {
        self.position = None;
        self.hit = None;
        if let Some(position) = position {
            self.observe(position, active, targets);
        }
    }
}

struct Worker {
    target: Target,
    backend: Option<Backend>,
    incoming: Option<mpsc::Receiver<Msg>>,
    media_incoming: Option<mpsc::Receiver<Msg>>,
    files_incoming: Option<mpsc::Receiver<Msg>>,
    file_tx: Option<mpsc::Sender<Msg>>,
    outgoing: Option<mpsc::Sender<Msg>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    video_tx: Option<mpsc::Sender<Msg>>,
    video_task: Option<tokio::task::JoinHandle<()>>,
    open: bool,
    closing: bool,
    background: bool,
    retry: tokio::time::Instant,
    rect: Option<Rect>,
    opening: Option<tokio::task::JoinHandle<Result<Backend, String>>>,
    last_frame: tokio::time::Instant,
    failed: Arc<AtomicBool>,
    send_bitrate: Arc<AtomicU32>,
    unavailable_since: Option<tokio::time::Instant>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(task) = &self.opening {
            task.abort();
        }
        for task in &self.tasks {
            task.abort();
        }
        if let Some(task) = &self.video_task {
            task.abort();
        }
    }
}
impl Worker {
    fn stop_backend(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        if let Some(task) = self.video_task.take() {
            task.abort();
        }
        self.backend = None;
        self.incoming = None;
        self.media_incoming = None;
        self.files_incoming = None;
        self.file_tx = None;
        self.outgoing = None;
        self.video_tx = None;
    }
    fn send(&self, msg: Msg) {
        if let Some(tx) = &self.outgoing {
            if tx.try_send(msg).is_err() {
                self.failed.store(true, Ordering::Release);
            }
        }
    }
}

pub struct Transport {
    pub session_id: u32,
    pub udp: Option<Arc<UdpSocket>>,
    pub peer: Arc<RwLock<Option<std::net::SocketAddr>>>,
    pub updates: mpsc::Receiver<Msg>,
    pub cancel: CancellationToken,
    pub started: Arc<dyn Fn(&pb::Welcome) + Send + Sync>,
}

async fn next_event(workers: &mut [Worker], next: &mut usize) -> (usize, Option<Msg>) {
    poll_fn(|cx| {
        for lane in 0..3 {
            for offset in 0..workers.len() {
                let i = (*next + offset) % workers.len();
                let worker = &mut workers[i];
                if lane == 2
                    && worker
                        .video_tx
                        .as_ref()
                        .is_some_and(|tx| tx.capacity() == 0)
                    && worker.open
                    && !worker.closing
                {
                    continue;
                }
                if lane == 1 && worker.file_tx.as_ref().is_some_and(|tx| tx.capacity() == 0) {
                    continue;
                }
                let incoming = match lane {
                    0 => &mut worker.incoming,
                    1 => &mut worker.files_incoming,
                    _ => &mut worker.media_incoming,
                };
                if let Some(rx) = incoming.as_mut() {
                    if let std::task::Poll::Ready(msg) = rx.poll_recv(cx) {
                        *next = (i + 1) % workers.len();
                        return std::task::Poll::Ready((i, msg));
                    }
                }
            }
        }
        std::task::Poll::Pending
    })
    .await
}

fn state(workers: &[Worker], active: u32, revision: u64) -> Msg {
    Msg::ScreensState(pb::ScreensState {
        revision,
        active_stream_id: active,
        screens: workers
            .iter()
            .filter(|w| w.open)
            .map(|w| w.target.info.clone())
            .collect(),
    })
}

fn begin_worker(
    worker: &Worker,
    open: &Open,
    hello: &pb::Hello,
    control_owner: bool,
) -> tokio::task::JoinHandle<Result<Backend, String>> {
    let mut cfg = worker.target.config.clone();
    let mut hello = hello.clone();
    let open = open.clone();
    hello.transport = 2;
    hello.fec_version = 0; // FEC is applied only at the final datagram sender.
    if !control_owner {
        cfg.clipboard = false;
        cfg.audio = false;
        hello.audio_enabled = false;
    }
    tokio::spawn(async move { open(cfg, hello).await })
}
fn attach_worker<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(
    worker: &mut Worker,
    mut backend: Backend,
    hello: &pb::Hello,
    transport: &Transport,
    network: &mut vnt_net::tcp::TcpWriter<W>,
    fec: Option<vnt_net::fec::Sender>,
    repair: Option<vnt_net::repair::VideoRepair>,
) -> Result<(), String> {
    worker
        .send_bitrate
        .store(hello.bitrate_kbps, Ordering::Release);
    let bitrate = worker.send_bitrate.clone();
    worker.last_frame = tokio::time::Instant::now();
    worker.failed.store(false, Ordering::Release);
    worker.unavailable_since = None;
    let info = &mut worker.target.info;
    info.width = backend.welcome.width;
    info.height = backend.welcome.height;
    info.fps = backend.welcome.fps;
    info.codec = backend.welcome.codec;
    info.hardware = backend.welcome.hardware;
    info.available = true;
    info.reason.clear();
    // Move the channel out while retaining the task's cancellation guard.
    let channel = std::mem::replace(&mut backend.channel, vnt_net::session::local_pair().0);
    let (reader, mut writer) = channel.into_split();
    let lanes = reader
        .into_message_lanes()
        .map_err(|_| "采集会话需要独立收件通道")?;
    let (tx, mut rx) = mpsc::channel(64);
    let (incoming_tx, incoming_rx) = mpsc::channel(64);
    let (files_tx, files_rx) = mpsc::channel(8);
    let (media_tx, media_rx) = mpsc::channel(2);
    worker.tasks.push(tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if writer.send_owned(msg).await.is_err() {
                break;
            }
        }
    }));
    for (mut lane, tx) in lanes.into_iter().zip([incoming_tx, files_tx, media_tx]) {
        worker.tasks.push(tokio::spawn(async move {
            while let Some(msg) = lane.recv().await {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        }));
    }
    let (file_tx, mut file_rx) = mpsc::channel(8);
    let mut file_writer = network.fork().ok_or("文件通道不可用")?;
    worker.tasks.push(tokio::spawn(async move {
        while let Some(msg) = file_rx.recv().await {
            if file_writer.send_owned(msg).await.is_err() {
                break;
            }
        }
    }));
    worker.file_tx = Some(file_tx);
    worker.media_incoming = Some(media_rx);
    worker.files_incoming = Some(files_rx);
    let (video_tx, mut video_rx) = mpsc::channel::<Msg>(2);
    let mut net = network.fork().ok_or("视频会话需要独立媒体写入器")?;
    let udp = transport.udp.clone();
    let peer = transport.peer.clone();
    let session_id = transport.session_id;
    let failed = worker.failed.clone();
    worker.video_task = Some(tokio::spawn(async move {
        let mut sender = vnt_net::udp::UdpFrameSender::default();
        sender.set_fec(fec);
        sender.set_repair(repair);
        let mut seq = vnt_net::udp::VideoSequence::default();
        while let Some(msg) = video_rx.recv().await {
            if let (Some(udp), Msg::ScreenPacket(packet)) = (&udp, &msg) {
                if let Some(target) = *peer.read().await {
                    // Each screen owns its own pacing debt and wire sequence.
                    if sender
                        .send_screen_frame(
                            udp,
                            target,
                            session_id,
                            seq.take(),
                            packet,
                            bitrate.load(Ordering::Acquire).saturating_mul(2),
                        )
                        .await
                        .is_err()
                    {
                        failed.store(true, Ordering::Release);
                        break;
                    }
                }
            } else if net.send_owned(msg).await.is_err() {
                failed.store(true, Ordering::Release);
                break;
            }
        }
    }));
    worker.video_tx = Some(video_tx);
    worker.incoming = Some(incoming_rx);
    worker.outgoing = Some(tx);
    worker.backend = Some(backend);
    worker.send(Msg::ScreenThrottle(pb::ScreenThrottle {
        background: worker.background,
    }));
    Ok(())
}

pub async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
    mut remote: ControlChannel<S>,
    hello: pb::Hello,
    targets: Vec<Target>,
    open: Open,
    mut transport: Transport,
) -> Result<(), String> {
    let fec = vnt_net::fec::Sender::negotiated(hello.fec_version, hello.transport != 2);
    let (video_repair, _repair_task) =
        if let Some(socket) = transport.udp.as_ref().filter(|_| hello.transport != 2) {
            let (repair, task) =
                vnt_net::repair::VideoRepair::start(socket.clone(), transport.peer.clone());
            (Some(repair), Some(task))
        } else {
            (None, None)
        };
    let valid_transport = vnt_proto::Transport::from_wire(hello.transport).ok();
    if hello.desktop_effects.is_none()
        || vnt_proto::color_depth::from_wire(hello.color_depth).is_none()
        || hello.fec_version > vnt_net::fec::VERSION
        || (hello.transport == 2 && hello.fec_version != 0)
        || !(1..=120).contains(&hello.fps)
        || !(100..=200_000).contains(&hello.bitrate_kbps)
        || !crate::resolution::validate(
            hello.resolution_mode,
            hello.target_width,
            hello.target_height,
        )
        || valid_transport.is_none()
        || (hello.transport != 2 && transport.udp.is_none())
    {
        remote
            .send(&Msg::Denied(pb::Denied {
                reason: "连接的视频参数或传输方式无效".into(),
            }))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let mut workers: Vec<_> = targets
        .into_iter()
        .map(|target| Worker {
            target,
            backend: None,
            incoming: None,
            media_incoming: None,
            files_incoming: None,
            file_tx: None,
            outgoing: None,
            tasks: Vec::new(),
            video_tx: None,
            video_task: None,
            open: true,
            closing: false,
            background: false,
            rect: None,
            opening: None,
            last_frame: tokio::time::Instant::now(),
            failed: Arc::new(AtomicBool::new(false)),
            send_bitrate: Arc::new(AtomicU32::new(hello.bitrate_kbps)),
            unavailable_since: None,
            retry: tokio::time::Instant::now(),
        })
        .collect();
    let owner = workers
        .iter()
        .position(|w| w.target.info.available)
        .ok_or("所有采集目标均不可用")?;
    let mut inner_hello = hello.clone();
    let multi_change = workers.len() > 1 && hello.resolution_mode == 2;
    if multi_change {
        inner_hello.resolution_mode = 1;
    }
    // Welcome must precede frames; each backend has a bounded IPC queue.
    let mut network = remote.fork_writer().ok_or("多屏需要托管远程会话")?;
    for (i, worker) in workers.iter_mut().enumerate() {
        if worker.target.info.available {
            worker.opening = Some(begin_worker(worker, &open, &inner_hello, i == owner));
            worker.target.info.available = false;
            worker.target.info.reason = "正在建立屏幕采集".into();
        }
    }
    let initial = tokio::time::timeout(Duration::from_secs(20), async {
        while !workers.iter().any(|worker| worker.backend.is_some()) {
            if workers.iter().all(|worker| worker.opening.is_none()) {
                break;
            }
            let (i, result) = poll_fn(|cx| {
                for (i, worker) in workers.iter_mut().enumerate() {
                    if let Some(task) = worker.opening.as_mut() {
                        if let std::task::Poll::Ready(result) =
                            std::future::Future::poll(std::pin::Pin::new(task), cx)
                        {
                            worker.opening = None;
                            return std::task::Poll::Ready((
                                i,
                                result.map_err(|e| e.to_string()).and_then(|r| r),
                            ));
                        }
                    }
                }
                std::task::Poll::Pending
            })
            .await;
            let worker = &mut workers[i];
            if let Err(reason) = result.and_then(|backend| {
                attach_worker(
                    worker,
                    backend,
                    &inner_hello,
                    &transport,
                    &mut network,
                    fec.clone(),
                    video_repair.clone(),
                )
            }) {
                log::warn!("屏幕采集初始化失败 {}: {reason}", worker.target.info.name);
                worker.target.info.reason = reason;
                worker.retry = tokio::time::Instant::now() + Duration::from_secs(2);
            }
        }
    });
    tokio::select! {_ = transport.cancel.cancelled()=>return Ok(()),_ = initial=>{}}
    let Some(first) = workers.iter().find(|w| w.backend.is_some()) else {
        let reasons = workers
            .iter()
            .map(|worker| format!("{}: {}", worker.target.info.name, worker.target.info.reason))
            .collect::<Vec<_>>()
            .join("；");
        remote
            .send(&Msg::Denied(pb::Denied {
                reason: format!("所有采集目标均不可用：{reasons}"),
            }))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    };
    let mut welcome = first.backend.as_ref().unwrap().welcome.clone();
    welcome.session_id = transport.session_id;
    welcome.transport = hello.transport;
    welcome.fec_version = if fec.is_some() {
        vnt_net::fec::VERSION
    } else {
        0
    };
    welcome.bind_token = Vec::new(); // Installed by the authenticated caller below.
    welcome.screens = workers.iter().map(|w| w.target.info.clone()).collect();
    welcome.audio_enabled = hello.audio_enabled && workers[owner].target.config.audio;
    if multi_change {
        welcome.resolution_mode = 1;
        welcome.resolution_reason = "多屏共享不修改物理分辨率，已使用逐屏等比缩放".into();
    }
    // The caller passes the external binding token as the initial update.
    if let Some(Msg::Welcome(binding)) = transport.updates.recv().await {
        welcome.bind_token = binding.bind_token;
    }
    (transport.started)(&welcome);
    remote
        .send(&Msg::Welcome(welcome))
        .await
        .map_err(|e| e.to_string())?;
    let (mut reader, mut writer) = remote.into_split();
    let mut active = first.target.info.stream_id;
    let mut revision = 1;
    writer
        .send_owned(state(&workers, active, revision))
        .await
        .map_err(|e| e.to_string())?;
    let mut next = 0;
    let mut timer = tokio::time::interval(Duration::from_millis(50));
    let mut cursor = CursorFollow::default();
    let mut observed_cursor = None;
    let mut last_peer = None;
    let mut last_heartbeat = tokio::time::Instant::now();
    let established_at = last_heartbeat;
    loop {
        tokio::select! {
            biased;
            _=transport.cancel.cancelled()=>break,
            msg=reader.recv()=>{
                let msg=msg.map_err(|e|e.to_string())?.ok_or("主控已断开")?;
                match msg {
                    Msg::SessionEnd(_)=>break,
                    Msg::Ping(p)=>{last_heartbeat=tokio::time::Instant::now(); for w in &workers {w.send(Msg::Ping(p));} writer.send_owned(Msg::Pong(pb::Pong{timestamp_ms:p.timestamp_ms})).await.map_err(|e|e.to_string())?;}
                    Msg::ScreenCommand(command)=>{

                        let Some(i)=workers.iter().position(|w|w.open && !w.closing && w.target.info.stream_id==command.stream_id) else {continue;};
                        if command.action==pb::screen_command::Action::Select as i32 {
                            if command.stream_id!=active {
                                for w in &workers {w.send(Msg::ResetInput(pb::ResetInput{}));}
                                active=command.stream_id; revision+=1;
                                let geometry=geometry(&workers); cursor.baseline(observed_cursor,active,&geometry);
                            }
                        } else if command.action==pb::screen_command::Action::Close as i32 && workers.iter().filter(|w|w.open && !w.closing).count()>1 {
                            workers[i].closing=true;
                            if let Some(task)=workers[i].opening.take(){task.abort();}
                            if let Some(task)=workers[i].video_task.take() {task.abort(); let _=task.await;}
                            workers[i].video_tx=None;
                            workers[i].send(Msg::ResetInput(pb::ResetInput{}));
                            if workers[i].backend.is_some() {workers[i].send(Msg::StopVideo(pb::StopVideo{acknowledged:false}));}
                            else {finish_close(&mut workers,i,&mut active,&mut revision);}
                        }
                        writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;
                    }
                    Msg::ScreenPacket(packet)=>{
                        if let Some(inner)=screen::unwrap(&packet) {
                            if let Some(w)=workers.iter_mut().find(|w|w.open && !w.closing && w.target.info.stream_id==packet.stream_id && w.target.info.generation==packet.generation) {
                                if let Msg::ScreenThrottle(state)=&inner {w.background=state.background;}
                                if let Msg::LinkFeedback(report)=&inner {if let Some(fec)=&fec {fec.feedback(report);}}
                                if matches!(inner,Msg::InputEvent(_)|Msg::ResetInput(_)|Msg::ClipboardPaste(_)) && (packet.stream_id!=active || packet.revision!=revision) {continue;}
                                if matches!(inner,Msg::ClipboardPaste(_)) {workers[owner].send(inner);continue;}
                                if matches!(inner,Msg::InputEvent(_)|Msg::ResetInput(_)|Msg::LinkFeedback(_)|Msg::RequestKeyframe|Msg::ScreenThrottle(_)) {w.send(inner);}
                            }
                        }
                    }
                    Msg::TransportState(ref state)=>{
                        for w in &workers {w.send(msg.clone()); if state.available && w.open {w.send(Msg::RequestKeyframe);}}
                    }
                    Msg::VideoNack(nack)=>{
                        if workers.iter().any(|w|w.open && !w.closing && w.target.info.stream_id==nack.stream_id && w.target.info.generation==nack.generation) { if let Some(repair)=&video_repair {repair.request(nack);} }
                    }
                    Msg::RequestKeyframe=>{for w in workers.iter().filter(|w|w.open && !w.closing){w.send(Msg::RequestKeyframe);}}
                    Msg::InputEvent(_)|Msg::ClipboardPaste(_)=>{} // Inputs must name the viewed stream.
                    Msg::LinkFeedback(report)=>{if let Some(fec)=&fec{fec.feedback(&report);}}
                    other=>{workers[owner].send(other);}
                }
            }
            update=transport.updates.recv()=>{if let Some(update)=update {writer.send_owned(update).await.map_err(|e|e.to_string())?;}}
            (i,msg)=next_event(&mut workers,&mut next)=>{
                let Some(msg)=msg else {
                    workers[i].stop_backend(); workers[i].target.info.available=false; workers[i].target.info.reason="采集会话恢复中".into(); workers[i].retry=tokio::time::Instant::now()+Duration::from_secs(1);
                    if workers[i].closing {finish_close(&mut workers,i,&mut active,&mut revision);} else {revision+=1;}
                    writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?; continue;
                };
                if matches!(msg,Msg::StopVideo(pb::StopVideo{acknowledged:true})) {
                    if workers[i].closing {finish_close(&mut workers,i,&mut active,&mut revision); if i!=owner {workers[i].stop_backend();} writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;}
                    continue;
                }
                if let Msg::CursorPosition(sample)=msg {
                    if sample.available {
                        observed_cursor=Some((sample.x,sample.y));
                        workers[i].rect=Some(Rect{x:sample.monitor_x,y:sample.monitor_y,width:sample.monitor_width,height:sample.monitor_height});
                    }
                    continue;
                }
                let w=&mut workers[i];
                if matches!(msg,Msg::VideoFrame(_)|Msg::StreamReset(_)|Msg::DesktopState(_)|Msg::StatsReport(_)) {
                    if !w.open || w.closing {continue;}
                    if let Msg::StatsReport(stats)=&msg {if stats.applied_bitrate_kbps>0{w.send_bitrate.store(stats.applied_bitrate_kbps,Ordering::Release);}}
                    if let Msg::StreamReset(reset)=&msg {w.target.info.width=reset.width;w.target.info.height=reset.height;w.target.info.fps=reset.fps;w.target.info.generation=w.target.info.generation.wrapping_add(1).max(1);}
                    if let Msg::DesktopState(status)=&msg {w.target.info.available=status.ready;w.target.info.reason=status.reason.clone();if status.ready{w.unavailable_since=None;}else{w.unavailable_since.get_or_insert(tokio::time::Instant::now());}revision+=1;}
                    let wrapped=screen::wrap(w.target.info.stream_id,w.target.info.generation,msg.clone());
                    if matches!(msg,Msg::VideoFrame(_)) {
                        if !w.target.info.available{continue;}
                        w.last_frame=tokio::time::Instant::now();
                        if let Some(tx)=&w.video_tx {
                            // Never drop a reference frame locally: backpressure this
                            // worker while the other workers retain independent queues.
                            tokio::select!{_ = transport.cancel.cancelled()=>break, result=tx.send(wrapped)=>{if result.is_err(){w.failed.store(true,Ordering::Release);}}}
                        }
                    } else {
                        writer.send_owned(wrapped).await.map_err(|e|e.to_string())?;
                        if matches!(msg,Msg::DesktopState(_)|Msg::StreamReset(_)) {revision+=1;writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;}
                    }
                } else if let Msg::SessionEnd(end)=msg {
                    w.stop_backend();w.target.info.available=false;w.target.info.reason=end.reason;w.retry=tokio::time::Instant::now()+Duration::from_secs(1);revision+=1;
                    writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;
                } else if matches!(msg,Msg::ClipboardPasteResult(_)) {
                    writer.send_owned(msg).await.map_err(|e|e.to_string())?;
                } else if i==owner && !matches!(msg,Msg::Pong(_)|Msg::Welcome(_)) {
                    if matches!(msg,Msg::ClipboardReply(_)|Msg::ClipboardImageChunk(_)){
                        if let Some(tx)=&workers[i].file_tx{tx.try_send(msg).map_err(|_|"文件转发通道已关闭")?;}
                    }else if let (Some(udp),Msg::AudioFrame(frame))=(&transport.udp,&msg){
                        if let Some(target)=*transport.peer.read().await{let _=tokio::time::timeout(Duration::from_millis(10),vnt_net::udp::send_audio(udp,target,transport.session_id,frame,fec.as_ref())).await;}
                    }else{writer.send_owned(msg).await.map_err(|e|e.to_string())?;}
                }
            }
            _=timer.tick()=>{
                if last_heartbeat.elapsed()>Duration::from_secs(45) {return Err("主控心跳超时".into());}
                if transport.udp.is_some() && established_at.elapsed()>Duration::from_secs(10) && transport.peer.read().await.is_none(){
                    writer.send_owned(Msg::SessionEnd(pb::SessionEnd{reason:"主控未建立 UDP 视频通道，请检查防火墙设置".into()})).await.map_err(|e|e.to_string())?;break;
                }
                let peer=*transport.peer.read().await;
                if peer.is_some() && peer!=last_peer {last_peer=peer;writer.send_owned(Msg::UdpBindAck(pb::UdpBindAck{session_id:transport.session_id})).await.map_err(|e|e.to_string())?;for w in workers.iter().filter(|w|w.open){w.send(Msg::RequestKeyframe);}}
                for i in 0..workers.len() {
                    if workers[i].failed.swap(false,Ordering::AcqRel){
                        workers[i].stop_backend();workers[i].target.info.available=false;workers[i].target.info.reason="屏幕通道恢复中".into();workers[i].retry=tokio::time::Instant::now()+Duration::from_secs(1);
                        if workers[i].closing{finish_close(&mut workers,i,&mut active,&mut revision);}else{revision+=1;}
                        writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;
                    }
                    if workers[i].opening.as_ref().is_some_and(|task|task.is_finished()) {
                        let result=workers[i].opening.take().unwrap().await.map_err(|e|e.to_string()).and_then(|r|r).and_then(|backend|attach_worker(&mut workers[i],backend,&inner_hello,&transport,&mut network, fec.clone(), video_repair.clone()));
                        if let Err(reason)=result{workers[i].target.info.available=false;workers[i].target.info.reason=reason;workers[i].retry=tokio::time::Instant::now()+Duration::from_secs(2);}
                        if !workers[i].open && i==owner {workers[i].send(Msg::StopVideo(pb::StopVideo{acknowledged:false}));}
                        revision+=1;writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;
                    }
                    if workers[i].open && !workers[i].closing && workers[i].backend.is_some() && ((workers[i].target.info.available && workers[i].last_frame.elapsed()>Duration::from_secs(10)) || workers[i].unavailable_since.is_some_and(|at|at.elapsed()>Duration::from_secs(2))) {
                        workers[i].stop_backend();workers[i].target.info.available=false;workers[i].target.info.reason="画面暂不可用，正在恢复此屏幕".into();revision+=1;
                        writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;
                    }
                    if (workers[i].open || i==owner) && !workers[i].closing && workers[i].backend.is_none() && workers[i].opening.is_none() && tokio::time::Instant::now()>=workers[i].retry {
                        workers[i].target.info.generation=workers[i].target.info.generation.wrapping_add(1).max(1);
                        workers[i].opening=Some(begin_worker(&workers[i],&open,&inner_hello,i==owner));
                    }
                }
                if let Some(position)=observed_cursor {
                    let geometry=geometry(&workers);
                    if let Some(id)=cursor.observe(position,active,&geometry) {
                        for w in &workers {w.send(Msg::ResetInput(pb::ResetInput{}));}
                        active=id;revision+=1;writer.send_owned(state(&workers,active,revision)).await.map_err(|e|e.to_string())?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn finish_close(workers: &mut [Worker], i: usize, active: &mut u32, revision: &mut u64) {
    workers[i].open = false;
    workers[i].closing = false;
    if *active == workers[i].target.info.stream_id {
        for offset in 1..workers.len() {
            let w = &workers[(i + offset) % workers.len()];
            if w.open && !w.closing {
                *active = w.target.info.stream_id;
                break;
            }
        }
    }
    *revision += 1;
}

fn geometry(workers: &[Worker]) -> Vec<(u32, bool, Rect)> {
    workers
        .iter()
        .filter(|w| w.open && !w.closing && w.target.info.available)
        .filter_map(|w| {
            let region = match &w.target.reference {
                CaptureTarget::Region { id } => {
                    w.target.config.screen_regions.iter().find(|r| &r.id == id)
                }
                _ => None,
            };
            let mut rect = w.rect?;
            if let Some(region) = region {
                rect.x += region.x as i32;
                rect.y += region.y as i32;
                rect.width = region.width;
                rect.height = region.height;
            }
            Some((w.target.info.stream_id, region.is_some(), rect))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn initial_failure_reports_the_backend_reason_to_the_controller() {
        let monitor = vnt_capture::synthetic_monitor(0, 100, 100);
        let config = AppConfig::initial(std::slice::from_ref(&monitor));
        let targets = targets(&config, &[monitor]).unwrap();
        let (mut client, host) = vnt_net::session::local_pair();
        let (_updates, rx) = mpsc::channel(1);
        let open: Open = Arc::new(|_, _| {
            Box::pin(async {
                Err("免安装版仅支持当前用户的普通桌面，请解锁后重试".into())
            })
        });
        let task = tokio::spawn(serve(
            host,
            pb::Hello {
                desktop_effects: Some(Default::default()),
                color_depth: 16,
                transport: 2,
                fps: 15,
                bitrate_kbps: 2000,
                ..Default::default()
            },
            targets,
            open,
            Transport {
                session_id: 1,
                udp: None,
                peer: Arc::new(RwLock::new(None)),
                updates: rx,
                cancel: CancellationToken::new(),
                started: Arc::new(|_| panic!("failed backend must not start a session")),
            },
        ));
        let message = tokio::time::timeout(Duration::from_secs(3), client.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Msg::Denied(denied) = message else {
            panic!("expected a denial, received {message:?}");
        };
        assert!(denied.reason.contains("屏幕1"));
        assert!(denied.reason.contains("请解锁后重试"));
        task.await.unwrap().unwrap();
    }

    #[test]
    fn overlapping_regions_prefer_the_current_target_then_regions_in_configuration_order() {
        let targets = vec![
            (
                1,
                false,
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                2,
                true,
                Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 20,
                },
            ),
            (
                3,
                true,
                Rect {
                    x: 10,
                    y: 0,
                    width: 30,
                    height: 30,
                },
            ),
        ];
        let mut follow = CursorFollow::default();
        assert_eq!(follow.observe((5, 5), 1, &targets), None);
        assert_eq!(
            follow.observe((15, 5), 1, &targets),
            None,
            "the current full display wins while it contains the cursor"
        );
        follow.baseline(Some((5, 5)), 2, &targets);
        assert_eq!(follow.observe((15, 5), 2, &targets), None);
        assert_eq!(
            follow.observe((25, 5), 2, &targets),
            Some(3),
            "a matching region wins over the full display"
        );
        assert_eq!(follow.observe((90, 5), 3, &targets), Some(1));
    }
    #[test]
    fn follow_uses_crossings_and_preserves_manual_selection() {
        let targets = vec![
            (
                1,
                false,
                Rect {
                    x: -100,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                2,
                false,
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ];
        let mut follow = CursorFollow::default();
        assert_eq!(follow.observe((-50, 50), 1, &targets), None);
        follow.baseline(Some((-50, 50)), 2, &targets);
        assert_eq!(follow.observe((-49, 50), 2, &targets), None);
        assert_eq!(follow.observe((1, 50), 1, &targets), Some(2));
        assert_eq!(follow.observe((500, 50), 2, &targets), None);
    }
    #[test]
    fn last_screen_and_order_are_preserved() {
        let mut cfg = AppConfig {
            selected_targets: vec![],
            ..Default::default()
        };
        assert!(targets(&cfg, &[]).is_err());
        cfg = AppConfig::initial(&[vnt_capture::synthetic_monitor(0, 100, 100)]);
        assert_eq!(
            cfg.targets(&[vnt_capture::synthetic_monitor(0, 100, 100)]),
            vec![CaptureTarget::Monitor {
                name: "Synthetic".into()
            }]
        );
    }
}
