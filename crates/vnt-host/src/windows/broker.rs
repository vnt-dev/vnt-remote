use super::{
    agent::{Bootstrap, DesktopEvent, Metadata, OpenSession, RootRequest},
    management::{read_json, write_json, Status},
    native,
};
use anyhow::Context;
use vnt_net::datagram::Datagram as UdpSocket;
mod display;
mod effects;
mod video;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::RwLock};
use tokio_util::sync::CancellationToken;
use video::{VideoRecovery, VideoSender};
use vnt_net::tcp::ControlChannel;
use vnt_proto::{control::ControlMessage, pb};

type AgentChannel = ControlChannel<tokio::net::windows::named_pipe::NamedPipeServer>;
#[derive(Default)]
struct RecoveryTasks {
    media: Option<tokio::task::JoinHandle<anyhow::Result<(Worker, pb::Welcome)>>>,
    user: Option<tokio::task::JoinHandle<anyhow::Result<Worker>>>,
    audio: Option<tokio::task::JoinHandle<anyhow::Result<Worker>>>,
}
impl Drop for RecoveryTasks {
    fn drop(&mut self) {
        if let Some(t) = self.media.take() {
            t.abort();
        }
        if let Some(t) = self.user.take() {
            t.abort();
        }
        if let Some(t) = self.audio.take() {
            t.abort();
        }
    }
}
struct Worker {
    desktop: tokio::sync::watch::Receiver<Option<DesktopEvent>>,
    process: Arc<native::Process>,
    channel: AgentChannel,
    metadata: Metadata,
}
#[derive(Clone)]
struct Binding {
    native: bool,
    token: Vec<u8>,
    ip: std::net::IpAddr,
    target: Arc<RwLock<Option<std::net::SocketAddr>>>,
}
pub struct Broker {
    overlay: tokio::sync::mpsc::Sender<crate::link::Incoming>,
    pub status: Arc<Mutex<Status>>,
    pub cancel: CancellationToken,
    stopped: tokio::sync::watch::Receiver<bool>,
}
fn listener_error(protocol: &str, port: u16, error: std::io::Error) -> anyhow::Error {
    let reason = match error.kind() {
        std::io::ErrorKind::AddrInUse => "端口已被占用，请关闭占用程序或修改被控端口",
        std::io::ErrorKind::PermissionDenied => {
            "端口访问被拒绝，可能已被独占或被系统保留，请检查权限或修改端口"
        }
        _ => "无法绑定监听地址",
    };
    anyhow::anyhow!("无法启动被控服务：{protocol} 端口 {port}，{reason}（{error}）")
}
impl Broker {
    pub fn accept_overlay(
        &self,
        channel: vnt_net::session::SessionChannel,
        peer: std::net::SocketAddr,
        media: Arc<UdpSocket>,
    ) {
        let _ = self.overlay.try_send((channel, peer, media));
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        if !*self.stopped.borrow_and_update() {
            let _ = self.stopped.changed().await;
        }
        let failure = self.status.lock().unwrap().error.clone();
        if let Some(error) = failure.filter(|e| e.starts_with("恢复被控")) {
            anyhow::bail!(error);
        }
        Ok(())
    }
    pub async fn start(
        config: crate::config::AppConfig,
        cancel: CancellationToken,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", config.port))
            .await
            .map_err(|error| listener_error("TCP", config.port, error))?;
        let socket = tokio::net::UdpSocket::bind(("0.0.0.0", config.port))
            .await
            .map_err(|error| listener_error("UDP", config.port, error))?;
        let bindings: Arc<Mutex<std::collections::HashMap<u32, Binding>>> = Default::default();
        let map = bindings.clone();
        let validate: vnt_net::quic::ValidateBind = Arc::new(move |id, token, peer| {
            map.lock()
                .unwrap()
                .get(&id)
                .is_some_and(|b| b.native && b.token == token && b.ip == peer.ip())
        });
        let passwords: vnt_net::quic::Password = Arc::new(|| {
            super::store::load()
                .map(|cfg| cfg.password)
                .map_err(std::io::Error::other)
        });
        let (mut quic, udp) =
            vnt_net::quic::Listener::new(socket, config.password.clone(), passwords, validate)
                .await?;
        let udp = Arc::new(udp);
        let status = Arc::new(Mutex::new(Status {
            installed: true,
            state: "running".into(),
            running: true,
            port: config.port,
            password_configured: !config.password.trim().is_empty(),
            ..Default::default()
        }));
        let (overlay, mut overlay_rx) = tokio::sync::mpsc::channel::<crate::link::Incoming>(16);
        let remote_sessions = vnt_net::session::SessionListener::default();
        let udp_cancel = cancel.clone();
        let udp_socket = udp.clone();
        let map = bindings.clone();
        let udp_task = tokio::spawn(async move {
            // 绑定帧是 5 字节头 + 小型 protobuf；按协议上限取一块小缓冲即可
            let mut buf = vec![0; vnt_proto::udp::DEFAULT_MTU + vnt_proto::udp::FRAG_HEADER_LEN];
            loop {
                let received = tokio::select! {_=udp_cancel.cancelled()=>break,v=udp_socket.recv_from(&mut buf)=>v};
                let Ok((n, peer)) = received else { continue };
                let Some((id, token)) = vnt_proto::control::decode_udp_bind_frame(&buf[..n]) else {
                    continue;
                };
                let binding = map.lock().unwrap().get(&id).cloned();
                if let Some(b) = binding {
                    if b.token == token && b.ip == peer.ip() {
                        *b.target.write().await = Some(peer);
                    }
                }
            }
        });
        let probe_cancel = cancel.clone();
        let probe_status = status.clone();
        let probe_task = tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! { biased; _=probe_cancel.cancelled()=>break,_=timer.tick()=>{

                } }
                let session = match native::capture_session() {
                    Ok(session) => session,
                    Err(error) => {
                        let mut status = probe_status.lock().unwrap();
                        status.console_session = None;
                        status.monitors.clear();
                        status.desktop.clear();
                        status.error = Some(format!("选择被控会话失败：{error:#}"));
                        continue;
                    }
                };
                let probe_config = match super::store::load() {
                    Ok(config) => config,
                    Err(error) => {
                        probe_status.lock().unwrap().error =
                            Some(format!("读取被控配置失败：{error:#}"));
                        continue;
                    }
                };
                let result = if let Some(id) = session {
                    tokio::select! {biased; _=probe_cancel.cancelled()=>break,result=metadata(&probe_config,id)=>result}
                } else {
                    Err(anyhow::anyhow!("暂无可采集的交互会话"))
                };
                let mut s = probe_status.lock().unwrap();
                s.console_session = session;
                match result {
                    Ok(meta) => {
                        s.monitors = meta.monitors;
                        s.desktop = meta.desktop;
                        if !s
                            .error
                            .as_deref()
                            .is_some_and(|e| e.starts_with("恢复被控"))
                        {
                            s.error = None;
                        }
                    }
                    Err(error) => {
                        s.monitors.clear();
                        s.desktop.clear();
                        if !s
                            .error
                            .as_deref()
                            .is_some_and(|e| e.starts_with("恢复被控"))
                        {
                            s.error = Some(error.to_string());
                        }
                    }
                }
            }
        });
        let (stopped_tx, stopped) = tokio::sync::watch::channel(false);
        let loop_cancel = cancel.clone();
        let state = status.clone();
        tokio::spawn(async move {
            let permits = Arc::new(tokio::sync::Semaphore::new(crate::server::MAX_CONNECTIONS));
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _=loop_cancel.cancelled()=>break,
                    Some(result)=tasks.join_next(),if !tasks.is_empty()=>{
                        if let Err(e) = result {
                            tracing::warn!("远程代理任务异常：{e}");
                        }
                    },
                    Some((channel, peer, media)) = overlay_rx.recv() => {
                        if let Ok(permit) = permits.clone().try_acquire_owned() {
                            let state = state.clone(); let bindings = bindings.clone(); let cancel = loop_cancel.clone();
                            tasks.spawn(async move { let _permit = permit; if let Err(error) = session(channel, peer, media, state, bindings, cancel, None).await { tracing::warn!("VNT 被控会话结束：{error:#}"); } });
                        }
                    },
                    Some(accepted) = quic.accept() => {
                        if let Ok(permit) = permits.clone().try_acquire_owned() {
                            let state = state.clone(); let bindings = bindings.clone(); let cancel = loop_cancel.clone();
                            tasks.spawn(async move {
                                let _permit = permit;
                                if let Err(error) = session(accepted.channel, accepted.peer, accepted.media, state, bindings, cancel, None).await {
                                    tracing::warn!("QUIC 被控会话结束: {error:#}");
                                }
                            });
                        }
                    },
                    accepted=listener.accept()=>{
                        let Ok((stream, peer)) = accepted else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            continue;
                        };
                        let remote_sessions = remote_sessions.clone();
                        let udp = udp.clone();
                        let state = state.clone();
                        let bindings = bindings.clone();
                        let cancel = loop_cancel.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            let result = async {
                                if let Some(channel) = remote_sessions.accept(stream, peer).await? {
                                    session(channel, peer, udp, state, bindings, cancel, None).await?;
                                }
                                Ok::<_, anyhow::Error>(())
                            }.await;
                            if let Err(e) = result {
                                tracing::warn!(%peer, "被控会话结束：{e:#}");
                            }
                        });
                    }
                }
            }
            loop_cancel.cancel();
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            let _ = udp_task.await;
            let _ = probe_task.await;
            // Stopping the host now leaves the service alive. Release pooled
            // agents too, so a restart cannot reuse stale capture/configuration.
            effects::wait_releases().await;
            if let Some(pool) = POOL.get() {
                pool.lock().await.clear();
            }
            display::wait_restores().await;
            drop(quic);
            drop(listener);
            drop(udp);
            {
                let mut status = state.lock().unwrap();
                status.running = false;
                status.state = "stopped".into();
            }
            let _ = stopped_tx.send(true);
        });
        Ok(Self {
            overlay,
            status,
            cancel,
            stopped,
        })
    }
}
struct Registration {
    id: u32,
    status: Arc<Mutex<Status>>,
    bindings: Arc<Mutex<std::collections::HashMap<u32, Binding>>>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.bindings.lock().unwrap().remove(&self.id);
        self.status
            .lock()
            .unwrap()
            .sessions
            .retain(|s| s.session_id != self.id);
    }
}
async fn send<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    ch: &mut ControlChannel<S>,
    m: &ControlMessage,
) -> anyhow::Result<()> {
    let message = m.msg_type();
    let channel = std::any::type_name::<S>();
    tokio::time::timeout(Duration::from_secs(2), ch.send(m))
        .await
        .with_context(|| format!("发送 {message:?} 到 {channel} 超时（2 秒）"))?
        .with_context(|| format!("发送 {message:?} 到 {channel} 失败"))?;
    Ok(())
}
struct AgentRoot {
    process: Arc<native::Process>,
    control: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>,
    desktop: tokio::sync::watch::Receiver<Option<DesktopEvent>>,
    reader: tokio::task::JoinHandle<()>,
    metadata: Metadata,
    secret: String,
    identity: String,
}
impl Drop for AgentRoot {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

// Passwords never enter the pool key. Existing workers keep their original root
// and desktop notifications while new sessions use the updated policy.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AgentKey {
    session: u32,
    user: bool,
    monitor: u32,
    monitor_name: Option<String>,
    hardware: bool,
    audio: bool,
    clipboard: bool,
    control: bool,
}
impl AgentKey {
    fn new(cfg: &crate::config::AppConfig, session: u32, user: bool) -> Self {
        let monitor_name = cfg
            .selected_targets
            .first()
            .and_then(|target| match target {
                crate::config::CaptureTarget::Monitor { name } => Some(name.clone()),
                crate::config::CaptureTarget::Region { id } => cfg
                    .screen_regions
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| r.monitor_name.clone()),
            });
        Self {
            session,
            user,
            monitor: if monitor_name.is_some() {
                0
            } else {
                cfg.monitor_index
            },
            monitor_name,
            hardware: cfg.hardware_encode,
            audio: user && cfg.audio,
            clipboard: user && cfg.clipboard,
            control: cfg.allow_control,
        }
    }
}
type AgentPool = std::collections::HashMap<AgentKey, AgentRoot>;
static POOL: std::sync::OnceLock<tokio::sync::Mutex<AgentPool>> = std::sync::OnceLock::new();

pub async fn update_capture_border(enabled: bool) -> anyhow::Result<()> {
    let Some(pool) = POOL.get() else {
        return Ok(());
    };
    let mut pool = pool.lock().await;
    let mut failure = None;
    for root in pool.values_mut() {
        if !root.process.running() || root.reader.is_finished() {
            continue;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            write_json(&mut root.control, &RootRequest::CaptureBorder(enabled)),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            failure = Some(anyhow::anyhow!("配置已保存，但更新被控边框失败，请重试"));
        }
    }
    failure.map_or(Ok(()), Err)
}
async fn root(
    cfg: &crate::config::AppConfig,
    session: u32,
    user: bool,
) -> anyhow::Result<AgentRoot> {
    anyhow::ensure!(
        native::capture_session()? == Some(session),
        "被控交互会话已切换"
    );
    let identity = native::session_identity(session)?;
    let name = format!(r"\\.\pipe\VntRemote.Agent.{}", native::random()?);
    let sddl = agent_acl(session, user)?;
    let mut pipe = native::pipe(&name, &sddl, true)?;
    let process = Arc::new(native::spawn_agent(
        &std::env::current_exe()?,
        session,
        user,
        &name,
        std::process::id(),
    )?);
    let secret = native::random()?;
    let mut config = cfg.clone();
    // Border visibility is live policy, including agents recreated by old sessions.
    config.capture_border = super::store::load()?.capture_border;
    config.password.clear();
    let metadata = tokio::time::timeout(Duration::from_secs(12), async {
        pipe.connect().await?;
        anyhow::ensure!(
            native::client_pid(&pipe)? == process.pid,
            "会话代理身份不匹配"
        );
        write_json(
            &mut pipe,
            &Bootstrap {
                secret: secret.clone(),
                config,
            },
        )
        .await?;
        read_json::<_, Result<Metadata, String>>(&mut pipe)
            .await?
            .map_err(anyhow::Error::msg)
    })
    .await??;
    anyhow::ensure!(
        native::capture_session()? == Some(session)
            && identity == native::session_identity(session)?,
        "用户会话在启动代理时发生变化"
    );
    let (mut read, control) = tokio::io::split(pipe);
    let (events, desktop) = tokio::sync::watch::channel(None);
    let reader = tokio::spawn(async move {
        while let Ok(event) = read_json::<_, DesktopEvent>(&mut read).await {
            events.send_replace(Some(event));
        }
    });
    Ok(AgentRoot {
        desktop,
        reader,
        process,
        control,
        metadata,
        secret,
        identity,
    })
}
fn agent_acl(session: u32, user: bool) -> anyhow::Result<String> {
    if user {
        let token = native::token(session, true)?;
        Ok(format!(
            "D:P(A;;GA;;;SY)(A;;GA;;;{})",
            native::user_sid(token.0)?
        ))
    } else {
        Ok("D:P(A;;GA;;;SY)".into())
    }
}
#[cfg(test)]
fn root_channel_open(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> bool {
    // After bootstrap the root only receives requests; EOF or unexpected data
    // means it must never be handed to another remote session.
    matches!(pipe.try_read(&mut [0u8; 1]), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
}
struct MediaRetry {
    next: std::time::Instant,
    failures: u32,
}
impl MediaRetry {
    fn new(now: std::time::Instant) -> Self {
        Self {
            next: now,
            failures: 0,
        }
    }
    fn reset(&mut self, now: std::time::Instant) {
        *self = Self::new(now);
    }
    fn ready(&self, now: std::time::Instant) -> bool {
        now >= self.next
    }
    fn failed(&mut self, now: std::time::Instant) {
        self.next = now + Duration::from_millis(250 * (1u64 << self.failures.min(3)));
        self.failures = self.failures.saturating_add(1);
    }
}
async fn with_root(
    cfg: &crate::config::AppConfig,
    session: u32,
    user: bool,
) -> anyhow::Result<tokio::sync::MutexGuard<'static, AgentPool>> {
    let mut pool = POOL.get_or_init(Default::default).lock().await;
    let identity = native::session_identity(session)?;
    let key = AgentKey::new(cfg, session, user);
    pool.retain(|existing, root| {
        existing.session == session
            && root.process.running()
            && root.identity == identity
            && !root.reader.is_finished()
            && (*existing == AgentKey::new(cfg, session, existing.user)
                || Arc::strong_count(&root.process) > 1)
    });
    if let std::collections::hash_map::Entry::Vacant(entry) = pool.entry(key) {
        entry.insert(root(cfg, session, user).await?);
    }
    Ok(pool)
}
pub async fn metadata(cfg: &crate::config::AppConfig, session: u32) -> anyhow::Result<Metadata> {
    query_metadata(cfg, session, false).await
}

async fn query_metadata(
    cfg: &crate::config::AppConfig,
    session: u32,
    available_only: bool,
) -> anyhow::Result<Metadata> {
    let mut pool = with_root(cfg, session, false).await?;
    let root = pool.get_mut(&AgentKey::new(cfg, session, false)).unwrap();
    let name = format!(r"\\.\pipe\VntRemote.Session.{}", native::random()?);
    let mut pipe = native::pipe(&name, &agent_acl(session, false)?, true)?;
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        write_json(
            &mut root.control,
            &RootRequest::Open(OpenSession {
                effects: None,
                pipe_name: name,
                display: None,
                metadata: true,
                available_only,
                region: None,
            }),
        )
        .await?;
        pipe.connect().await?;
        anyhow::ensure!(
            native::client_pid(&pipe)? == root.process.pid,
            "元数据代理身份不匹配"
        );
        read_json::<_, Result<Metadata, String>>(&mut pipe)
            .await?
            .map_err(anyhow::Error::msg)
    })
    .await??;
    root.metadata = result.clone();
    Ok(result)
}
async fn worker(
    cfg: &crate::config::AppConfig,
    hello: &pb::Hello,
    session: u32,
    user: bool,
) -> anyhow::Result<Worker> {
    if !user {
        let _ = metadata(cfg, session).await?;
    }
    let name = format!(r"\\.\pipe\VntRemote.Session.{}", native::random()?);
    let pipe = native::pipe(&name, &agent_acl(session, user)?, true)?;
    let (process, metadata, secret, desktop) = {
        let mut pool = with_root(cfg, session, user).await?;
        let key = AgentKey::new(cfg, session, user);
        let root = pool.get_mut(&key).unwrap();
        let sent = tokio::time::timeout(
            Duration::from_secs(2),
            write_json(
                &mut root.control,
                &RootRequest::Open(OpenSession {
                    effects: None,
                    metadata: false,
                    available_only: false,
                    region: cfg.selected_region().map_err(anyhow::Error::msg)?,
                    pipe_name: name,
                    display: None,
                }),
            ),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            pool.remove(&key);
            anyhow::bail!("会话代理控制通道已关闭");
        }
        (
            root.process.clone(),
            root.metadata.clone(),
            root.secret.clone(),
            root.desktop.clone(),
        )
    };
    tokio::time::timeout(Duration::from_secs(12), pipe.connect()).await??;
    anyhow::ensure!(
        native::client_pid(&pipe)? == process.pid,
        "会话代理身份不匹配"
    );
    let mut channel = ControlChannel::new(pipe);
    let mut h = hello.clone();
    h.transport = vnt_proto::Transport::Tcp.to_proto() as i32;
    h.audio_enabled = user && hello.audio_enabled && cfg.audio;
    send(&mut channel, &ControlMessage::Hello(h)).await?;
    vnt_net::auth::respond(&mut channel, &secret).await?;
    if user {
        match tokio::time::timeout(Duration::from_secs(10), channel.recv()).await?? {
            Some(ControlMessage::Welcome(_)) => {}
            Some(ControlMessage::Denied(reason)) => anyhow::bail!("{}", reason.reason),
            _ => anyhow::bail!("用户代理未完成剪贴板初始化握手"),
        }
    }
    Ok(Worker {
        desktop,
        process,
        channel,
        metadata,
    })
}
async fn media(
    cfg: &crate::config::AppConfig,
    h: &pb::Hello,
    session: u32,
) -> anyhow::Result<(Worker, pb::Welcome)> {
    let mut w = worker(cfg, h, session, false).await?;
    let welcome = tokio::time::timeout(Duration::from_secs(15), w.channel.recv()).await??;
    match welcome {
        Some(ControlMessage::Welcome(v)) => Ok((w, v)),
        Some(ControlMessage::Denied(d)) => anyhow::bail!("{}", d.reason),
        _ => anyhow::bail!("采集代理未完成握手"),
    }
}
fn identity() -> anyhow::Result<Option<(u32, String)>> {
    let Some(session) = native::capture_session()? else {
        return Ok(None);
    };
    Ok(Some((session, native::session_identity(session)?)))
}
async fn receive_agent<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    channel: &mut ControlChannel<S>,
    events: &mut tokio::sync::watch::Receiver<Option<DesktopEvent>>,
    capacity: impl std::future::Future<Output = std::io::Result<()>>,
    root_events: bool,
) -> std::io::Result<Option<ControlMessage>> {
    tokio::select! {
        biased;
        changed = events.changed(), if root_events => {
            if changed.is_err() { return Ok(None); }
            let event = events.borrow_and_update().clone();
            Ok(event.map(|event| crate::server::agent_message(event.generation,
                pb::agent_message::Payload::Desktop(pb::AgentDesktop {
                    desktop: event.metadata.desktop, capture_ready: event.capture_ready, input_ready: false,
                    reason: event.reason, monitors_json: serde_json::to_vec(&event.metadata.monitors).unwrap_or_default(),
                }))))
        }
        received = async { capacity.await?; channel.recv().await } => received,
    }
}

#[derive(Default)]
struct AgentEpoch {
    generation: u64,
    configured: u64,
    input_ready: bool,
}
impl AgentEpoch {
    fn desktop(&mut self, generation: u64, input_ready: bool) -> Option<bool> {
        if generation == 0 || generation < self.generation {
            return None;
        }
        let changed = generation != self.generation;
        if changed {
            self.generation = generation;
            self.configured = 0;
            self.input_ready = false;
        }
        self.input_ready |= input_ready;
        Some(changed)
    }
    fn configure(&mut self, generation: u64) -> bool {
        if generation == 0 || generation != self.generation {
            return false;
        }
        self.configured = generation;
        true
    }
    fn accepts_video(&self, generation: u64) -> bool {
        generation != 0 && generation == self.generation && self.configured == generation
    }
}
fn desktop_state(ready: bool, desktop: &str, reason: &str, generation: u32) -> ControlMessage {
    ControlMessage::DesktopState(pb::DesktopState {
        min_pts_ms: 0,
        ready,
        desktop: desktop.into(),
        reason: reason.chars().take(250).collect(),
        generation,
    })
}
fn clipboard(m: &ControlMessage) -> bool {
    matches!(
        m,
        ControlMessage::ClipboardImageReady(_)
            | ControlMessage::ClipboardImage(_)
            | ControlMessage::ClipboardImageChunk(_)
            | ControlMessage::ClipboardImageCancel(_)
            | ControlMessage::ClipboardText(_)
            | ControlMessage::ClipboardPasteResult(_)
            | ControlMessage::ClipboardOffer(_)
            | ControlMessage::ClipboardRequest(_)
            | ControlMessage::ClipboardReply(_)
    )
}
// Only the latest publication can wait for initial user-agent startup. Never
// replay requests, file handles, or publications across desktop/user changes.
#[derive(Default)]
struct PendingClipboard {
    publication: Vec<ControlMessage>,
    image: Option<vnt_proto::clipboard::ImageAssembly>,
}
impl PendingClipboard {
    fn remember(&mut self, message: &ControlMessage) {
        match message {
            ControlMessage::ClipboardText(_) | ControlMessage::ClipboardOffer(_) => {
                self.clear();
                self.publication.push(message.clone());
            }
            ControlMessage::ClipboardImage(header) => {
                self.clear();
                self.image = vnt_proto::clipboard::ImageAssembly::new(*header);
            }
            ControlMessage::ClipboardImageCancel(cancel) => {
                if self
                    .image
                    .as_ref()
                    .is_some_and(|i| i.header.image_id == cancel.image_id)
                    || matches!(self.publication.first(), Some(ControlMessage::ClipboardImage(i)) if i.image_id == cancel.image_id)
                {
                    self.clear();
                }
            }
            ControlMessage::ClipboardImageChunk(chunk) => {
                let Some(image) = self.image.as_mut() else {
                    return;
                };
                if image.header.image_id != chunk.image_id {
                    return;
                }
                match image.push(chunk) {
                    Ok(false) => {}
                    Err(_) => {
                        self.clear();
                    }
                    Ok(true) => {
                        let image = self.image.take().unwrap();
                        let header = image.header;
                        let bytes = image.into_bytes();
                        self.publication
                            .push(ControlMessage::ClipboardImage(header));
                        for (i, data) in bytes
                            .chunks(vnt_proto::control::CLIPBOARD_CHUNK_BYTES)
                            .enumerate()
                        {
                            self.publication.push(ControlMessage::ClipboardImageChunk(
                                pb::ClipboardImageChunk {
                                    image_id: header.image_id,
                                    offset: (i * vnt_proto::control::CLIPBOARD_CHUNK_BYTES) as u32,
                                    data: data.to_vec(),
                                },
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    fn expire(&mut self) {
        if self.image.as_ref().is_some_and(|image| image.expired()) {
            self.clear();
        }
    }
    fn take(&mut self) -> Vec<ControlMessage> {
        self.expire();
        std::mem::take(&mut self.publication)
    }
    // A newly available receiver must see the descriptor immediately so it can
    // acknowledge readiness; waiting for all chunks here would deadlock the sender.
    fn take_for_receiver(&mut self) -> Vec<ControlMessage> {
        self.expire();
        if let Some(image) = self.image.take() {
            let header = image.header;
            let mut messages = vec![ControlMessage::ClipboardImage(header)];
            for (index, chunk) in image
                .into_bytes()
                .chunks(vnt_proto::control::CLIPBOARD_CHUNK_BYTES)
                .enumerate()
            {
                messages.push(ControlMessage::ClipboardImageChunk(
                    pb::ClipboardImageChunk {
                        image_id: header.image_id,
                        offset: (index * vnt_proto::control::CLIPBOARD_CHUNK_BYTES) as u32,
                        data: chunk.to_vec(),
                    },
                ));
            }
            return messages;
        }
        self.take()
    }
    fn clear(&mut self) {
        self.publication.clear();
        self.image = None;
    }
}
fn authorize_sas(
    allow_control: bool,
    session: Option<u32>,
    console: Option<u32>,
) -> anyhow::Result<u32> {
    anyhow::ensure!(allow_control, "观看模式不允许发送 Ctrl+Alt+Del");
    let session = session.ok_or_else(|| anyhow::anyhow!("暂无物理控制台会话"))?;
    anyhow::ensure!(
        Some(session) == console,
        "Ctrl+Alt+Del 仅支持物理控制台；当前被控会话已切换或为 RDP 会话"
    );
    Ok(session)
}
#[derive(Debug, PartialEq, Eq)]
enum SasObservation {
    Waiting,
    SecureDesktop,
    SessionChanged,
    Unconfirmed,
}
#[derive(Default)]
struct SasFeedback(Option<(u32, std::time::Instant)>);
impl SasFeedback {
    fn observe(
        &mut self,
        session: Option<u32>,
        desktop: Option<&str>,
        now: std::time::Instant,
    ) -> SasObservation {
        let Some((target, started)) = self.0 else {
            return SasObservation::Waiting;
        };
        let result = if session != Some(target) {
            SasObservation::SessionChanged
        } else if desktop.is_some_and(|name| name.eq_ignore_ascii_case("winlogon")) {
            SasObservation::SecureDesktop
        } else if now.saturating_duration_since(started) >= Duration::from_secs(8) {
            SasObservation::Unconfirmed
        } else {
            SasObservation::Waiting
        };
        if result != SasObservation::Waiting {
            self.0 = None;
        }
        result
    }
}
async fn session(
    mut remote: vnt_net::session::SessionChannel,
    peer: std::net::SocketAddr,
    udp: Arc<UdpSocket>,
    status: Arc<Mutex<Status>>,
    bindings: Arc<Mutex<std::collections::HashMap<u32, Binding>>>,
    cancel: CancellationToken,
    snapshot: Option<crate::config::AppConfig>,
) -> anyhow::Result<()> {
    let internal = snapshot.is_some();
    let Some(ControlMessage::Hello(hello)) =
        tokio::time::timeout(Duration::from_secs(10), remote.recv()).await??
    else {
        return Ok(());
    };
    if vnt_proto::system::from_wire(hello.system).is_err() {
        send(
            &mut remote,
            &ControlMessage::Denied(pb::Denied {
                reason: "主控系统信息缺失或无效，请同步升级双方程序".into(),
            }),
        )
        .await?;
        return Ok(());
    }
    if hello.proto_ver != vnt_proto::PROTO_VER || hello.desktop_effects.is_none() {
        send(
            &mut remote,
            &ControlMessage::Denied(pb::Denied {
                reason: "协议版本不匹配，请同步升级主控与被控".into(),
            }),
        )
        .await?;
        return Ok(());
    }
    let (nonce, proof) = vnt_net::auth::challenge(&mut remote).await?;
    let mut cfg = match snapshot.map(Ok).unwrap_or_else(super::store::load) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!("新连接读取被控配置失败：{error:#}");
            send(
                &mut remote,
                &ControlMessage::Denied(pb::Denied {
                    reason: "被控端无法读取配置，请检查配置文件".into(),
                }),
            )
            .await?;
            return Ok(());
        }
    };
    let transport = vnt_proto::Transport::from_wire(hello.transport);
    let denied = if !vnt_proto::auth::verify_remote(&cfg.password, &nonce, &proof) {
        Some("密码错误或被控端未配置访问密码")
    } else if vnt_proto::color_depth::from_wire(hello.color_depth).is_none()
        || hello.fec_version > vnt_net::fec::VERSION
        || (hello.transport == 2 && hello.fec_version != 0)
        || !(1..=120).contains(&hello.fps)
        || !(100..=200000).contains(&hello.bitrate_kbps)
        || transport.is_err()
        || !crate::resolution::validate(
            hello.resolution_mode,
            hello.target_width,
            hello.target_height,
        )
    {
        Some("连接参数无效")
    } else {
        None
    };
    if let Some(reason) = denied {
        send(
            &mut remote,
            &ControlMessage::Denied(pb::Denied {
                reason: reason.into(),
            }),
        )
        .await?;
        return Ok(());
    }
    let transport = transport.unwrap();
    let current = identity()?.ok_or_else(|| anyhow::anyhow!("暂无可采集的交互会话"))?;
    let metadata = match query_metadata(&cfg, current.0, !internal).await {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(session = current.0, error = %format!("{error:#}"), "获取可用屏幕失败");
            send(
                &mut remote,
                &ControlMessage::Denied(pb::Denied {
                    reason: format!("获取可用屏幕失败：{error:#}"),
                }),
            )
            .await?;
            return Ok(());
        }
    };
    // One lease per logical connection. Per-screen backend sessions and capture
    // restarts must not acquire or release the user's desktop settings.
    let _effects = if !internal && cfg.allow_control && !current.1.is_empty() {
        Some(effects::Lease::start(
            cfg.clone(),
            current.clone(),
            hello
                .desktop_effects
                .as_ref()
                .expect("validated desktop effects")
                .into(),
        ))
    } else {
        None
    };
    if !internal {
        return Box::pin(serve_screens(
            remote,
            peer,
            udp,
            status,
            bindings,
            cancel,
            cfg,
            hello,
            metadata.monitors,
        ))
        .await;
    }
    if internal {
        crate::screens::resolve_backend(&mut cfg, &metadata.monitors)
            .map_err(anyhow::Error::msg)?;
    }
    let region = match cfg.selected_region() {
        Ok(region) => region,
        Err(reason) => {
            send(&mut remote, &ControlMessage::Denied(pb::Denied { reason })).await?;
            return Ok(());
        }
    };
    if let Some(region) = &region {
        let error = if !metadata.desktop.eq_ignore_ascii_case("default") {
            Some("区域共享在锁屏或安全桌面暂停".to_string())
        } else {
            region.resolve(&metadata.monitors).err()
        };
        if let Some(reason) = error {
            send(&mut remote, &ControlMessage::Denied(pb::Denied { reason })).await?;
            return Ok(());
        }
    }
    let monitor = metadata
        .monitors
        .iter()
        .find(|m| {
            region.as_ref().map_or_else(
                || {
                    if internal {
                        m.index == cfg.monitor_index
                    } else {
                        m.primary
                    }
                },
                |r| r.matches(m),
            )
        })
        .or_else(|| metadata.monitors.iter().find(|m| m.primary))
        .or(metadata.monitors.first())
        .ok_or_else(|| anyhow::anyhow!("没有可用显示器"))?;
    let monitor = monitor.clone();
    cfg.monitor_index = monitor.index;
    let mut hello = hello;
    let mut resolution_reason = String::new();
    let wants_change = hello.resolution_mode == 2 && cfg.allow_control && region.is_none();
    let mut lease = match display::Lease::acquire(
        current.0,
        monitor.name.clone(),
        wants_change,
        cfg.clone(),
        status.clone(),
    ) {
        Ok(lease) => lease,
        Err(error) => {
            send(
                &mut remote,
                &ControlMessage::Denied(pb::Denied {
                    reason: error.to_string(),
                }),
            )
            .await?;
            return Ok(());
        }
    };
    let mut changed_display = false;
    if hello.resolution_mode == 2 {
        if region.is_some() {
            resolution_reason = "区域共享不修改显示器分辨率，已回退软件缩放".into();
        } else if !cfg.allow_control {
            resolution_reason = "仅观看模式无法修改被控分辨率，已回退软件缩放".into();
        } else if !lease.exclusive {
            resolution_reason = "已有其他主控连接，已回退软件缩放".into();
        } else if !metadata.desktop.eq_ignore_ascii_case("default") {
            resolution_reason = "安全桌面无法修改分辨率，已回退软件缩放".into();
        } else {
            match lease
                .apply(monitor.index, hello.target_width, hello.target_height)
                .await
            {
                Ok(()) => changed_display = true,
                Err(error) => {
                    // An acknowledgement can fail after applying the display mode.
                    // Restore before admitting fallback sessions.
                    lease.restore_now().await?;
                    resolution_reason = format!("修改被控分辨率失败，已回退软件缩放：{error:#}");
                }
            }
        }
        hello.resolution_mode = if changed_display { 0 } else { 1 };
        if !changed_display {
            lease.downgrade();
        }
    }
    let (first, mut welcome) = match media(&cfg, &hello, current.0).await {
        Ok(v) => v,
        Err(e) => {
            send(
                &mut remote,
                &ControlMessage::Denied(pb::Denied {
                    reason: format!("采集代理启动失败：{e}"),
                }),
            )
            .await?;
            return Ok(());
        }
    };
    welcome.host_edition = pb::HostEdition::Installed as i32;
    welcome.resolution_mode = if changed_display {
        2
    } else {
        hello.resolution_mode
    };
    welcome.resolution_reason = resolution_reason;
    hello.codecs = vec![welcome.codec];
    let target = Arc::new(RwLock::new(
        matches!(hello.transport, 3 | 4).then_some(peer),
    ));
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    let token = bytes.to_vec();
    let id = {
        let mut map = bindings.lock().unwrap();
        loop {
            let mut b = [0; 4];
            getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("{e}"))?;
            let id = u32::from_le_bytes(b).max(1);
            if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(id) {
                entry.insert(Binding {
                    native: hello.transport == 1,
                    token: token.clone(),
                    ip: peer.ip(),
                    target: target.clone(),
                });
                break id;
            }
        }
    };
    let _registration = Registration {
        id,
        status: status.clone(),
        bindings,
    };
    welcome.session_id = id;
    welcome.bind_token = if transport != vnt_proto::Transport::Tcp {
        token
    } else {
        Vec::new()
    };
    let fec =
        vnt_net::fec::Sender::negotiated(hello.fec_version, transport != vnt_proto::Transport::Tcp);
    welcome.fec_version = if fec.is_some() {
        vnt_net::fec::VERSION
    } else {
        0
    };
    welcome.transport = transport.to_proto() as i32;
    welcome.audio_enabled = cfg.audio && hello.audio_enabled;
    welcome.view_only = !cfg.allow_control;
    tracing::info!(session_id=id, view_only=welcome.view_only, desktop=%first.metadata.desktop, agent_pid=first.process.pid, "远程会话已认证");
    status
        .lock()
        .unwrap()
        .sessions
        .push(crate::server::SessionInfo {
            transport,
            session_id: id,
            peer: peer.to_string(),
            codec: vnt_proto::Codec::from_proto(pb::Codec::try_from(welcome.codec)?)
                .unwrap()
                .as_str()
                .into(),
            hardware: welcome.hardware,
            encoder_name: welcome.encoder_name.clone(),
            width: welcome.width,
            height: welcome.height,
            view_only: welcome.view_only,
            audio_enabled: welcome.audio_enabled,
        });
    send(&mut remote, &ControlMessage::Welcome(welcome)).await?;
    let mut desktop_events = first.desktop.clone();
    let mut media = Some(first);
    let mut user: Option<Worker> = None;
    let mut audio_user: Option<Worker> = None;
    let mut audio_retry = std::time::Instant::now() - Duration::from_secs(3);
    let (file_tx, mut file_rx) = tokio::sync::mpsc::channel::<(u32, ControlMessage)>(8);
    let mut file_tasks = tokio::task::JoinSet::new();
    let mut file_writer = remote.fork_writer().expect("managed remote session");
    let file_epoch = Arc::new(std::sync::atomic::AtomicU32::new(1));
    let writer_epoch = file_epoch.clone();
    file_tasks.spawn(async move {
        while let Some((epoch, msg)) = file_rx.recv().await {
            if epoch != writer_epoch.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            if file_writer.send_owned(msg).await.is_err() {
                break;
            }
        }
    });
    let mut pending_clipboard = PendingClipboard::default();
    let mut current = Some(current);
    let mut generation = 1;
    let mut background = false;
    let mut agent_epoch = AgentEpoch::default();
    let mut desktop_switch_started = std::time::Instant::now();
    let mut audio_generation = 1u32;
    let mut audio_source = None;
    let mut ready = false;
    let mut video_closed = false;
    let mut control_online = true;
    let mut udp_config_pending = false;
    let mut input_received = false;
    let mut seq = 0u16;
    let mut video_epoch = vnt_proto::recovery::EpochMapper::default();
    let mut video_recovery = VideoRecovery::default();
    let mut video_send_drops = 0u64;
    let mut video_bitrate_kbps = hello.bitrate_kbps;
    let (video_repair, _repair_task) = if transport != vnt_proto::Transport::Tcp {
        let (repair, task) = vnt_net::repair::VideoRepair::start(udp.clone(), target.clone());
        (Some(repair), Some(task))
    } else {
        (None, None)
    };
    let mut udp_video = (transport != vnt_proto::Transport::Tcp)
        .then(|| VideoSender::with_repair(udp.clone(), id, fec.clone(), video_repair.clone()));
    let start = std::time::Instant::now();
    let mut last_activity = std::time::Instant::now();
    let mut last_video_stats = std::time::Instant::now();
    let mut last_remote_video = (0u64, 0u64);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut media_retry = MediaRetry::new(std::time::Instant::now());
    let mut udp_bound = false;
    let mut recovery = RecoveryTasks::default();
    let mut user_retry = std::time::Instant::now() - Duration::from_secs(3);
    let mut last_sas = std::time::Instant::now() - Duration::from_secs(2);
    let mut sas_feedback = SasFeedback::default();
    send(
        &mut remote,
        &desktop_state(false, "", "正在连接桌面", generation),
    )
    .await?;
    loop {
        let video_capacity = udp_video.as_ref().map(VideoSender::capacity_waiter);
        tokio::select! {
            _=cancel.cancelled()=>{
                let _ = send(
                    &mut remote,
                    &ControlMessage::SessionEnd(pb::SessionEnd {
                        reason: "被控服务已停止".into(),
                    }),
                )
                .await;
                break;
            },
            msg=remote.recv()=>{
                let Some(msg) = msg? else { break };
                last_activity = std::time::Instant::now();
                match msg {
                    ControlMessage::Ping(p) => {
                        send(
                            &mut remote,
                            &ControlMessage::Pong(pb::Pong {
                                timestamp_ms: p.timestamp_ms,
                            }),
                        )
                        .await?;
                        if let Some(w) = media.as_mut() {
                            if send(&mut w.channel, &ControlMessage::Ping(p))
                                .await
                                .is_err()
                            {
                                media = None;
                            }
                        }
                        if let Some(w) = audio_user.as_mut() { let _ = send(&mut w.channel, &ControlMessage::Ping(p)).await; }
                        if let Some(w) = user.as_mut() {
                            if send(&mut w.channel, &ControlMessage::Ping(p))
                                .await
                                .is_err()
                            {
                                user = None;
                            }
                        }
                    }
                    ControlMessage::TransportState(state) => {
                        if state.kind == 0 {
                            control_online = state.available;
                            if state.available { udp_config_pending = false; }
                        }
                        if state.kind == 0 && !state.available {
                            if let Some(w) = media.as_mut() {
                                send(&mut w.channel, &ControlMessage::ResetInput(pb::ResetInput {})).await?;
                            }
                        }
                        if state.kind == 0 || state.kind == 1 {
                            if let Some(w) = user.as_mut() { send(&mut w.channel, &ControlMessage::TransportState(state)).await?; }
                        }
                    }
                    ControlMessage::SessionEnd(_) => break,
                    ControlMessage::SendSas => {
                        let authorized = authorize_sas(cfg.allow_control, current.as_ref().map(|v| v.0), native::console_session());
                        let result = match authorized {
                            Err(error) => Err(error),
                            Ok(_) if last_sas.elapsed() < Duration::from_secs(1) => Err(anyhow::anyhow!("操作过于频繁，请稍后重试")),
                            Ok(target) => {
                                last_sas = std::time::Instant::now();
                                super::service::sas(target).await.map(|_| target)
                            }
                        };
                        let reason = match result {
                            Ok(target) => {
                                sas_feedback.0 = Some((target, std::time::Instant::now()));
                                String::new()
                            }
                            Err(error) => {
                                let reason = format!("Ctrl+Alt+Del 不可用：{error:#}");
                                tracing::warn!(session_id=id, "{reason}");
                                reason
                            }
                        };
                        let desktop = media.as_ref().map(|w| w.metadata.desktop.as_str()).unwrap_or("");
                        send(&mut remote, &desktop_state(ready, desktop, &reason, generation)).await?;
                    }
                    ControlMessage::ClipboardPaste(request) => {
                        // Commands are never remembered/replayed when the user agent reconnects.
                        let message=ControlMessage::ClipboardPaste(request.clone());
                        let available=cfg.allow_control && cfg.clipboard && ready && !video_closed
                            && media.as_ref().is_some_and(|w|w.metadata.desktop.eq_ignore_ascii_case("default"));
                        let delivered=if available {
                            if let Some(u)=user.as_mut() {send(&mut u.channel,&message).await.is_ok()} else {false}
                        } else {false};
                        if !delivered {send(&mut remote,&ControlMessage::ClipboardPasteResult(pb::ClipboardPasteResult{
                            request_id:request.request_id,success:false,reason:"当前桌面不可用或不允许粘贴".into(),
                        })).await?;}
                    }
                    ref msg if clipboard(msg) && cfg.allow_control && cfg.clipboard => {
                        if (ready || video_closed) && media.as_ref().is_some_and(|w| w.metadata.desktop.eq_ignore_ascii_case("default")) {
                            if pending_clipboard.image.is_some() && matches!(msg,
                                ControlMessage::ClipboardImage(_) | ControlMessage::ClipboardImageChunk(_) | ControlMessage::ClipboardImageCancel(_)
                                | ControlMessage::ClipboardText(_) | ControlMessage::ClipboardOffer(_)) {
                                pending_clipboard.remember(msg);
                                if let Some(u) = user.as_mut() {
                                    let publications = pending_clipboard.take_for_receiver();
                                    for publication in &publications {
                                        if send(&mut u.channel, publication).await.is_err() {
                                            user = None;
                                            for publication in &publications { pending_clipboard.remember(publication); }
                                            break;
                                        }
                                    }
                                }
                            } else if let Some(u) = user.as_mut() {
                                if send(&mut u.channel, msg).await.is_err() {
                                    user = None;
                                    pending_clipboard.remember(msg);
                                }
                            } else {
                                pending_clipboard.remember(msg);
                            }
                        }
                    }
                    ControlMessage::StopVideo(stop) if !stop.acknowledged => {
                        video_closed=true;
                        if let Some(w)=media.as_mut(){send(&mut w.channel,&ControlMessage::StopVideo(stop)).await?;}
                    }
                    ControlMessage::VideoNack(nack) => {
                        if nack.stream_id == 0 { if let Some(repair) = &video_repair { repair.request(nack); } }
                    }
                    ControlMessage::ResetInput(reset) => {
                        if let Some(u)=user.as_mut(){let _=send(&mut u.channel,&ControlMessage::ResetInput(reset.clone())).await;}
                        if let Some(w)=media.as_mut(){send(&mut w.channel,&ControlMessage::ResetInput(reset)).await?;}
                    }
                    ControlMessage::ScreenThrottle(state) => {
                        background = state.background;
                        if let Some(w) = media.as_mut() {
                            if send(&mut w.channel, &ControlMessage::ScreenThrottle(state)).await.is_err() {
                                media = None;
                            }
                        }
                    }
                    ControlMessage::InputEvent(_) if !cfg.allow_control => {}
                    msg @ (ControlMessage::InputEvent(_)
                    | ControlMessage::RequestKeyframe
                    | ControlMessage::LinkFeedback(_)) => {
                        if let ControlMessage::LinkFeedback(report)=&msg {if let Some(fec)=&fec{fec.feedback(report);}}
                        if let Some(w) = media.as_mut() {
                            if ready && matches!(msg, ControlMessage::InputEvent(_)) && !input_received {
                                input_received = true;
                                tracing::info!(session_id=id, agent_pid=w.process.pid, desktop=%w.metadata.desktop, "已收到主控键鼠输入，正在转发至会话代理");
                            }
                            let message = if let ControlMessage::InputEvent(event) = msg {
                                if !ready || (agent_epoch.generation != 0 && !agent_epoch.input_ready) {
                                    continue;
                                }
                                if agent_epoch.generation == 0 {
                                    ControlMessage::InputEvent(event)
                                } else {
                                    crate::server::agent_message(
                                        agent_epoch.generation,
                                        pb::agent_message::Payload::Input(event),
                                    )
                                }
                            } else {
                                msg
                            };
                            if send(&mut w.channel, &message).await.is_err() {
                                media = None;
                            }
                        }
                    }
                    _ => {}
                }
            },
            received=async{
                receive_agent(&mut media.as_mut().unwrap().channel, &mut desktop_events, async {
                    if let Some(ready) = video_capacity { ready.await?; }
                    Ok(())
                }, region.is_none()).await
            },if media.is_some()=>{
                let received = match received {
                    Ok(Some(ControlMessage::Agent(message))) => {
                        use pb::agent_message::Payload;
                        match message.payload {
                            Some(Payload::Desktop(state)) => {
                                let Some(changed) = agent_epoch.desktop(message.generation, state.input_ready)
                                else {
                                    continue;
                                };
                                if changed {
                                    desktop_switch_started = std::time::Instant::now();
                                    generation = generation.wrapping_add(1).max(1);
                                    file_epoch.store(generation, std::sync::atomic::Ordering::Release);
                                    ready = false;
                                    user = None;
                                    audio_user = None;
                                    if let Some(t) = recovery.user.take() {
                                        t.abort();
                                    }
                                    if let Some(t) = recovery.audio.take() {
                                        t.abort();
                                    }
                                    pending_clipboard.clear();
                                    if let Some(sender) = udp_video.as_mut() {
                                        sender.reset(udp.clone(), id).await;
                                    }
                                    video_recovery.dropped();
                                    send(
                                        &mut remote,
                                        &desktop_state(
                                            false,
                                            &state.desktop,
                                            if state.reason.is_empty() { "桌面切换中，正在恢复画面" } else { &state.reason },
                                            generation,
                                        ),
                                    )
                                    .await?;
                                    send(
                                        &mut remote,
                                        &ControlMessage::AudioState(pb::AudioState {
                                            generation: audio_generation,
                                            available: false,
                                            reason: "桌面切换中".into(),
                                        }),
                                    )
                                    .await?;
                                    tracing::info!(session_id=id,agent_pid=media.as_ref().unwrap().process.pid,agent_generation=agent_epoch.generation,desktop=%state.desktop,"代理进程内切换桌面");
                                }
                                if let Some(w) = media.as_mut() {
                                    w.metadata.desktop = state.desktop;
                                    if let Ok(monitors) = serde_json::from_slice::<Vec<vnt_capture::MonitorInfo>>(
                                        &state.monitors_json,
                                    ) {
                                        w.metadata.monitors = monitors;
                                    }
                                }
                                continue;
                            }
                            Some(Payload::Config(config)) if agent_epoch.configure(message.generation) => {
                                Ok(Some(ControlMessage::StreamReset(config)))
                            }
                            Some(Payload::Video(frame)) if agent_epoch.accepts_video(message.generation) => {
                                Ok(Some(ControlMessage::VideoFrame(frame)))
                            }
                            Some(Payload::InputFeedback(state))
                                if message.generation == agent_epoch.generation =>
                            {
                                Ok(Some(ControlMessage::DesktopState(state)))
                            }
                            _ => continue,
                        }
                    }
                    other => other,
                };
                match received {
                    Ok(Some(ControlMessage::VideoFrame(mut frame))) => {
                        if let Some(d) = &mut frame.dependencies { video_epoch.map(generation as u64, d); }
                        if udp_config_pending { continue; }
                        if transport != vnt_proto::Transport::Tcp && frame.dependencies.is_none() && !video_recovery.accepts(frame.keyframe) {
                            continue;
                        }
                        if !ready && !frame.keyframe {
                            continue;
                        }
                        let desktop = media.as_ref().unwrap().metadata.desktop.clone();
                        frame.pts_ms = start.elapsed().as_millis().min(u32::MAX as u128) as u32;
                        if !ready {
                            ready = true;
                            let mut state=desktop_state(true,&desktop,"",generation);
                            if let ControlMessage::DesktopState(state)=&mut state {state.min_pts_ms=frame.pts_ms;}
                            send(&mut remote,&state).await?;
                            tracing::info!(session_id=id, agent_pid=media.as_ref().unwrap().process.pid, agent_generation=agent_epoch.generation,
                                elapsed_ms=desktop_switch_started.elapsed().as_millis(), "新桌面首个关键帧恢复");
                        }
                        if transport == vnt_proto::Transport::Tcp {
                            remote.send_owned(ControlMessage::VideoFrame(frame)).await?;
                        } else if let Some(addr) = *target.read().await {
                            let frame_seq = seq;
                            seq = seq.wrapping_add(1);
                            if !udp_video.as_ref().unwrap().try_send(
                                frame, addr, frame_seq, video_bitrate_kbps.saturating_mul(2),
                            ) {
                                video_recovery.dropped();
                                video_send_drops += 1;
                                if video_send_drops.is_power_of_two() {
                                    tracing::warn!(session_id=id, frame_seq, video_send_drops,
                                        "UDP 视频发送队列已满，丢弃本帧并等待关键帧");
                                }
                            }
                        }
                    }
                    Ok(Some(ControlMessage::CursorPosition(cursor))) => {send(&mut remote,&ControlMessage::CursorPosition(cursor)).await?;}
                    Ok(Some(ControlMessage::StopVideo(stop))) => {
                        send(&mut remote,&ControlMessage::StopVideo(stop)).await?;
                    }
                    Ok(Some(ControlMessage::StatsReport(mut report))) => {
                        if report.applied_bitrate_kbps > 0 {
                            video_bitrate_kbps = report.applied_bitrate_kbps;
                        }
                        let (frames, bytes) = if let Some(sender) = &udp_video {
                            std::mem::take(&mut *sender.sent.lock().unwrap())
                        } else {
                            let current = remote.traffic().video_snapshot();
                            let delta = (current.0.wrapping_sub(last_remote_video.0), current.1.wrapping_sub(last_remote_video.1));
                            last_remote_video = current;
                            delta
                        };
                        let elapsed = last_video_stats.elapsed().as_secs_f64().max(0.001);
                        report.fps = (frames as f64 / elapsed) as u32;
                        report.bitrate_kbps = (bytes as f64 * 8.0 / 1000.0 / elapsed) as u32;
                        last_video_stats = std::time::Instant::now();
                        send(&mut remote, &ControlMessage::StatsReport(report)).await?
                    }
                    Ok(Some(msg @ ControlMessage::StreamReset(_))) => {
                        if !control_online && transport != vnt_proto::Transport::Tcp { udp_config_pending = true; }
                        if let Some(sender) = udp_video.as_mut() {
                            sender.reset(udp.clone(), id).await;
                            video_recovery.dropped();
                        }
                        send(&mut remote, &msg).await?
                    }
                    Ok(Some(ControlMessage::DesktopState(state))) => {
                        if state.reason.is_empty() {
                            tracing::info!(session_id=id, "会话代理已成功注入键鼠输入");
                        } else {
                            tracing::warn!(session_id=id, reason=%state.reason, "会话代理输入异常");
                        }
                        if ready {
                            let desktop = &media.as_ref().unwrap().metadata.desktop;
                            send(&mut remote, &desktop_state(true, desktop, &state.reason, generation)).await?;
                        }
                    }
                    Ok(Some(ControlMessage::SessionEnd(end))) => {
                        tracing::warn!(session_id=id, reason=%end.reason, "采集/输入代理会话结束");
                        send(&mut remote, &desktop_state(false, "", &end.reason, generation)).await?;
                        media = None;
                    }
                    Ok(None) | Err(_) => {
                        media = None;
                    }
                    _ => {}
                }
            },
            received=async{audio_user.as_mut().unwrap().channel.recv().await},if audio_user.is_some()=>{
                match received {
                    Ok(Some(ControlMessage::AudioFrame(mut frame))) => {
                        frame.generation = audio_generation;
                        if transport != vnt_proto::Transport::Tcp {
                            if let Some(addr) = *target.read().await {
                                let _=tokio::time::timeout(Duration::from_millis(10),vnt_net::udp::send_audio(&udp,addr,id,&frame,fec.as_ref())).await;
                            }
                        } else {
                            remote.send_owned(ControlMessage::AudioFrame(frame)).await?
                        }
                    }
                    Ok(Some(ControlMessage::AudioState(mut s))) => {
                        if audio_source != Some(s.generation) {
                            audio_source = Some(s.generation);
                            audio_generation = audio_generation.wrapping_add(1).max(1);
                        }
                        s.generation = audio_generation;
                        send(&mut remote, &ControlMessage::AudioState(s)).await?
                    }
                    Ok(None) | Err(_) => audio_user = None,
                    _ => {}
                }
            },
            received=async{user.as_mut().unwrap().channel.recv().await},if user.is_some() && file_tx.capacity() > 0=>{
                match received {
                    Ok(Some(msg @ (ControlMessage::ClipboardReply(_) | ControlMessage::ClipboardImageChunk(_)))) if cfg.allow_control => {
                                       file_tx.try_send((generation,msg)).map_err(|_| anyhow::anyhow!("文件转发任务已停止"))?;
                    }
                    Ok(Some(msg)) if clipboard(&msg) && cfg.allow_control => send(&mut remote, &msg).await?,
                    Ok(None) | Err(_) => user = None,
                    _ => {}
                }
            },
            result=async { recovery.media.as_mut().unwrap().await }, if recovery.media.is_some()=>{
                recovery.media = None;
                match result {
                    Ok(Ok((mut w, wel))) => {
                        if !control_online && transport != vnt_proto::Transport::Tcp { udp_config_pending = true; }
                        media_retry.reset(std::time::Instant::now());
                        if video_closed {send(&mut w.channel,&ControlMessage::StopVideo(pb::StopVideo{acknowledged:false})).await?;}
                        let synchronized = async {
                            send(&mut w.channel, &ControlMessage::ScreenThrottle(pb::ScreenThrottle { background })).await?;
                            send(&mut w.channel, &ControlMessage::RequestKeyframe).await
                        }.await;
                        if let Err(error) = synchronized {
                            media_retry.failed(std::time::Instant::now());
                            tracing::warn!("新桌面代理尚未就绪，将重试：{error}");
                            continue;
                        }
                        if let Some(sender) = udp_video.as_mut() {
                            sender.reset(udp.clone(), id).await;
                            video_recovery.dropped();
                        }
                        send(
                            &mut remote,
                            &ControlMessage::StreamReset(pb::StreamReset {
                                width: wel.width,
                                height: wel.height,
                                fps: wel.fps,
                                decoder_config: wel.decoder_config,
                            }),
                        )
                        .await?;
                        agent_epoch=AgentEpoch::default();
                        desktop_events = w.desktop.clone();
                        media = Some(w);
                    }
                    result => {
                        media_retry.failed(std::time::Instant::now());
                        let reason = match result {
                            Ok(Err(e)) => e.to_string(),
                            Err(e) => e.to_string(),
                            _ => unreachable!(),
                        };
                        send(
                            &mut remote,
                            &desktop_state(false, "", &format!("等待桌面：{reason}"), generation),
                        )
                        .await?;
                    }
                }
            },
            result=async { recovery.user.as_mut().unwrap().await }, if recovery.user.is_some()=>{
                recovery.user = None;
                match result {
                    Ok(Ok(mut w)) => {
                        let publications = pending_clipboard.take_for_receiver();
                        let mut replay_failed = false;
                        for publication in &publications {
                            if let Err(error) = send(&mut w.channel, publication).await {
                                tracing::warn!(session_id=id, "用户代理恢复剪贴板失败，将重试：{error}");
                                replay_failed = true;
                                break;
                            }
                        }
                        if replay_failed {
                            for publication in &publications { pending_clipboard.remember(publication); }
                            continue;
                        }
                        send(&mut remote, &desktop_state(ready, &w.metadata.desktop, "", generation)).await?;
                        tracing::info!(session_id=id, agent_pid=w.process.pid, "用户代理已就绪，剪贴板通道已连接");
                        user = Some(w)
                    }
                    result => {
                        let reason = match result {
                            Ok(Err(e)) => format!("用户代理不可用，剪贴板/声音暂不可用：{e:#}"),
                            Err(e) => format!("用户代理任务失败：{e}"),
                            _ => unreachable!(),
                        };
                        tracing::warn!(session_id=id, "{reason}");
                        send(&mut remote, &desktop_state(ready, "", &reason, generation)).await?;
                    }
                }
            },
            result=async { recovery.audio.as_mut().unwrap().await }, if recovery.audio.is_some()=>{
                recovery.audio = None;
                match result {
                    Ok(Ok(worker)) => { audio_source = None; audio_user = Some(worker); }
                    _ => { audio_user = None; }
                }
            },
            event=async { udp_video.as_mut().unwrap().events.recv().await }, if udp_video.is_some()=>{
                match event {
                    Some(Ok(true)) => video_recovery.delivered(true),
                    Some(Ok(false)) => video_recovery.dropped(),
                    Some(Err(error)) => return Err(error).context("向主控发送 UDP 视频帧失败"),
                    None => anyhow::bail!("UDP 视频发送任务已结束"),
                }
            },
            _=tick.tick()=>{
                if video_recovery.request_due(std::time::Instant::now()) {
                    if let Some(w) = media.as_mut() {
                        if let Err(error) = send(&mut w.channel, &ControlMessage::RequestKeyframe).await {
                            tracing::warn!(session_id=id, "视频恢复请求无法送达代理，将重建代理通道：{error:#}");
                            media = None;
                        }
                    }
                }
                if last_activity.elapsed() > Duration::from_secs(30) {
                    break;
                }
                if transport != vnt_proto::Transport::Tcp && !udp_bound {
                    if target.read().await.is_some() {
                        udp_bound = true;
                        send(
                            &mut remote,
                            &ControlMessage::UdpBindAck(pb::UdpBindAck { session_id: id }),
                        )
                        .await?;
                    } else if start.elapsed() > Duration::from_secs(10) {
                        break;
                    }
                }
                // Do not keep capturing an old user's desktop when session
                // selection fails (for example, multiple RDP logins).
                let next = identity().unwrap_or(None);
                let alive = media.as_ref().is_some_and(|w| w.process.running());
                if next != current || !alive {
                    if next != current {
                        media_retry.reset(std::time::Instant::now());
                        pending_clipboard.clear();
                        if let Some(t) = recovery.media.take() {
                            t.abort();
                        }
                        if let Some(t) = recovery.user.take() {
                            t.abort();
                        }
                    }
                    if ready || media.is_some() {
                        media_retry.reset(std::time::Instant::now());
                        if let Some(t) = recovery.user.take() {
                            t.abort();
                        }
                        generation = generation.wrapping_add(1).max(1);
                        file_epoch.store(generation,std::sync::atomic::Ordering::Release);
                        ready = false;
                        media = None;
                        user = None;
                        audio_user = None;
                        if let Some(t) = recovery.audio.take() { t.abort(); }
                        if let Some(sender) = udp_video.as_mut() {
                            sender.reset(udp.clone(), id).await;
                            video_recovery.dropped();
                        }
                        pending_clipboard.clear();
                        send(
                            &mut remote,
                            &desktop_state(false, "", "桌面切换中，正在恢复画面", generation),
                        )
                        .await?;
                        send(
                            &mut remote,
                            &ControlMessage::AudioState(pb::AudioState {
                                generation: audio_generation,
                                available: false,
                                reason: "桌面切换中".into(),
                            }),
                        )
                        .await?;
                    }
                    current = next;
                    if recovery.media.is_none() && media_retry.ready(std::time::Instant::now()) {
                        if let Some((session, _)) = &current {
                            let session = *session;
                            let config = cfg.clone();
                            let hello = hello.clone();
                            recovery.media = Some(tokio::spawn(async move {
                                self::media(&config, &hello, session).await
                            }));
                        }
                    }
                }
                if let Some(w) = &media {
                    let mut s = status.lock().unwrap();
                    s.console_session = current.as_ref().map(|v| v.0);
                    s.desktop = w.metadata.desktop.clone();
                    s.monitors = w.metadata.monitors.clone();
                }
                let sas_desktop = media.as_ref().filter(|w| w.process.running()).map(|w| w.metadata.desktop.as_str());
                let sas_reason = match sas_feedback.observe(current.as_ref().map(|v| v.0), sas_desktop, std::time::Instant::now()) {
                    SasObservation::Waiting => None,
                    SasObservation::SecureDesktop => {
                        tracing::info!(session_id=id, "Ctrl+Alt+Del 后已观察到 Windows 安全桌面");
                        Some("")
                    }
                    SasObservation::SessionChanged => Some("发送 Ctrl+Alt+Del 时控制台会话已变化，请重试"),
                    SasObservation::Unconfirmed => Some("Ctrl+Alt+Del 请求已发送，但未检测到 Windows 安全界面；请检查被控端系统策略或服务日志"),
                };
                if let Some(reason) = sas_reason {
                    send(&mut remote, &desktop_state(ready, sas_desktop.unwrap_or(""), reason, generation)).await?;
                }
                pending_clipboard.expire();
                if user.as_ref().is_some_and(|u| !u.process.running()) {
                    user = None;
                }
                if audio_user.as_ref().is_some_and(|w| !w.process.running()) { audio_user = None; }
                if audio_user.is_none() && recovery.audio.is_none() && cfg.audio && hello.audio_enabled
                    && audio_retry.elapsed() >= Duration::from_secs(3) && (ready || video_closed)
                    && media.as_ref().is_some_and(|w| w.metadata.desktop.eq_ignore_ascii_case("default"))
                    && current.as_ref().is_some_and(|v| !v.1.is_empty()) {
                    audio_retry = std::time::Instant::now();
                    let session = current.as_ref().unwrap().0;
                    let config = cfg.clone(); let hello = hello.clone();
                    recovery.audio = Some(tokio::spawn(async move { worker(&config, &hello, session, true).await }));
                }
                if user.is_none()
                    && recovery.user.is_none()
                    && user_retry.elapsed() >= Duration::from_secs(3)
                    && (ready || video_closed)
                    && media
                        .as_ref()
                        .is_some_and(|w| w.metadata.desktop.eq_ignore_ascii_case("default"))
                    && cfg.clipboard && cfg.allow_control
                    && current.as_ref().is_some_and(|v| !v.1.is_empty())
                {
                    user_retry = std::time::Instant::now();
                    if let Some((session, _)) = &current {
                        let session = *session;
                        let config = cfg.clone();
                        let mut hello = hello.clone();
                        hello.audio_enabled = false;
                        recovery.user = Some(tokio::spawn(async move {
                            worker(&config, &hello, session, true).await
                        }));
                    }
                }
            }
        }
    }
    // RAII also covers every early return; this path can still report failures.
    if let Err(error) = lease.restore_now().await {
        let reason = format!("恢复被控分辨率失败：{error:#}");
        tracing::warn!("{reason}");
        let _ = send(
            &mut remote,
            &ControlMessage::SessionEnd(pb::SessionEnd { reason }),
        )
        .await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn serve_screens(
    mut remote: vnt_net::session::SessionChannel,
    peer: std::net::SocketAddr,
    udp: Arc<UdpSocket>,
    status: Arc<Mutex<Status>>,
    bindings: Arc<Mutex<std::collections::HashMap<u32, Binding>>>,
    cancel: CancellationToken,
    cfg: crate::config::AppConfig,
    hello: pb::Hello,
    monitors: Vec<vnt_capture::MonitorInfo>,
) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
        let targets = match crate::screens::targets(&cfg, &monitors) {
            Ok(t) => t,
            Err(reason) => {
                send(&mut remote, &ControlMessage::Denied(pb::Denied { reason })).await?;
                return Ok(());
            }
        };
        if targets.iter().all(|t| !t.info.available) {
            send(
                &mut remote,
                &ControlMessage::Denied(pb::Denied {
                    reason: "所有采集目标均不可用".into(),
                }),
            )
            .await?;
            return Ok(());
        }
        let target = Arc::new(RwLock::new(
            matches!(hello.transport, 3 | 4).then_some(peer),
        ));
        let mut token = vec![0; 16];
        getrandom::fill(&mut token).map_err(|e| anyhow::anyhow!("{e}"))?;
        let id = {
            let mut map = bindings.lock().unwrap();
            loop {
                let mut b = [0; 4];
                getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("{e}"))?;
                let id = u32::from_be_bytes(b).max(1);
                if let std::collections::hash_map::Entry::Vacant(e) = map.entry(id) {
                    e.insert(Binding {
                        native: hello.transport == 1,
                        token: token.clone(),
                        ip: peer.ip(),
                        target: target.clone(),
                    });
                    break id;
                }
            }
        };
        let _registration = Registration {
            id,
            status: status.clone(),
            bindings,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(ControlMessage::Welcome(pb::Welcome {
            system: vnt_proto::system::local() as i32,
            bind_token: token,
            ..Default::default()
        }))
        .await?;
        let open_udp = udp.clone();
        let open_cancel = cancel.clone();
        let open: crate::screens::Open = Arc::new(move |cfg, hello| {
            let udp = open_udp.clone();
            let cancel = open_cancel.clone();
            Box::pin(async move {
                let (local, remote) = vnt_net::session::local_pair();
                let password = cfg.password.clone();
                let task = tokio::spawn(async move {
                    let _ = Box::pin(session(
                        remote,
                        peer,
                        udp,
                        Default::default(),
                        Default::default(),
                        cancel,
                        Some(cfg),
                    ))
                    .await;
                });
                crate::screens::handshake(local, hello, &password, task).await
            })
        });
        let session_status = status.clone();
        let started = Arc::new(move |welcome: &pb::Welcome| {
            session_status
                .lock()
                .unwrap()
                .sessions
                .push(crate::server::SessionInfo {
                    transport: vnt_proto::Transport::from_wire(welcome.transport)
                        .unwrap_or(vnt_proto::Transport::Tcp),
                    session_id: id,
                    peer: peer.to_string(),
                    codec: vnt_proto::Codec::from_proto(welcome.codec())
                        .map(|c| c.as_str())
                        .unwrap_or("h264")
                        .into(),
                    hardware: welcome.hardware,
                    encoder_name: welcome.encoder_name.clone(),
                    width: welcome.width,
                    height: welcome.height,
                    view_only: welcome.view_only,
                    audio_enabled: welcome.audio_enabled,
                });
        });
        let transport = crate::screens::Transport {
            session_id: id,
            udp: (hello.transport != 2).then_some(udp),
            peer: target,
            updates: rx,
            cancel,
            started,
        };
        // Keep the update lane alive for the lifetime of the logical session.
        let result = crate::screens::serve(remote, hello, targets, open, transport).await;
        drop(tx);
        result.map_err(anyhow::Error::msg)
    })
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    #[test]
    fn pooled_capture_is_bound_to_device_name_across_monitor_reordering() {
        use crate::config::{AppConfig, CaptureTarget};
        let mut cfg = AppConfig {
            selected_targets: vec![CaptureTarget::Monitor {
                name: "display-a".into(),
            }],
            ..Default::default()
        };
        let original = AgentKey::new(&cfg, 1, false);
        cfg.monitor_index = 2;
        assert!(original == AgentKey::new(&cfg, 1, false));
        cfg.selected_targets = vec![CaptureTarget::Monitor {
            name: "display-b".into(),
        }];
        cfg.monitor_index = 0;
        assert!(original != AgentKey::new(&cfg, 1, false));
    }
    #[test]
    fn desktop_rebuild_starts_immediately_and_only_failures_trigger_bounded_backoff() {
        let start = std::time::Instant::now();
        let mut retry = MediaRetry::new(start);
        assert!(retry.ready(start));
        for delay in [250, 500, 1000, 2000, 2000] {
            retry.failed(start);
            assert!(!retry.ready(start + Duration::from_millis(delay - 1)));
            assert!(retry.ready(start + Duration::from_millis(delay)));
        }
        retry.reset(start);
        assert!(retry.ready(start));
        retry.failed(start);
        assert!(retry.ready(start + Duration::from_millis(250)));
    }
    #[tokio::test]
    async fn closing_agent_admission_retires_the_root_before_process_cleanup_finishes() {
        let name = format!(
            r"\\.\pipe\VntRemote.RootRetireTest.{}",
            native::random().unwrap()
        );
        let pipe = native::pipe(&name, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;BU)", true).unwrap();
        let client = native::connect(&name).await.unwrap();
        pipe.connect().await.unwrap();
        assert!(root_channel_open(&pipe));
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), async {
            while root_channel_open(&pipe) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    #[test]
    fn sas_authorization_rejects_viewers_and_stale_sessions() {
        assert!(authorize_sas(false, Some(1), Some(1)).is_err());
        assert!(authorize_sas(true, None, None).is_err());
        assert!(authorize_sas(true, Some(1), Some(2)).is_err());
        assert_eq!(authorize_sas(true, Some(1), Some(1)).unwrap(), 1);
    }
    #[test]
    fn sas_feedback_requires_observed_secure_desktop_and_reports_timeout_or_session_change() {
        let start = std::time::Instant::now();
        let mut pending = SasFeedback(Some((1, start)));
        assert_eq!(
            pending.observe(Some(1), Some("Default"), start),
            SasObservation::Waiting
        );
        assert_eq!(
            pending.observe(Some(1), None, start + Duration::from_secs(7)),
            SasObservation::Waiting
        );
        assert_eq!(
            pending.observe(Some(1), Some("Winlogon"), start + Duration::from_secs(7)),
            SasObservation::SecureDesktop
        );
        assert!(pending.0.is_none());
        pending.0 = Some((1, start));
        assert_eq!(
            pending.observe(Some(1), Some("Default"), start + Duration::from_secs(8)),
            SasObservation::Unconfirmed
        );
        pending.0 = Some((1, start));
        assert_eq!(
            pending.observe(Some(2), Some("Winlogon"), start),
            SasObservation::SessionChanged
        );
    }
    #[test]
    fn pending_clipboard_keeps_latest_publication_and_clears_on_desktop_change() {
        let mut pending = PendingClipboard::default();
        pending.remember(&ControlMessage::ClipboardText(pb::ClipboardText {
            text: "old".into(),
        }));
        pending.remember(&ControlMessage::ClipboardOffer(pb::ClipboardOffer {
            offer_id: 7,
            ..Default::default()
        }));
        pending.remember(&ControlMessage::ClipboardRequest(
            pb::ClipboardRequest::default(),
        ));
        assert!(
            matches!(pending.take().as_slice(), [ControlMessage::ClipboardOffer(o)] if o.offer_id == 7)
        );
        pending.remember(&ControlMessage::ClipboardText(pb::ClipboardText {
            text: "previous user".into(),
        }));
        pending.clear();
        assert!(pending.take().is_empty());
    }
    #[test]
    fn pending_images_replay_only_complete_publications() {
        let mut pending = PendingClipboard::default();
        let header = ControlMessage::ClipboardImage(pb::ClipboardImage {
            image_id: 1,
            width: 1,
            height: 1,
            total_bytes: 4,
        });
        let chunk = |offset| {
            ControlMessage::ClipboardImageChunk(pb::ClipboardImageChunk {
                image_id: 1,
                offset,
                data: vec![1, 2],
            })
        };
        pending.remember(&header);
        pending.remember(&chunk(0));
        assert!(pending.take().is_empty()); // An agent can become ready midway.
        pending.remember(&chunk(2));
        let messages = pending.take();
        assert!(
            matches!(messages.as_slice(), [ControlMessage::ClipboardImage(_), ControlMessage::ClipboardImageChunk(c)] if c.data == [1,2,1,2])
        );
        for message in &messages {
            pending.remember(message);
        }
        assert_eq!(pending.take().len(), 2); // Failed replay can retry from its header.
        pending.remember(&header);
        pending.remember(&chunk(2));
        assert!(pending.take().is_empty());
        pending.remember(&header);
        pending.clear();
        pending.remember(&chunk(0));
        pending.remember(&chunk(2));
        assert!(pending.take().is_empty());
        pending.remember(&header);
        pending.remember(&ControlMessage::ClipboardText(pb::ClipboardText {
            text: "new".into(),
        }));
        pending.remember(&chunk(0));
        assert!(matches!(
            pending.take().as_slice(),
            [ControlMessage::ClipboardText(_)]
        ));
    }
    #[test]
    fn pending_image_descriptor_unblocks_new_receiver_before_chunks_exist() {
        let mut pending = PendingClipboard::default();
        let header = pb::ClipboardImage {
            image_id: 7,
            width: 1,
            height: 1,
            total_bytes: 4,
        };
        pending.remember(&ControlMessage::ClipboardImage(header));
        assert!(
            matches!(pending.take_for_receiver().as_slice(), [ControlMessage::ClipboardImage(h)] if h.image_id == 7)
        );
        assert!(pending.image.is_none());
    }

    #[tokio::test]
    async fn occupied_tcp_and_udp_ports_return_actionable_errors_and_release_sockets() {
        let tcp = TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let cfg = crate::config::AppConfig {
            password: "test-only".into(),
            port,
            ..Default::default()
        };
        let error = Broker::start(cfg, CancellationToken::new())
            .await
            .err()
            .expect("occupied TCP must fail")
            .to_string();
        assert!(
            error.contains("TCP")
                && error.contains(&port.to_string())
                && (error.contains("占用") || error.contains("访问被拒绝")),
            "{error}"
        );
        drop(tcp);

        let udp = UdpSocket::bind(("0.0.0.0", 0)).await.unwrap();
        let port = udp.local_addr().unwrap().port();
        let cfg = crate::config::AppConfig {
            password: "test-only".into(),
            port,
            ..Default::default()
        };
        let error = Broker::start(cfg, CancellationToken::new())
            .await
            .err()
            .expect("occupied UDP must fail")
            .to_string();
        assert!(
            error.contains("UDP")
                && error.contains(&port.to_string())
                && (error.contains("占用") || error.contains("访问被拒绝")),
            "{error}"
        );
        let _released_tcp = TcpListener::bind(("0.0.0.0", port))
            .await
            .expect("failed startup must close its TCP socket");
    }
}

#[cfg(test)]
mod desktop_epoch_tests {
    use super::*;
    #[test]
    fn late_epochs_cannot_restore_input_or_bypass_configuration() {
        let mut e = AgentEpoch::default();
        assert!(!e.accepts_video(0));
        assert_eq!(e.desktop(2, false), Some(true));
        assert!(!e.accepts_video(2));
        assert!(e.configure(2));
        assert!(e.accepts_video(2));
        assert_eq!(e.desktop(2, true), Some(false));
        assert!(e.input_ready);
        assert_eq!(e.desktop(4, false), Some(true));
        assert!(!e.input_ready);
        assert!(!e.accepts_video(2));
        assert!(!e.accepts_video(4));
        assert_eq!(e.desktop(3, true), None);
        assert!(!e.configure(3));
        assert!(!e.input_ready);
        assert_eq!(e.desktop(4, true), Some(false));
        assert!(e.configure(4));
        assert!(e.accepts_video(4));
    }
    #[tokio::test]
    async fn desktop_events_bypass_a_saturated_video_lane_and_preserve_the_pipe() {
        let (pipe, peer) = tokio::io::duplex(1024);
        let mut channel = ControlChannel::new(pipe);
        let mut peer = ControlChannel::new(peer);
        let (events, mut state) = tokio::sync::watch::channel(None);
        events.send_replace(Some(DesktopEvent {
            generation: 7,
            metadata: Metadata {
                desktop: "Winlogon".into(),
                monitors: Vec::new(),
            },
            capture_ready: false,
            reason: String::new(),
        }));
        let message = tokio::time::timeout(
            Duration::from_millis(100),
            receive_agent(&mut channel, &mut state, std::future::pending(), true),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(matches!(message, ControlMessage::Agent(event) if event.generation == 7));
        peer.send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 42 }))
            .await
            .unwrap();
        assert!(
            matches!(receive_agent(&mut channel, &mut state, std::future::ready(Ok(())), true).await.unwrap(),
            Some(ControlMessage::Ping(p)) if p.timestamp_ms == 42)
        );
    }
}
