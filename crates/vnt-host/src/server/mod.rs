//! 被控端服务：TCP 认证/协商 + 输入注入，按会话选择 TCP/UDP 视频推流。
//!
//! 每个客户端会话：
//! ```text
//! accept → Hello 校验 → 协商编码 → Welcome
//!        ├─ TCP reader:  InputEvent→SendInput / RequestKeyframe / Ping→Pong
//!        ├─ TCP writer:  UdpBindAck / StreamReset / StatsReport / Pong / SessionEnd / VideoFrame（纯 TCP）
//!        ├─ UDP recv(共享socket): UdpBind 绑定客户端地址
//!        ├─ 共享 capture: 单次抓屏 → Arc<BGRA> → 各会话独立订阅
//!        ├─ pipeline 线程: 编码 → net_tx
//!        └─ UDP sender:   完整帧分片 → 客户端
//! ```

mod congestion;
#[cfg(test)]
mod managed_session_tests;
#[cfg(test)]
mod multiscreen_tests;
mod portable;
mod video;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::EventSink;
use congestion::{FeedbackInbox, KeyframeGate, SendCongestion};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use vnt_capture::{CaptureCancelHandle, SharedCapture};
use vnt_codec::VideoEncoder;
use vnt_net::datagram::Datagram as UdpSocket;
use vnt_net::tcp::ControlChannel;
use vnt_proto::control::{decode_udp_bind_frame, ControlMessage};
use vnt_proto::negotiate::negotiate_with_decoder_hardware;
use vnt_proto::pb;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub password: String,
    pub port: u16,
    pub monitor_index: u32,
    pub region: Option<vnt_capture::ScreenRegion>,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// 硬件编码开关：关闭后能力探测只上报软件编码，会话用纯软件后端。
    pub hardware_encode: bool,
    pub audio: bool,
    /// 仅共享屏幕（观看模式）：忽略客户端输入事件，剪贴板同步强制关闭。
    pub view_only: bool,
    /// 剪贴板同步服务（会话注册/进度事件出口）。
    pub clipboard: Arc<crate::clipboard::ClipboardService>,
    /// 测试用：合成帧源代替真实抓屏（无人值守桌面 WGC 无新帧时保证可测）。
    #[doc(hidden)]
    pub synthetic: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub transport: vnt_proto::Transport,
    pub session_id: u32,
    pub peer: String,
    pub codec: String,
    pub hardware: bool,
    pub encoder_name: String,
    pub width: u32,
    pub height: u32,
    /// 仅共享屏幕（观看模式）会话。
    pub view_only: bool,
    pub audio_enabled: bool,
}

pub struct ServerHandle {
    overlay_context: ServerContext,
    pub port: u16,
    active: SessionRegistry,
    capture: SharedCapture,
    audio: vnt_audio::SharedAudio,
    stop_tx: tokio::sync::watch::Sender<bool>,
    /// 监听/UDP 等基础任务。
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// 已建立的会话任务（含仍在握手中的），stop 时逐一终止。
    sessions: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl ServerHandle {
    pub fn accept_overlay(
        &self,
        channel: vnt_net::session::SessionChannel,
        peer: SocketAddr,
        media: Arc<UdpSocket>,
    ) {
        let mut context = self.overlay_context.clone();
        context.udp = Some(media);
        let task = tokio::spawn(async move {
            if let Err(error) = handle_channel(channel, peer, context).await {
                tracing::warn!("VNT 被控会话结束：{error}");
            }
        });
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|t| !t.is_finished());
        sessions.push(task);
    }

    pub fn active_sessions(&self) -> Vec<SessionInfo> {
        self.active
            .lock()
            .unwrap()
            .values()
            .filter_map(|a| a.info.clone())
            .collect()
    }

    /// 停止服务：通知所有会话（含管线线程）退出，等待清理完成后兜底 abort。
    pub async fn stop(mut self) {
        // 1) 广播停止：会话 reader 循环 select 感知，会话的停止看门狗
        //    取消帧队列直接唤醒管线线程
        self.capture.cancel();
        self.audio.stop();
        let _ = self.stop_tx.send(true);
        // 2) 立即终止监听/UDP 基础任务（不再接受新连接/新绑定）
        for t in self.tasks.drain(..) {
            t.abort();
            let _ = t.await;
        }
        // 3) 等待会话任务自行清理（发送 SessionEnd、停管线、释放会话槽），
        //    最多 3 秒，超时强制 abort（此时管线线程也会因帧队列取消自行退出）
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let sessions: Vec<_> = self.sessions.lock().unwrap().drain(..).collect();
        for jh in sessions {
            while !jh.is_finished() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !jh.is_finished() {
                tracing::warn!("会话任务未及时退出，强制终止");
                jh.abort();
            }
            let _ = jh.await; // Abort requests cancellation; await its resource cleanup.
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.capture.cancel();
        self.audio.stop();
        let _ = self.stop_tx.send(true);
        for task in &self.tasks {
            task.abort();
        }
        for task in self.sessions.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

type SessionRegistry = Arc<std::sync::Mutex<HashMap<u32, ActiveSession>>>;

/// Registration is removed on all exits, including failed handshakes and abort.
struct SessionRegistration {
    sessions: SessionRegistry,
    id: u32,
    sink: Arc<dyn EventSink>,
    reason: String,
}
impl Drop for SessionRegistration {
    fn drop(&mut self) {
        let removed = self.sessions.lock().unwrap().remove(&self.id);
        if removed.is_some_and(|a| a.info.is_some()) {
            self.sink.emit_json(
                "server://session-end",
                serde_json::json!({
                    "sessionId": self.id, "reason": self.reason,
                })
                .to_string(),
            );
        }
    }
}

/// Bridge cancellation that can precede the lazy capture subscription.
#[derive(Clone, Default)]
struct SessionCapture(Arc<std::sync::Mutex<SessionCaptureState>>);

#[derive(Default)]
struct SessionCaptureState {
    cancelled: bool,
    handle: Option<CaptureCancelHandle>,
}

impl SessionCapture {
    fn attach(&self, handle: CaptureCancelHandle) -> bool {
        let mut state = self.0.lock().unwrap();
        if state.cancelled {
            drop(state);
            handle.cancel();
            return false;
        }
        state.handle = Some(handle);
        true
    }

    fn cancel(&self) {
        let handle = {
            let mut state = self.0.lock().unwrap();
            state.cancelled = true;
            state.handle.take()
        };
        if let Some(handle) = handle {
            handle.cancel();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap().cancelled
    }
}

/// Task abort must also stop the synchronous pipeline and close its sockets.
struct SessionResources {
    capture_session: SessionCapture,
    pipeline_cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Capture EOF and thread panics must terminate the session just like explicit
/// encoder errors; otherwise TCP heartbeats keep a frozen video session alive.
struct PipelineExit(tokio::sync::watch::Sender<bool>, SessionCapture);

impl Drop for PipelineExit {
    fn drop(&mut self) {
        if !self.1.is_cancelled() {
            let _ = self.0.send(true);
        }
    }
}
impl Drop for SessionResources {
    fn drop(&mut self) {
        self.pipeline_cancel.cancel();
        self.capture_session.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Clone)]
struct ActiveSession {
    info: Option<SessionInfo>,
    /// UDP 绑定凭证（随机，仅随 Welcome 下发）：防止未认证接收端
    /// 凭猜测的 session_id 劫走视频流。
    bind_token: Vec<u8>,
    /// TCP 对端 IP：UDP 绑定只接受来自同一 IP 的源地址。
    peer_ip: std::net::IpAddr,
    /// 重绑次数上限（防止恶意churn）。
    rebinds: Arc<std::sync::atomic::AtomicU32>,
    control_tx: mpsc::Sender<ControlMessage>,
    udp_peer: Arc<RwLock<Option<SocketAddr>>>,
}

/// 单个会话允许的 UDP 重绑次数。
const MAX_UDP_REBINDS: u32 = 8;

/// Includes pending authentication and established sessions, so incomplete
/// handshakes cannot allocate an unbounded number of tasks and frame buffers.
pub const MAX_CONNECTIONS: usize = 32;

pub async fn start(sink: Arc<dyn EventSink>, cfg: ServerConfig) -> Result<ServerHandle, String> {
    start_inner(sink, cfg, None).await
}

pub async fn start_portable(
    sink: Arc<dyn EventSink>,
    cfg: ServerConfig,
    config_path: std::path::PathBuf,
) -> Result<ServerHandle, String> {
    #[cfg(windows)]
    if !cfg.synthetic {
        crate::windows::desktop_effects::initialize();
    }
    start_inner(sink, cfg, Some(portable::Portable::new(config_path))).await
}

async fn start_inner(
    sink: Arc<dyn EventSink>,
    cfg: ServerConfig,
    portable: Option<Arc<portable::Portable>>,
) -> Result<ServerHandle, String> {
    start_with_source(sink, cfg, portable, None).await
}

/// Test/benchmark entry point with a deterministic capture source. The first
/// StreamReset advertises its actual dimensions, as for a monitor mode change.
#[doc(hidden)]
pub async fn start_with_capture(
    sink: Arc<dyn EventSink>,
    cfg: ServerConfig,
    capture: SharedCapture,
) -> Result<ServerHandle, String> {
    start_with_source(sink, cfg, None, Some(capture)).await
}

async fn start_with_source(
    sink: Arc<dyn EventSink>,
    cfg: ServerConfig,
    portable: Option<Arc<portable::Portable>>,
    source: Option<SharedCapture>,
) -> Result<ServerHandle, String> {
    if !(1..=120).contains(&cfg.fps) || !(100..=200_000).contains(&cfg.bitrate_kbps) {
        return Err("帧率或码率超出支持范围".into());
    }
    // Avoid Windows' excluded ephemeral UDP ranges when choosing a port.
    // Prefer a port supporting both transports, retaining a TCP-only fallback.
    let mut fallback = None;
    let mut bound = None;
    let mut last_error = String::new();
    for _ in 0..if cfg.port == 0 { 32 } else { 1 } {
        let port = if cfg.port == 0 {
            let mut bytes = [0u8; 2];
            getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
            20000 + u16::from_be_bytes(bytes) % 30000
        } else {
            cfg.port
        };
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(e) => {
                last_error = format!("TCP 绑定端口 {port} 失败: {e}");
                continue;
            }
        };
        match tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => {
                bound = Some((listener, Some(socket), port));
                break;
            }
            Err(e) => {
                tracing::warn!("UDP 绑定端口 {port} 失败: {e}");
                if fallback.is_none() {
                    fallback = Some((listener, None, port));
                }
            }
        }
    }
    let (listener, udp, port) = bound.or(fallback).ok_or(last_error)?;
    tracing::info!("被控端监听 0.0.0.0:{port}, UDP available={}", udp.is_some());

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let clipboard = cfg.clipboard.clone();
    if cfg.synthetic {
        if let Some(portable) = &portable {
            portable.capture_available.store(true, Ordering::Release);
        }
    }
    let audio = vnt_audio::SharedAudio::new(cfg.synthetic);
    let session_slot: SessionRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (quic_listener, udp) = if let Some(socket) = udp {
        let slots = session_slot.clone();
        let validate: vnt_net::quic::ValidateBind = Arc::new(move |id, token, peer| {
            slots.lock().unwrap().get(&id).is_some_and(|active| {
                active
                    .info
                    .as_ref()
                    .is_some_and(|info| info.transport == vnt_proto::Transport::Hybrid)
                    && active.bind_token == token
                    && active.peer_ip == peer.ip()
            })
        });
        let portable = portable.clone();
        let password = cfg.password.clone();
        let passwords: vnt_net::quic::Password = Arc::new(move || {
            if let Some(portable) = &portable {
                crate::store::load_existing_from(&portable.path)
                    .map(|cfg| cfg.password)
                    .map_err(std::io::Error::other)
            } else {
                Ok(password.clone())
            }
        });
        let (listener, media) =
            vnt_net::quic::Listener::new(socket, cfg.password.clone(), passwords, validate)
                .await
                .map_err(|e| format!("初始化 QUIC 监听失败: {e}"))?;
        (Some(listener), Some(Arc::new(media)))
    } else {
        (None, None)
    };
    let input_owners = Arc::new(std::sync::Mutex::new(InputOwners::default()));
    let capture = source.unwrap_or_else(|| {
        if cfg.synthetic {
            SharedCapture::synthetic(1280, 720)
        } else if let Some(portable) = &portable {
            SharedCapture::resilient_monitor(
                cfg.monitor_index,
                portable.desktop.clone(),
                portable.capture_available.clone(),
            )
        } else {
            SharedCapture::monitor(cfg.monitor_index)
        }
    });
    // Keep all capabilities; each session filters its own configuration snapshot.
    let caps = vnt_codec::probe_caps();
    if let Some(portable) = &portable {
        portable
            .captures
            .lock()
            .unwrap()
            .insert(cfg.monitor_index, capture.clone());
    }
    for c in &caps {
        tracing::info!("编码能力: {c:?}");
    }
    let sessions: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let mut tasks = Vec::new();
    if let Some(portable) = portable.clone() {
        clipboard.set_enabled(false);
        tasks.push(tokio::spawn(
            portable.watch(Some(clipboard.clone()), stop_rx.clone()),
        ));
    }

    // UDP 绑定接收（共享 socket 的读侧）
    if let Some(udp) = udp.clone() {
        let slot = session_slot.clone();
        let stop_rx = stop_rx.clone();
        tasks.push(tokio::spawn(async move {
            // 绑定帧是 5 字节头 + 小型 protobuf（约 25 字节）；按协议上限取
            // 一块小缓冲即可，超限的杂包截断后解析失败即忽略
            let mut buf = vec![0u8; vnt_proto::udp::DEFAULT_MTU + vnt_proto::udp::FRAG_HEADER_LEN];
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let (len, from) = match udp.recv_from(&mut buf).await {
                    Ok(packet) => packet,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        // Windows reports an ICMP error for a recently closed
                        // video peer on this shared socket's next receive.
                        // The listener must survive so other peers can bind.
                        tracing::debug!("UDP 对端已关闭，继续接收其他会话: {error}");
                        continue;
                    }
                    Err(error) => {
                        tracing::error!("UDP 绑定接收失败: {error}");
                        break;
                    }
                };
                let Some((sid, token)) = decode_udp_bind_frame(&buf[..len]) else {
                    continue;
                };
                let active = slot.lock().unwrap().get(&sid).cloned();
                if let Some(active) = active {
                    // 三重校验：会话 ID + 绑定凭证 + 来源 IP 必须与 TCP 对端一致
                    if token != active.bind_token || from.ip() != active.peer_ip {
                        tracing::warn!("拒绝非法 UDP 绑定: {from} (sid={sid})");
                        continue;
                    }
                    let ActiveSession {
                        rebinds,
                        control_tx,
                        udp_peer,
                        ..
                    } = active;
                    let already = *udp_peer.read().await == Some(from);
                    if !already && rebinds.load(Ordering::Relaxed) >= MAX_UDP_REBINDS {
                        tracing::warn!("UDP 重绑超限，忽略 {from}");
                        continue;
                    }
                    if !already {
                        rebinds.fetch_add(1, Ordering::Relaxed);
                        *udp_peer.write().await = Some(from);
                        let _ = control_tx.try_send(ControlMessage::UdpBindAck(pb::UdpBindAck {
                            session_id: sid,
                        }));
                        tracing::info!("UDP 视频通道 → {from}");
                    }
                }
            }
        }));
    }

    let overlay_context = ServerContext {
        worker: false,
        snapshot: None,
        service_desktop: None,
        portable: portable.clone(),
        capture: capture.clone(),
        audio: audio.clone(),
        input_owners: input_owners.clone(),
        cfg: cfg.clone(),
        caps: caps.clone(),
        session_slot: session_slot.clone(),
        udp: None,
        sink: sink.clone(),
        stop_rx: stop_rx.clone(),
        clipboard: clipboard.clone(),
    };
    let admission = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    if let Some(mut listener) = quic_listener {
        let context = overlay_context.clone();
        let sessions = sessions.clone();
        let admission = admission.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(accepted) = listener.accept().await {
                let Ok(permit) = admission.clone().try_acquire_owned() else {
                    continue;
                };
                let mut context = context.clone();
                context.udp = Some(accepted.media);
                let task = tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) =
                        handle_channel(accepted.channel, accepted.peer, context).await
                    {
                        tracing::warn!("QUIC 被控会话结束: {error}");
                    }
                });
                let mut sessions = sessions.lock().unwrap();
                sessions.retain(|task| !task.is_finished());
                sessions.push(task);
            }
        }));
    }
    let remote_sessions = vnt_net::session::SessionListener::default();
    // TCP 接收循环
    {
        let capture = capture.clone();
        let audio = audio.clone();
        let admission = admission.clone();
        let stop_rx = stop_rx.clone();
        let slot = session_slot.clone();
        let caps = caps.clone();
        let cfg = cfg.clone();
        let udp = udp.clone();
        let sink = sink.clone();
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let Ok(permit) = admission.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                stream.set_nodelay(true).ok();
                let fut = handle_remote_conn(
                    stream,
                    peer,
                    remote_sessions.clone(),
                    ServerContext {
                        worker: false,
                        snapshot: None,
                        service_desktop: None,
                        portable: portable.clone(),
                        capture: capture.clone(),
                        audio: audio.clone(),
                        input_owners: input_owners.clone(),
                        cfg: cfg.clone(),
                        caps: caps.clone(),
                        session_slot: slot.clone(),
                        udp: udp.clone(),
                        sink: sink.clone(),
                        stop_rx: stop_rx.clone(),
                        clipboard: clipboard.clone(),
                    },
                );
                let jh = tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = fut.await {
                        tracing::error!("会话处理异常: {e}");
                    }
                });
                // 注册会话任务：ServerHandle::stop 时逐一终止
                let mut guard = sessions.lock().unwrap();
                guard.retain(|h| !h.is_finished());
                guard.push(jh);
            }
        }));
    }

    Ok(ServerHandle {
        overlay_context,
        port,
        active: session_slot,
        capture,
        audio,
        stop_tx,
        tasks,
        sessions,
    })
}

#[derive(Clone)]
struct ServerContext {
    /// Internal per-screen capture channel; never selected from peer input.
    worker: bool,
    snapshot: Option<crate::config::AppConfig>,
    service_desktop: Option<Arc<vnt_capture::DesktopCapture>>,
    portable: Option<Arc<portable::Portable>>,
    capture: SharedCapture,
    audio: vnt_audio::SharedAudio,
    input_owners: Arc<std::sync::Mutex<InputOwners>>,
    cfg: ServerConfig,
    caps: Vec<vnt_proto::negotiate::EncoderCap>,
    session_slot: SessionRegistry,
    udp: Option<Arc<UdpSocket>>,
    sink: Arc<dyn EventSink>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    clipboard: Arc<crate::clipboard::ClipboardService>,
}

fn session_ready(portable: &portable::Portable, video_stopped: &AtomicBool) -> bool {
    portable.normal_desktop() && (portable.available() || video_stopped.load(Ordering::Acquire))
}

async fn handle_remote_conn(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    sessions: vnt_net::session::SessionListener,
    context: ServerContext,
) -> Result<(), String> {
    if let Some(channel) = sessions
        .accept(stream, peer)
        .await
        .map_err(|e| e.to_string())?
    {
        handle_channel(channel, peer, context).await?;
    }
    Ok(())
}

fn serve_screens<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
    mut channel: ControlChannel<S>,
    peer: SocketAddr,
    context: ServerContext,
    saved: crate::config::AppConfig,
    hello: pb::Hello,
) -> futures_util::future::BoxFuture<'static, Result<(), String>> {
    Box::pin(async move {
        let monitors = if context.cfg.synthetic {
            vec![vnt_capture::synthetic_monitor(0, 1280, 720)]
        } else {
            match tokio::task::spawn_blocking(vnt_capture::list_available_monitors)
                .await
                .map_err(|e| e.to_string())?
            {
                Ok(monitors) => monitors,
                Err(error) => {
                    let _ = channel
                        .send(&ControlMessage::Denied(pb::Denied {
                            reason: error.to_string(),
                        }))
                        .await;
                    return Ok(());
                }
            }
        };
        let targets = match crate::screens::targets(&saved, &monitors) {
            Ok(targets) => targets,
            Err(reason) => {
                let _ = channel
                    .send(&ControlMessage::Denied(pb::Denied { reason }))
                    .await;
                return Ok(());
            }
        };
        if targets.iter().all(|t| !t.info.available) {
            let _ = channel
                .send(&ControlMessage::Denied(pb::Denied {
                    reason: "所有采集目标均不可用".into(),
                }))
                .await;
            return Ok(());
        }
        let mut token = vec![0; 16];
        getrandom::fill(&mut token).map_err(|e| e.to_string())?;
        let peer_target = Arc::new(RwLock::new(
            matches!(hello.transport, 3 | 4).then_some(peer),
        ));
        let (tx, rx) = mpsc::channel(64);
        let id = {
            let mut sessions = context.session_slot.lock().unwrap();
            loop {
                let mut bytes = [0; 4];
                getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
                let id = u32::from_be_bytes(bytes).max(1);
                if let std::collections::hash_map::Entry::Vacant(e) = sessions.entry(id) {
                    e.insert(ActiveSession {
                        info: None,
                        bind_token: token.clone(),
                        peer_ip: peer.ip(),
                        rebinds: Arc::new(AtomicU32::new(0)),
                        control_tx: tx.clone(),
                        udp_peer: peer_target.clone(),
                    });
                    break id;
                }
            }
        };
        let _registration = SessionRegistration {
            sessions: context.session_slot.clone(),
            id,
            sink: context.sink.clone(),
            reason: "多屏会话结束".into(),
        };
        tx.send(ControlMessage::Welcome(pb::Welcome {
            system: vnt_proto::system::local() as i32,
            bind_token: token,
            ..Default::default()
        }))
        .await
        .map_err(|e| e.to_string())?;
        let cancel = CancellationToken::new();
        let watcher_cancel = cancel.clone();
        let mut stop = context.stop_rx.clone();
        let watcher = tokio::spawn(async move {
            if !*stop.borrow() {
                let _ = stop.changed().await;
            }
            watcher_cancel.cancel();
        });
        let guard = AbortTask(watcher);
        let udp = if hello.transport != 2 {
            context.udp.clone()
        } else {
            None
        };
        let open_context = context.clone();
        let open: crate::screens::Open = Arc::new(move |mut snapshot, hello| {
            if !open_context.cfg.synthetic {
                let resolved = vnt_capture::list_monitors()
                    .map_err(|e| e.to_string())
                    .and_then(|monitors| crate::screens::resolve_backend(&mut snapshot, &monitors));
                if let Err(reason) = resolved {
                    return Box::pin(async move { Err(reason) });
                }
            }
            let mut context = open_context.clone();
            context.worker = true;
            context.snapshot = Some(snapshot.clone());
            context.session_slot = Default::default();
            context.sink = Arc::new(crate::NoopEventSink);
            Box::pin(async move {
                let (local, remote) = vnt_net::session::local_pair();
                let task = tokio::spawn(async move {
                    let _ = Box::pin(handle_channel(remote, peer, context)).await;
                });
                crate::screens::handshake(local, hello, &snapshot.password, task).await
            })
        });
        let registry = context.session_slot.clone();
        let sink = context.sink.clone();
        let started = Arc::new(move |welcome: &pb::Welcome| {
            let info = SessionInfo {
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
            };
            if let Some(active) = registry.lock().unwrap().get_mut(&id) {
                active.info = Some(info.clone());
            }
            sink.emit_json(
                "server://session",
                serde_json::to_string(&info).unwrap_or_default(),
            );
        });
        let result = crate::screens::serve(
            channel,
            hello,
            targets,
            open,
            crate::screens::Transport {
                session_id: id,
                udp,
                peer: peer_target,
                updates: rx,
                cancel,
                started,
            },
        )
        .await;
        drop(guard);
        result
    })
}

struct AbortTask(tokio::task::JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn handle_conn<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    peer: SocketAddr,
    context: ServerContext,
) -> Result<(), String> {
    handle_channel(ControlChannel::new(stream), peer, context).await
}

async fn handle_channel<
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
>(
    mut channel: ControlChannel<S>,
    peer: SocketAddr,
    context: ServerContext,
) -> Result<(), String> {
    let parent_context = context.clone();
    let ServerContext {
        worker,
        snapshot,
        service_desktop,
        portable,
        capture,
        audio,
        input_owners,
        cfg,
        caps,
        session_slot,
        udp,
        sink,
        mut stop_rx,
        clipboard,
    } = context;
    // ---- 握手 ----
    let hello = tokio::time::timeout(Duration::from_secs(10), channel.recv())
        .await
        .map_err(|_| "握手超时".to_string())?;
    let hello = match hello {
        Ok(Some(ControlMessage::Hello(h))) => h,
        Ok(_) => return Ok(()),
        Err(_) => return Ok(()),
    };
    if vnt_proto::system::from_wire(hello.system).is_err() {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: "主控系统信息缺失或无效，请同步升级双方程序".into(),
            }))
            .await;
        return Ok(());
    }
    if hello.proto_ver != vnt_proto::PROTO_VER || hello.desktop_effects.is_none() {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: "协议版本不匹配，请同步升级主控与被控".into(),
            }))
            .await;
        return Ok(());
    }
    let (nonce, proof) = vnt_net::auth::challenge(&mut channel)
        .await
        .map_err(|e| format!("密码认证失败: {e}"))?;
    // One snapshot supplies authentication and all policies for this session.
    // Updating a session must never change the shared clipboard policy.
    let mut cfg = cfg;
    let mut capture = capture;
    let original_monitor = cfg.monitor_index;
    let mut portable = portable;
    let mut saved_config = None;
    let clipboard_enabled = if let Some(portable) = &portable {
        let saved = match snapshot
            .clone()
            .map(Ok)
            .unwrap_or_else(|| crate::store::load_existing_from(&portable.path))
        {
            Ok(saved) => saved,
            Err(error) => {
                let _ = channel
                    .send(&ControlMessage::Denied(pb::Denied {
                        reason: "被控端无法读取配置，请检查配置文件".into(),
                    }))
                    .await;
                return Err(format!("读取被控配置失败: {error}"));
            }
        };
        saved_config = Some(saved.clone());
        cfg.region = match if snapshot.is_none() {
            Ok(None)
        } else {
            saved.selected_region()
        } {
            Ok(region) => region,
            Err(reason) => {
                let _ = channel
                    .send(&ControlMessage::Denied(pb::Denied { reason }))
                    .await;
                return Ok(());
            }
        };
        cfg.password = saved.password;
        cfg.monitor_index = saved.monitor_index;
        cfg.hardware_encode = saved.hardware_encode;
        cfg.audio = saved.audio;
        cfg.view_only = !saved.allow_control;
        saved.clipboard
    } else {
        clipboard.is_enabled()
    };
    let authenticated = vnt_proto::auth::verify_remote(&cfg.password, &nonce, &proof);
    if !authenticated {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: "密码错误".into(),
            }))
            .await;
        tracing::warn!("客户端 {peer} 密码错误");
        return Ok(());
    }
    #[cfg(windows)]
    let _effects = if snapshot.is_none() && portable.is_some() && !cfg.synthetic && !cfg.view_only {
        let settings = crate::windows::desktop_effects::Settings::from(
            hello
                .desktop_effects
                .as_ref()
                .expect("validated desktop effects"),
        );
        tokio::task::spawn_blocking(move || {
            crate::windows::desktop_effects::Lease::acquire(settings)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    if !worker {
        let saved = if let Some(saved) = saved_config {
            saved
        } else {
            let monitors = if cfg.synthetic {
                vec![vnt_capture::synthetic_monitor(0, 1280, 720)]
            } else {
                vnt_capture::list_available_monitors().map_err(|e| e.to_string())?
            };
            let mut saved = crate::config::AppConfig::initial(&monitors);
            saved.password = cfg.password.clone();
            saved.hardware_encode = cfg.hardware_encode;
            saved.audio = cfg.audio;
            saved.allow_control = !cfg.view_only;
            saved.clipboard = clipboard_enabled;
            if let Some(region) = &cfg.region {
                saved.screen_regions = vec![region.clone()];
                saved.selected_targets = vec![crate::config::CaptureTarget::Region {
                    id: region.id.clone(),
                }];
            } else if let Some(monitor) = monitors.iter().find(|m| m.index == cfg.monitor_index) {
                saved.selected_targets = vec![crate::config::CaptureTarget::Monitor {
                    name: monitor.name.clone(),
                }];
            }
            saved
        };
        return serve_screens(channel, peer, parent_context, saved, hello).await;
    }
    if portable.is_none() {
        if let Some(saved) = &snapshot {
            cfg.monitor_index = saved.monitor_index;
            cfg.region = saved.selected_region()?;
        }
    }
    if portable.as_ref().is_some_and(|p| !p.normal_desktop()) {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: "免安装版仅支持当前用户的普通桌面，请解锁后重试".into(),
            }))
            .await;
        return Ok(());
    }
    if vnt_proto::color_depth::from_wire(hello.color_depth).is_none()
        || hello.fec_version > vnt_net::fec::VERSION
        || (hello.transport == 2 && hello.fec_version != 0)
        || !(1..=120).contains(&hello.fps)
        || !(100..=200_000).contains(&hello.bitrate_kbps)
        || !crate::resolution::validate(
            hello.resolution_mode,
            hello.target_width,
            hello.target_height,
        )
    {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: "视频参数超出允许范围".into(),
            }))
            .await;
        return Ok(());
    }
    let transport = match vnt_proto::Transport::from_wire(hello.transport) {
        Ok(mode) if mode == vnt_proto::Transport::Tcp || udp.is_some() => mode,
        result => {
            let reason = match result {
                Err(e) => e,
                _ => "UDP 不可用，请选择纯 TCP",
            };
            let _ = channel
                .send(&ControlMessage::Denied(pb::Denied {
                    reason: reason.into(),
                }))
                .await;
            return Ok(());
        }
    };
    if let Some(region) = &cfg.region {
        let result = if !portable::normal_desktop() && !cfg.synthetic {
            Err("区域共享在锁屏或安全桌面暂停".to_string())
        } else {
            let monitors = if cfg.synthetic {
                Ok(vec![vnt_capture::synthetic_monitor(0, 1280, 720)])
            } else {
                vnt_capture::list_monitors().map_err(|e| e.to_string())
            };
            monitors.and_then(|monitors| region.resolve(&monitors).map(|m| m.index))
        };
        match result {
            Ok(index) => cfg.monitor_index = index,
            Err(reason) => {
                let _ = channel
                    .send(&ControlMessage::Denied(pb::Denied { reason }))
                    .await;
                return Ok(());
            }
        }
    }
    if cfg.monitor_index != original_monitor {
        if let Some(root) = &portable {
            let selected = root
                .monitors
                .lock()
                .unwrap()
                .entry(cfg.monitor_index)
                .or_insert_with(|| {
                    let monitor = portable::Portable::new(root.path.clone());
                    #[cfg(test)]
                    let monitor = {
                        let mut monitor = monitor;
                        Arc::get_mut(&mut monitor).unwrap().probe = root.probe.clone();
                        monitor
                    };
                    monitor
                        .desktop
                        .store(root.normal_desktop(), Ordering::Release);
                    monitor
                        .capture_available
                        .store(cfg.synthetic, Ordering::Release);
                    tokio::spawn(monitor.clone().watch(None, stop_rx.clone()));
                    monitor
                })
                .clone();
            portable = Some(selected);
        }
    }
    if let Some(portable) = &portable {
        capture = portable
            .captures
            .lock()
            .unwrap()
            .entry(cfg.monitor_index)
            .or_insert_with(|| {
                if cfg.synthetic {
                    SharedCapture::synthetic(1280, 720)
                } else {
                    SharedCapture::resilient_monitor(
                        cfg.monitor_index,
                        portable.desktop.clone(),
                        portable.capture_available.clone(),
                    )
                }
            })
            .clone();
    }
    if let (Some(base), Some(region)) = (&portable, &cfg.region) {
        portable = Some(portable::Portable::for_region(
            base.clone(),
            region.clone(),
            cfg.monitor_index,
        ));
    }
    let caps: Vec<_> = caps
        .into_iter()
        .filter(|c| cfg.hardware_encode || !c.hardware)
        .collect();
    cfg.fps = hello.fps;
    cfg.bitrate_kbps = hello.bitrate_kbps;
    let view_only = cfg.view_only;
    let mut bind_token = if transport != vnt_proto::Transport::Tcp {
        vec![0u8; 16]
    } else {
        Vec::new()
    };
    getrandom::fill(&mut bind_token).map_err(|e| format!("生成会话凭证失败: {e}"))?;
    let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(64);
    let udp_peer = Arc::new(RwLock::new(
        matches!(
            transport,
            vnt_proto::Transport::Vnt | vnt_proto::Transport::Quic
        )
        .then_some(peer),
    ));
    // Reserve a collision-free ID atomically without replacing another peer.
    let session_id = {
        let mut sessions = session_slot.lock().unwrap();
        loop {
            let mut bytes = [0u8; 4];
            getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
            let id = u32::from_be_bytes(bytes).max(1);
            if let std::collections::hash_map::Entry::Vacant(entry) = sessions.entry(id) {
                entry.insert(ActiveSession {
                    info: None,
                    bind_token: bind_token.clone(),
                    peer_ip: peer.ip(),
                    rebinds: Arc::new(AtomicU32::new(0)),
                    control_tx: control_tx.clone(),
                    udp_peer: udp_peer.clone(),
                });
                break id;
            }
        }
    };
    let mut registration = SessionRegistration {
        sessions: session_slot.clone(),
        id: session_id,
        sink: sink.clone(),
        reason: "connection ended".into(),
    };

    // ---- 协商 ----
    let client_codecs: Vec<vnt_proto::Codec> = hello
        .codecs
        .iter()
        .filter_map(|c| {
            pb::Codec::try_from(*c)
                .ok()
                .and_then(vnt_proto::Codec::from_proto)
        })
        .collect();
    let client_hardware_codecs: Vec<_> = hello
        .hardware_codecs
        .iter()
        .filter_map(|c| {
            pb::Codec::try_from(*c)
                .ok()
                .and_then(vnt_proto::Codec::from_proto)
        })
        .collect();
    let Some(cap) = negotiate_with_decoder_hardware(&caps, &client_codecs, &client_hardware_codecs)
    else {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: format!("无共同支持的编解码（客户端可解: {client_codecs:?}）"),
            }))
            .await;
        return Ok(());
    };

    // ---- 显示器 ----
    let monitors = match if cfg.synthetic {
        Ok(vec![vnt_capture::synthetic_monitor(
            cfg.monitor_index,
            1280,
            720,
        )])
    } else {
        vnt_capture::list_monitors()
    } {
        Ok(m) => m,
        Err(_) if cfg.synthetic => vec![vnt_capture::MonitorInfo {
            index: cfg.monitor_index,
            width: 1280,
            height: 720,
            name: "Synthetic".into(),
            hmonitor: 0,
            orientation: 0,
            primary: true,
        }],
        Err(e) => {
            let _ = channel
                .send(&ControlMessage::Denied(pb::Denied {
                    reason: format!("枚举显示器失败: {e}"),
                }))
                .await;
            return Err(format!("枚举显示器失败: {e}"));
        }
    };
    let selected = service_desktop.as_ref().and_then(|d| d.snapshot().monitor);
    let secure = service_desktop.as_ref().is_some_and(|d| {
        !d.snapshot()
            .desktop
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case("default"))
    });
    let Some(monitor) = selected
        .as_ref()
        .or_else(|| {
            monitors
                .iter()
                .find(|m| !secure && m.index == cfg.monitor_index)
        })
        .or_else(|| {
            service_desktop
                .as_ref()
                .and_then(|_| monitors.iter().find(|m| m.primary).or(monitors.first()))
        })
    else {
        let _ = channel
            .send(&ControlMessage::Denied(pb::Denied {
                reason: "指定的显示器不存在".into(),
            }))
            .await;
        return Err("指定的显示器不存在".into());
    };
    let mut monitor = monitor.clone();
    if cfg.synthetic {
        // Keep test workload independent of a 4K/8K host display and whether
        // its GPU is available; real-desktop tests still use native dimensions.
        monitor.width = 1280;
        monitor.height = 720;
    }

    // Physical display changes are owned by the Windows broker, never by a
    // disposable capture worker. Direct servers fall back to session scaling.
    let resolution_mode = if hello.resolution_mode == 2 {
        1
    } else {
        hello.resolution_mode
    };
    let resolution_reason = if hello.resolution_mode == 2 {
        "当前运行方式不支持修改显示模式，已回退软件缩放".to_string()
    } else {
        String::new()
    };
    let color_depth = vnt_proto::color_depth::from_wire(hello.color_depth).unwrap();
    let scale_target = (resolution_mode == 1).then_some((hello.target_width, hello.target_height));
    let (source_width, source_height) = cfg
        .region
        .as_ref()
        .map(|r| (r.width, r.height))
        .unwrap_or((monitor.width, monitor.height));
    let (output_width, output_height) = scale_target
        .map(|target| crate::resolution::dimensions(source_width, source_height, target))
        .unwrap_or((source_width, source_height));
    let fec =
        vnt_net::fec::Sender::negotiated(hello.fec_version, transport != vnt_proto::Transport::Tcp);
    let audio_enabled = cfg.audio && hello.audio_enabled;
    if channel
        .send(&ControlMessage::Welcome(pb::Welcome {
            system: vnt_proto::system::local() as i32,
            color_depth,
            capabilities: hello.capabilities & vnt_proto::platform_capabilities(),
            fec_version: if fec.is_some() {
                vnt_net::fec::VERSION
            } else {
                0
            },
            screens: Vec::new(),
            host_edition: pb::HostEdition::Portable as i32,
            resolution_mode,
            resolution_reason,
            audio_enabled,
            session_token: Vec::new(),
            transport: transport.to_proto() as i32,
            proto_ver: vnt_proto::PROTO_VER,
            session_id,
            bind_token,
            codec: cap.codec.to_proto().into(),
            width: output_width,
            height: output_height,
            fps: cfg.fps,
            hardware: cap.hardware,
            decoder_config: Vec::new(), // Annex B 参数集随 IDR 内联
            encoder_name: cap.name.clone(),
            view_only: cfg.view_only,
        }))
        .await
        .is_err()
    {
        return Ok(());
    }
    tracing::info!(
        "客户端 {peer} 接入: transport={transport:?} codec={:?} hw={} encoder={} view_only={}",
        cap.codec,
        cap.hardware,
        cap.name,
        cfg.view_only
    );

    // ---- 会话资源（会话槽已在握手阶段原子占用）----
    let (reader, writer) = channel.into_split();
    let (net_tx, net_rx) = mpsc::channel::<VideoOut>(2);
    let (video_repair, _repair_task) = if transport != vnt_proto::Transport::Tcp {
        let (repair, task) =
            vnt_net::repair::VideoRepair::start(udp.as_ref().unwrap().clone(), udp_peer.clone());
        (Some(repair), Some(task))
    } else {
        (None, None)
    };
    // Cancellation exists before the lazy capture subscription is registered.
    let capture_session = SessionCapture::default();
    // 关键帧闸门 + 接收端求助计数：reader 与管线线程共享，统一节流所有
    // 关键帧请求（客户端请求、发送队列打满），避免拥塞时 IDR 风暴
    let keyframe_gate = Arc::new(KeyframeGate::new());
    let receiver_distress = Arc::new(AtomicU32::new(0));
    let feedback = Arc::new(FeedbackInbox::default());
    let background = Arc::new(AtomicBool::new(false));
    let sent_stats = Arc::new(SentStats(
        Default::default(),
        (transport == vnt_proto::Transport::Tcp).then(|| writer.traffic()),
    ));
    // 管线故障中止信号：抓屏/编码失败时终止整个会话（而非静默占用槽）
    let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);

    let info = SessionInfo {
        transport,
        session_id,
        peer: peer.to_string(),
        codec: cap.codec.as_str().to_string(),
        hardware: cap.hardware,
        encoder_name: cap.name.clone(),
        width: output_width,
        height: output_height,
        view_only: cfg.view_only,
        audio_enabled,
    };
    session_slot
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .unwrap()
        .info = Some(info.clone());
    sink.emit_json(
        "server://session",
        serde_json::to_string(&info).unwrap_or_default(),
    );
    let mut resources = SessionResources {
        capture_session: capture_session.clone(),
        pipeline_cancel: CancellationToken::new(),
        tasks: Vec::new(),
    };

    let video_stopped = Arc::new(AtomicBool::new(false));
    let (audio_tx, mut audio_rx) = mpsc::channel::<Arc<vnt_audio::EncodedFrame>>(4);
    if audio_enabled {
        match audio.subscribe() {
            Ok(mut subscription) => {
                let portable_audio = portable.clone();
                let audio_video_stopped = video_stopped.clone();
                let control = control_tx.clone();
                let udp = udp.clone();
                let peer = udp_peer.clone();
                let fec = fec.clone();
                resources.tasks.push(tokio::spawn(async move {
                    let initial = subscription.state.borrow_and_update().clone();
                    if control.send(ControlMessage::AudioState(initial)).await.is_err() { return; }
                    let mut send_failed = false;
                    loop {
                        tokio::select! {
                            biased;
                            changed = subscription.state.changed() => {
                                if changed.is_err() { break; }
                                let state = subscription.state.borrow_and_update().clone();
                                if control.send(ControlMessage::AudioState(state)).await.is_err() { break; }
                            }
                            frame = subscription.frames.recv() => {
                                let frame = match frame {
                                    Ok(frame) => frame,
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(_) => break,
                                };
                                if portable_audio.as_ref().is_some_and(|p| !session_ready(p,&audio_video_stopped) || frame.captured_at < *p.invalid_before.lock().unwrap()) { continue; }
                                if frame.captured_at.elapsed() > Duration::from_millis(60) { continue; }
                                if transport == vnt_proto::Transport::Tcp {
                                    // Drop on backpressure; never wait behind desktop/file data.
                                    let _ = audio_tx.try_send(frame);
                                } else if let Some(target) = *peer.read().await {
                                    {
                                        match tokio::time::timeout(Duration::from_millis(10), vnt_net::udp::send_audio(udp.as_ref().unwrap(),target,session_id,&frame.packet,fec.as_ref())).await {
                                            Ok(Err(error)) if !send_failed => {
                                                send_failed = true;
                                                let _ = control.send(ControlMessage::AudioState(pb::AudioState {
                                                    generation: frame.packet.generation, available: false,
                                                    reason: format!("音频发送失败：{error}").chars().take(250).collect(),
                                                })).await;
                                            }
                                            Ok(Ok(_)) if send_failed => {
                                                send_failed = false;
                                                let state = subscription.state.borrow().clone();
                                                let _ = control.send(ControlMessage::AudioState(state)).await;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }));
            }
            Err(error) => {
                let _ = control_tx.try_send(ControlMessage::AudioState(pb::AudioState {
                    generation: 0,
                    available: false,
                    reason: format!("音频启动失败：{error}"),
                }));
            }
        }
    } else {
        drop(audio_tx);
    }

    // Bounded writes keep backpressure from blocking session shutdown indefinitely.
    let writer_abort = abort_tx.clone();
    let mut net_rx = Some(net_rx);
    let mut tcp_net_rx = if transport == vnt_proto::Transport::Tcp {
        net_rx.take()
    } else {
        None
    };
    // 剪贴板批量通道：按需读取的数据回复。容量小 → 天然背压，分片流量不会
    // 把 Pong/统计/输入回复挤到后面（控制消息在 select 中优先级更高）。
    let (clip_tx, clip_rx) = mpsc::channel::<ControlMessage>(crate::clipboard::OUT_QUEUE);
    // 对端剪贴板消息入口：reader 循环写、服务内的接收任务读
    let (clip_in_tx, clip_in_rx) =
        mpsc::channel::<crate::clipboard::InboundClipboard>(crate::clipboard::INBOUND_QUEUE);
    // 守卫 drop（含握手后的所有退出路径）即注销会话：取消进行中的传输
    let clipboard_guard = clipboard.register(crate::clipboard::SessionSpec {
        capabilities: hello.capabilities & vnt_proto::platform_capabilities(),
        session_id,
        file_tx: clip_tx,
        control_tx: control_tx.clone(),
        in_rx: clip_in_rx,
        session_cancel: CancellationToken::new(),
        sink: sink.clone(),
        // 观看模式：会话注册即禁用——本地复制不下发，对端消息也进不来
        enabled: clipboard_enabled && !view_only,
    });
    let clipboard_key = clipboard_guard.key();
    if !view_only {clipboard.allow_remote_paste(clipboard_key);}
    if let Some(portable) = &portable {
        clipboard.control_changed(clipboard_key, portable.available());
    }
    let control_connected = Arc::new(AtomicBool::new(true));
    let udp_reconfigure = Arc::new(AtomicBool::new(false));
    let mut clip_rx = clip_rx;
    let separate_files = writer.fork();
    let files_managed = separate_files.is_some();
    if let Some(mut files) = separate_files {
        let (_, empty) = mpsc::channel(1);
        let mut incoming = std::mem::replace(&mut clip_rx, empty);
        resources.tasks.push(tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if files.send_owned(message).await.is_err() {
                    break;
                }
            }
        }));
    }
    let (video_configured_tx, video_configured_rx) = tokio::sync::watch::channel(0u64);
    // A managed video producer can wait on its own bounded media lane without
    // parking the control writer (Pong/input/state must survive media pressure).
    if tcp_net_rx.is_some() {
        if let Some(mut media_writer) = writer.fork() {
            let mut videos = tcp_net_rx.take().unwrap();
            let media_portable = portable.clone();
            let media_desktop = service_desktop.clone();
            let media_cancel = resources.pipeline_cancel.clone();
            let media_abort = abort_tx.clone();
            let mut video_configured = video_configured_rx;
            resources.tasks.push(tokio::spawn(async move {
                loop {
                    let frame = tokio::select! {
                        biased;
                        _ = media_cancel.cancelled() => break,
                        frame = videos.recv() => match frame { Some(frame) => frame, None => break },
                    };
                    if !frame.current_for(media_portable.as_deref(),media_desktop.as_deref()) { continue; }
                    // The independent producer must not overtake a StreamReset
                    // which is still waiting in the application control inbox.
                    while *video_configured.borrow_and_update() < frame.config_revision {
                        tokio::select! {
                            biased;
                            _ = media_cancel.cancelled() => return,
                            changed = video_configured.changed() => if changed.is_err() { return; },
                        }
                    }
                    let sent = tokio::select! {
                        biased;
                        _ = media_cancel.cancelled() => break,
                        sent = media_writer.send_owned(frame.into_message(media_desktop.is_some())) => sent,
                    };
                    if sent.is_err() {
                        let _ = media_abort.send(true);
                        break;
                    }
                }
            }));
        }
    }
    let writer_index = resources.tasks.len();
    let writer_portable = portable.clone();
    let writer_video_stopped = video_stopped.clone();
    let writer_desktop = service_desktop.clone();
    resources.tasks.push(tokio::spawn(async move {
        let mut writer = writer;
        let mut control_rx = control_rx;
        let mut clip_rx = clip_rx;
        loop {
            let msg = tokio::select! {
                biased;
                msg = control_rx.recv() => match msg { Some(msg) => msg, None => break },
                frame = audio_rx.recv(), if !audio_rx.is_closed() || !audio_rx.is_empty() => {
                    let Some(frame) = frame else { continue };
                    if writer_portable.as_ref().is_some_and(|p| !session_ready(p,&writer_video_stopped) || frame.captured_at < *p.invalid_before.lock().unwrap()) { continue; }
                    if frame.captured_at.elapsed() > Duration::from_millis(60) { continue; }
                    ControlMessage::AudioFrame((*frame.packet).clone())
                }
                // Randomized inner selection gives file replies and video a
                // fair turn after the reliable-control and audio priorities.
                bulk = async {
                    tokio::select! {
                        video = async {
                            loop {
                                let video = tcp_net_rx.as_mut().unwrap().recv().await;
                                if video.as_ref().is_some_and(|v| !v.current_for(writer_portable.as_deref(),writer_desktop.as_deref())) { continue; }
                                break video;
                            }
                        }, if tcp_net_rx.is_some() => {
                            video.map(|v| v.into_message(writer_desktop.is_some()))
                        }
                        clip = clip_rx.recv(), if !files_managed => clip,
                    }
                }, if tcp_net_rx.is_some() || !files_managed => match bulk { Some(msg) => msg, None => break },
            };
            let ending = matches!(msg, ControlMessage::SessionEnd(_));
            let configures_video=matches!(msg,ControlMessage::StreamReset(_)) || matches!(&msg,ControlMessage::Agent(m) if matches!(m.payload,Some(pb::agent_message::Payload::Config(_))));
 if let ControlMessage::Agent(m)=&msg {
 if matches!(m.payload,Some(pb::agent_message::Payload::Video(_) | pb::agent_message::Payload::Config(_))) && writer_desktop.as_ref().is_some_and(|d|!d.accepts(m.generation)) {
 if configures_video { video_configured_tx.send_modify(|r|*r+=1); } continue;
 }
 }
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), writer.send_owned(msg)).await,
                Ok(Ok(()))
            ) {
                let _ = writer_abort.send(true);
                break;
            }
            if configures_video {
                video_configured_tx.send_modify(|revision| *revision += 1);
            }
            if ending { break; }
        }
    }));

    // UDP sender（分片发送视频帧）
    if transport != vnt_proto::Transport::Tcp {
        let net_rx = net_rx.take().unwrap();
        let udp = udp.clone().unwrap();
        let udp_peer = udp_peer.clone();
        let sent_stats = sent_stats.clone();
        let keyframe_gate = keyframe_gate.clone();
        let udp_reconfigure = udp_reconfigure.clone();
        let udp_portable = portable.clone();
        let udp_desktop = service_desktop.clone();
        let fec = fec.clone();
        let video_repair = video_repair.clone();
        resources.tasks.push(tokio::spawn(async move {
            let mut net_rx = net_rx;
            let mut waiting_keyframe = false;
            let mut expected_seq = None;
            let mut wire_seq = vnt_net::udp::VideoSequence::default();

            let mut sender = vnt_net::udp::UdpFrameSender::default();
            sender.set_fec(fec);
            sender.set_repair(video_repair);
            while let Some(v) = net_rx.recv().await {
                if !v.current_for(udp_portable.as_deref(), udp_desktop.as_deref()) {
                    waiting_keyframe = true;
                    continue;
                }
                if udp_reconfigure.load(Ordering::Acquire) {
                    continue;
                }
                let Some(target) = *udp_peer.read().await else {
                    continue; // 客户端尚未绑定 UDP，丢弃（保留最近关键帧更好，v1 简化）
                };
                if v.dependencies.is_none() && expected_seq.is_some_and(|seq| seq != v.seq) {
                    waiting_keyframe = true;
                }
                expected_seq = Some(v.seq.wrapping_add(1));
                if v.dependencies.is_none() && waiting_keyframe && !v.keyframe {
                    keyframe_gate.request(Instant::now());
                    continue;
                }
                if let Err(error) = sender
                    .send_encoded_frame(
                        &udp,
                        target,
                        session_id,
                        wire_seq.take(),
                        v.keyframe,
                        v.pts_ms,
                        &v.data,
                        v.dependencies.as_ref(),
                        v.bitrate_kbps.saturating_mul(2),
                    )
                    .await
                {
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) {
                        if v.dependencies.is_none() {
                            waiting_keyframe = true;
                            keyframe_gate.request(Instant::now());
                        }
                        continue;
                    }
                    tracing::error!("UDP 视频发送失败: {error}");
                    break;
                }
                waiting_keyframe = false;
                let mut sent = sent_stats.0.lock().unwrap();
                sent.0 += 1;
                sent.1 += v.data.len() as u64;
            }
        }));
    }

    if let Some(portable) = portable.as_ref().filter(|p| p.region.is_some()) {
        resources
            .tasks
            .push(tokio::spawn(portable.clone().watch(None, stop_rx.clone())));
    }
    // 输入注入器（会话槽已注册，此后的失败都必须走清理路径，不能 ? 提前返回）
    // 观看模式不注入键鼠：用空注入器占位，会话清理路径（held 释放等）保持
    // 统一，且注入器不可用的机器也依然可以共享屏幕
    let input_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let input_ticket = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let injector: Arc<dyn vnt_input::InputInjector + Send + Sync> = if view_only || cfg.synthetic {
        Arc::new(NoopInjector)
    } else {
        #[cfg(windows)]
        let result = if let Some(desktop) = &service_desktop {
            crate::windows::input::DesktopInjector::for_switching(
                desktop.clone(),
                input_generation.clone(),
                input_ticket.clone(),
            )
        } else if let Some(region) = cfg.region.as_ref().filter(|_| portable.is_some()) {
            crate::windows::input::DesktopInjector::for_user_region(region.clone())
        } else if portable.is_some() {
            crate::windows::input::DesktopInjector::for_user_monitor(monitor.hmonitor as isize)
        } else {
            crate::windows::input::DesktopInjector::for_monitor(monitor.hmonitor as isize)
        };
        #[cfg(target_os = "macos")]
        let result = vnt_input::MacInputInjector::for_display(monitor.index);
        match result {
            Ok(injector) => Arc::new(injector),
            Err(error) => {
                let _ = control_tx
                    .send(ControlMessage::SessionEnd(pb::SessionEnd {
                        reason: format!("输入线程不可用：{error}"),
                    }))
                    .await;
                tracing::error!("输入线程不可用：{error}");
                return Ok(());
            }
        }
    };

    let injector: Arc<dyn vnt_input::InputInjector + Send + Sync> =
        if let Some(region) = &cfg.region {
            Arc::new(crate::region::RegionInjector {
                inner: injector,
                region: region.clone(),
            })
        } else {
            injector
        };
    let _cursor = if snapshot.is_some() || service_desktop.is_some() {
        let tx = control_tx.clone();
        vnt_capture::CursorObserver::start(monitor.name.clone(), move |sample| {
            let _ = tx.try_send(ControlMessage::CursorPosition(pb::CursorPosition {
                x: sample.x,
                y: sample.y,
                available: sample.available,
                monitor_x: sample.monitor_x,
                monitor_y: sample.monitor_y,
                monitor_width: sample.monitor_width,
                monitor_height: sample.monitor_height,
            }));
        })
    } else {
        None
    };
    // 每个会话只持有独立编码器，抓屏源在服务内共享。
    let injector: Arc<dyn vnt_input::InputInjector + Send + Sync> =
        Arc::new(SessionInputInjector {
            desktop: service_desktop.clone(),
            id: session_id,
            inner: injector,
            owners: input_owners.clone(),
        });

    if let Some(desktop) = &service_desktop {
        let desktop = desktop.clone();
        let input_generation = input_generation.clone();
        let tx = control_tx.clone();
        let cancel = resources.pipeline_cancel.clone();
        resources.tasks.push(tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(50));
            let mut previous = None;
            loop {
                tokio::select! {_=cancel.cancelled()=>break,_=timer.tick()=>{}}
                let snapshot = desktop.snapshot();
                let input_ready =
                    view_only || input_generation.load(Ordering::Acquire) == snapshot.generation;
                if snapshot.capture_ready
                    && !input_ready
                    && snapshot.changed_at.elapsed() >= Duration::from_secs(10)
                {
                    desktop.request_restart();
                }
                let next = (snapshot.generation, snapshot.capture_ready, input_ready);
                if previous == Some(next) {
                    continue;
                }
                previous = Some(next);
                let state = pb::AgentDesktop {
                    desktop: snapshot.desktop.unwrap_or_default(),
                    capture_ready: snapshot.capture_ready,
                    input_ready,
                    reason: snapshot.error.chars().take(256).collect(),
                    monitors_json: serde_json::to_vec(&snapshot.monitors).unwrap_or_default(),
                };
                if tx
                    .send(agent_message(
                        snapshot.generation,
                        pb::agent_message::Payload::Desktop(state),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    if let Some(portable) = &portable {
        let _ = control_tx
            .send(portable.state(false, 0, "等待普通桌面画面"))
            .await;
    }
    let expected_monitor = snapshot
        .as_ref()
        .and_then(|saved| saved.selected_targets.first())
        .and_then(|target| match target {
            crate::config::CaptureTarget::Monitor { name } => Some(name.clone()),
            _ => None,
        });
    let mut pipeline = Some({
        let service_desktop = service_desktop.clone();
        let portable = portable.clone();
        let region = cfg.region.clone();
        let codec = cap.codec;
        let fps = cfg.fps;
        let bitrate = cfg.bitrate_kbps;
        let hardware_encode = cfg.hardware_encode;
        let capture_session = capture_session.clone();
        let keyframe_gate = keyframe_gate.clone();
        let receiver_distress = receiver_distress.clone();
        let feedback = feedback.clone();
        let background = background.clone();
        let pipeline_control_tx = control_tx.clone();
        let pipeline_peer = udp_peer.clone();
        let control_connected = control_connected.clone();
        let udp_reconfigure = udp_reconfigure.clone();
        let net_tx = net_tx.clone();
        let abort_tx = abort_tx.clone();
        let pipeline_cancel = resources.pipeline_cancel.clone();
        let runtime = tokio::runtime::Handle::current();
        match std::thread::Builder::new()
            .name("server-pipeline".into())
            .spawn(move || {
                let _on_exit = PipelineExit(abort_tx.clone(), capture_session.clone());
                // A successful TCP handshake alone must not allocate a capture
                // and GPU encoder in hybrid mode. TCP video is ready after Welcome.
                while transport != vnt_proto::Transport::Tcp
                    && pipeline_peer
                        .try_read()
                        .map(|p| p.is_none())
                        .unwrap_or(true)
                {
                    if capture_session.is_cancelled() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if capture_session.is_cancelled() {
                    return;
                }
                pipeline_loop(PipelineContext {
                    expected_monitor,
                    region,
                    service_desktop,
                    portable,
                    scale_target,
                    color_depth,
                    control_connected,
                    udp_reconfigure,
                    adapt_local_wait: transport == vnt_proto::Transport::Tcp && files_managed,
                    codec,
                    capture,
                    fps,
                    bitrate_kbps: bitrate,
                    hardware_encode,
                    capture_session,
                    keyframe_gate,
                    receiver_distress,
                    feedback,
                    background,
                    sent_stats,
                    control_tx: pipeline_control_tx,
                    net_tx,
                    abort_tx,
                    pipeline_cancel,
                    runtime,
                });
            }) {
            Ok(h) => h,
            Err(e) => {
                // 线程创建失败：按会话结束清理，避免卡死客户端
                let _ = control_tx
                    .send(ControlMessage::SessionEnd(pb::SessionEnd {
                        reason: format!("管线线程创建失败: {e}"),
                    }))
                    .await;
                return Ok(());
            }
        }
    });

    // 服务停止必须直接取消订阅唤醒管线：reader 循环可能长时间阻塞在
    // 控制通道发送上（客户端不收包时 TCP 写阻塞、通道打满），不能依赖
    // 它中转停止信号
    let server_stop_cancel = capture_session.clone();
    let pipeline_stop_cancel = resources.pipeline_cancel.clone();
    let mut watcher_stop_rx = stop_rx.clone();
    resources.tasks.push(tokio::spawn(async move {
        if !*watcher_stop_rx.borrow() {
            // Ok=服务已停止；Err=Server 已 drop（发送端消失），同样收尾
            let _ = watcher_stop_rx.changed().await;
        }
        server_stop_cancel.cancel();
        pipeline_stop_cancel.cancel();
    }));

    // 会话内按住状态：连接异常中断时（控制端崩溃/断网）释放全部，
    // 避免远端卡键/卡鼠标
    let mut held = HeldInput::default();
    let mut held_generation = 0;
    let mut input_available = None;
    let mut last_input_error = Instant::now();
    let mut desktop_rx = portable.as_ref().map(|p| p.ready.subscribe());

    // ---- TCP reader 主循环 ----
    // 同时感知：客户端消息（带 2s 轮询超时做存活判断）、管线故障、服务停止
    let mut reader = reader;
    let mut abort_rx = abort_rx;
    let mut last_activity = tokio::time::Instant::now();
    let session_started = last_activity;
    // 客户端 Ping（2s 一次）驱动存活判断：连续 30s 无任何消息视为
    // 半开/僵死连接，主动收尾以释放会话槽与按住的键
    const SERVER_LIVENESS: Duration = Duration::from_secs(30);
    let reason = loop {
        if transport != vnt_proto::Transport::Tcp
            && session_started.elapsed() >= Duration::from_secs(10)
            && udp_peer.read().await.is_none()
        {
            let reason = "客户端未建立 UDP 视频通道";
            let _ = control_tx.try_send(ControlMessage::SessionEnd(pb::SessionEnd {
                reason: reason.into(),
            }));
            break reason.to_string();
        }
        let msg: Option<Result<Option<ControlMessage>, std::io::Error>> = tokio::select! {
            _ = async { if let Some(rx) = &mut desktop_rx { let _ = rx.changed().await; } else { std::future::pending::<()>().await; } } => {
                if let Some(portable) = &portable {
                    clipboard.control_changed(clipboard_key,
                        session_ready(portable,&video_stopped) && control_connected.load(Ordering::Acquire));
                    if !session_ready(portable,&video_stopped) {
                        held.release_all(&*injector);
                        let _ = control_tx.send(portable.state(false, 0, if portable.region.is_some() { "屏幕区域暂不可用，已暂停画面与输入，恢复原桌面和显示模式后自动继续" } else { "普通桌面暂不可访问，免安装版已暂停，返回桌面后自动恢复" })).await;
                    } else {
                        held.release_all(&*injector);
                        held = HeldInput::default();
                        keyframe_gate.request_urgent();
                    }
                }
                continue;
            },
            recv = tokio::time::timeout(Duration::from_secs(2), reader.recv()) => match recv {
                Ok(m) => Some(m),
                Err(_) => {
                    // 超时：仅做存活检查，不视为断连
                    if last_activity.elapsed() > SERVER_LIVENESS {
                        break "客户端无响应（心跳超时）".to_string();
                    }
                    None
                }
            },
            res = abort_rx.changed() => {
                let aborted = matches!(res, Ok(())) && *abort_rx.borrow() || res.is_err();
                if aborted {
                    let _ = control_tx
                        .send(ControlMessage::SessionEnd(pb::SessionEnd {
                            reason: "服务端推流故障".into(),
                        }))
                        .await;
                    break "服务端推流故障".to_string();
                }
                None
            }
            res = stop_rx.changed() => {
                let stopped = matches!(res, Ok(())) && *stop_rx.borrow() || res.is_err();
                if stopped {
                    let _ = control_tx
                        .send(ControlMessage::SessionEnd(pb::SessionEnd {
                            reason: "服务已停止".into(),
                        }))
                        .await;
                    break "服务已停止".to_string();
                }
                None
            }
        };
        let Some(msg) = msg else { continue };
        last_activity = tokio::time::Instant::now();
        let mut input_epoch = 0;
        let msg = match msg {
            Ok(Some(ControlMessage::Agent(message))) if service_desktop.is_some() => {
                let desktop = service_desktop.as_ref().unwrap();
                if !desktop.accepts(message.generation)
                    || input_generation.load(Ordering::Acquire) != message.generation
                {
                    continue;
                }
                if let Some(pb::agent_message::Payload::Input(event)) = message.payload {
                    input_epoch = message.generation;
                    Ok(Some(ControlMessage::InputEvent(event)))
                } else {
                    continue;
                }
            }
            Ok(Some(ControlMessage::InputEvent(_))) if service_desktop.is_some() => continue,
            other => other,
        };
        if service_desktop
            .as_ref()
            .is_some_and(|d| !d.accepts(held_generation))
        {
            held = HeldInput::default();
            held_generation = service_desktop.as_ref().unwrap().snapshot().generation;
        }
        match msg {
            Ok(Some(ControlMessage::StopVideo(stop))) if !stop.acknowledged => {
                video_stopped.store(true, Ordering::Release);
                if let Some(portable) = &portable {
                    clipboard.control_changed(
                        clipboard_key,
                        session_ready(portable, &video_stopped)
                            && control_connected.load(Ordering::Acquire),
                    );
                }
                capture_session.cancel();
                resources.pipeline_cancel.cancel();
                if let Some(thread) = pipeline.take() {
                    let _ = tokio::task::spawn_blocking(move || thread.join()).await;
                }
                let _ = control_tx
                    .send(ControlMessage::StopVideo(pb::StopVideo {
                        acknowledged: true,
                    }))
                    .await;
            }
            Ok(Some(ControlMessage::ResetInput(_))) => {
                clipboard.cancel_remote_paste(clipboard_key);
                held.release_all(&*injector);
            }
            Ok(Some(ControlMessage::TransportState(state))) => {
                if state.kind == 0 {
                    control_connected.store(state.available, Ordering::Release);
                    clipboard.control_changed(clipboard_key, state.available);
                    if !state.available {
                        held.release_all(&*injector);
                    } else {
                        udp_reconfigure.store(false, Ordering::Release);
                        keyframe_gate.request_urgent();
                    }
                }
                if state.kind == 1 {
                    clipboard.transport_changed(clipboard_key, state.available);
                }
            }
            Ok(Some(ControlMessage::InputEvent(ev))) => {
                // 观看模式：输入只观看不注入（校验也一并跳过）
                if !view_only
                    && portable
                        .as_ref()
                        .is_none_or(|p| session_ready(p, &video_stopped))
                {
                    if service_desktop.is_some() && held_generation != input_epoch {
                        held = HeldInput::default();
                        held_generation = input_epoch;
                    }
                    input_ticket.store(input_epoch, Ordering::Release);
                    let result = apply_input(&*injector, &ev, &mut held);
                    let (available, reason) = match result {
                        InputOutcome::Applied => (true, String::new()),
                        InputOutcome::Rejected => continue,
                        InputOutcome::Failed(error) => (false, format!("键鼠输入失败：{error}")),
                    };
                    if input_available != Some(available)
                        || (!available && last_input_error.elapsed() >= Duration::from_secs(5))
                    {
                        if !available {
                            last_input_error = Instant::now();
                        }
                        let state = pb::DesktopState {
                            ready: true,
                            reason,
                            ..Default::default()
                        };
                        let message = if service_desktop.is_some() {
                            agent_message(
                                input_epoch,
                                pb::agent_message::Payload::InputFeedback(state),
                            )
                        } else {
                            ControlMessage::DesktopState(state)
                        };
                        let _ = control_tx.send(message).await;
                    }
                    input_available = Some(available);
                }
            }
            Ok(Some(ControlMessage::ScreenThrottle(state))) => {
                if background.swap(state.background, Ordering::AcqRel) && !state.background {
                    keyframe_gate.request_urgent();
                }
            }
            Ok(Some(ControlMessage::LinkFeedback(report))) => {
                if let Some(fec) = &fec {
                    fec.feedback(&report);
                }
                feedback.publish(report, Instant::now());
            }
            Ok(Some(ControlMessage::VideoNack(nack))) => {
                if nack.stream_id == 0 {
                    if let Some(repair) = &video_repair {
                        repair.request(nack);
                    }
                }
            }
            Ok(Some(ControlMessage::RequestKeyframe)) => {
                // 统一走闸门节流；重复请求且 IDR 欠太久 → 记为拥塞证据，
                // 驱动管线降码率（链路在丢包但发送队列未必打得满）
                if keyframe_gate.request(Instant::now()) {
                    receiver_distress.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(Some(ControlMessage::Ping(p))) => {
                let _ = control_tx
                    .send(ControlMessage::Pong(pb::Pong {
                        timestamp_ms: p.timestamp_ms,
                    }))
                    .await;
            }
            Ok(Some(ControlMessage::SendSas)) => {
                // Only the service broker can send SAS. Unsupported commands must
                // not change desktop readiness or trigger video recovery.
                tracing::debug!("忽略非服务被控端的 Ctrl+Alt+Del 请求");
            }
            Ok(Some(
                msg @ (ControlMessage::ClipboardImageReady(_)
                | ControlMessage::ClipboardImage(_)
                | ControlMessage::ClipboardImageChunk(_)
                | ControlMessage::ClipboardImageCancel(_)
                | ControlMessage::ClipboardText(_)
                | ControlMessage::ClipboardPaste(_)
                | ControlMessage::ClipboardOffer(_)
                | ControlMessage::ClipboardRequest(_)
                | ControlMessage::ClipboardReply(_)),
            )) => {
                // 观看模式不同步剪贴板：对端消息直接丢弃（会话注册也已禁用，
                // 这里再挡一道，省去无效的通道转发）
                if !view_only
                    && portable
                        .as_ref()
                        .is_none_or(|p| session_ready(p, &video_stopped))
                {
                    dispatch_clipboard(&clipboard, clipboard_key, &clip_in_tx, msg).await;
                }
            }
            Ok(Some(ControlMessage::SessionEnd(se))) => break se.reason,
            Ok(Some(_)) => {}
            Ok(None) => break "客户端断开".to_string(),
            Err(e) => break format!("连接错误: {e}"),
        }
    };

    // ---- 清理 ----
    held.release_all(&*injector);
    // 注销剪贴板会话：结束操作并释放源文件句柄
    drop(clipboard_guard);
    // All background tasks are owned by resources; normal and aborted exits cancel them.
    // 取消订阅：直接唤醒阻塞在收帧上的管线线程
    capture_session.cancel();
    resources.pipeline_cancel.cancel();
    registration.reason = reason.clone();
    drop(registration);
    drop(net_tx);
    drop(control_tx);
    // 管线线程由订阅取消自行退出（阻塞 join 会卡死异步 worker，
    // 且 abort 无法打断同步等待，曾导致 stop() 后进程挂起）
    drop(pipeline);
    // Give the writer a bounded chance to deliver SessionEnd, then close its TCP half.
    let mut writer_task = resources.tasks.remove(writer_index);
    if tokio::time::timeout(Duration::from_millis(300), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
        let _ = writer_task.await;
    }
    // Release UDP socket clones before a configuration change rebinds the port.
    for task in resources.tasks.drain(..) {
        task.abort();
        let _ = task.await;
    }
    tracing::info!("客户端 {peer} 会话结束: {reason}");
    Ok(())
}

#[derive(Default)]
struct SentStats(
    std::sync::Mutex<(u64, u64)>,
    Option<vnt_net::traffic::TrafficCounters>,
);

impl SentStats {
    fn take(&self) -> (u64, u64) {
        let mut previous = self.0.lock().unwrap();
        if let Some(traffic) = &self.1 {
            let current = traffic.video_snapshot();
            let delta = (
                current.0.wrapping_sub(previous.0),
                current.1.wrapping_sub(previous.1),
            );
            *previous = current;
            delta
        } else {
            std::mem::take(&mut *previous)
        }
    }
}

/// 把对端剪贴板消息转交接收任务。
///
/// 失败只记日志：reader 循环必须继续分发输入事件，不能因剪贴板积压卡住。
async fn dispatch_clipboard(
    clipboard: &crate::clipboard::ClipboardService,
    connection_key: u64,
    tx: &mpsc::Sender<crate::clipboard::InboundClipboard>,
    message: crate::clipboard::InboundClipboard,
) {
    if let Err(e) = crate::clipboard::dispatch_inbound(clipboard, connection_key, tx, message).await
    {
        tracing::debug!("剪贴板消息分发失败: {e}");
    }
}

pub(crate) fn agent_message(
    generation: u64,
    payload: pb::agent_message::Payload,
) -> ControlMessage {
    ControlMessage::Agent(pb::AgentMessage {
        generation,
        payload: Some(payload),
    })
}
fn stream_config(local: bool, generation: u64, config: pb::StreamReset) -> ControlMessage {
    if local {
        agent_message(generation, pb::agent_message::Payload::Config(config))
    } else {
        ControlMessage::StreamReset(config)
    }
}
/// 管线输出帧。
struct VideoOut {
    dependencies: Option<pb::VideoDependencies>,
    config_revision: u64,
    desktop_generation: u64,
    // Follow adaptive encoder changes, with 2x peak headroom for keyframes.
    bitrate_kbps: u32,
    seq: u16,
    keyframe: bool,
    pts_ms: u32,
    data: Vec<u8>,
}

impl VideoOut {
    fn into_message(self, local: bool) -> ControlMessage {
        let frame = pb::VideoFrame {
            dependencies: self.dependencies,
            keyframe: self.keyframe,
            pts_ms: self.pts_ms,
            data: self.data,
        };
        if local {
            agent_message(
                self.desktop_generation,
                pb::agent_message::Payload::Video(frame),
            )
        } else {
            ControlMessage::VideoFrame(frame)
        }
    }
    fn current_for(
        &self,
        portable: Option<&portable::Portable>,
        desktop: Option<&vnt_capture::DesktopCapture>,
    ) -> bool {
        if desktop.is_some_and(|d| !d.accepts(self.desktop_generation)) {
            return false;
        }
        portable.is_none_or(|p| {
            p.available() && self.desktop_generation == p.generation.load(Ordering::Acquire) as u64
        })
    }
}

/// 共享帧订阅 + 独立编码循环（专用线程）。
/// 会话停止或服务级停止都经订阅的取消信号直接唤醒（无轮询睡眠）；
/// 采集端退出（排空残余帧后）或下行通道关闭同样立即退出，
/// 防止无消费者时空转。
struct PipelineContext {
    expected_monitor: Option<String>,
    region: Option<vnt_capture::ScreenRegion>,
    service_desktop: Option<Arc<vnt_capture::DesktopCapture>>,
    portable: Option<Arc<portable::Portable>>,
    scale_target: Option<(u32, u32)>,
    color_depth: u32,
    control_connected: Arc<AtomicBool>,
    udp_reconfigure: Arc<AtomicBool>,
    // Agent IPC always uses TCP framing, including when the Broker paces UDP.
    // Only an actual managed TCP network writer can infer pressure from waits.
    adapt_local_wait: bool,
    codec: vnt_proto::Codec,
    capture: SharedCapture,
    fps: u32,
    bitrate_kbps: u32,
    /// 硬件编码开关：透传给 `vnt_codec::create_encoder`。
    hardware_encode: bool,
    /// 会话独立的订阅取消状态，可在订阅创建前取消。
    capture_session: SessionCapture,
    /// 关键帧闸门（与会话 reader 共享）：节流所有关键帧请求。
    keyframe_gate: Arc<KeyframeGate>,
    /// 接收端"长时间拿不到 IDR"的累计次数（reader 写、管线采样）。
    receiver_distress: Arc<AtomicU32>,
    feedback: Arc<FeedbackInbox>,
    background: Arc<AtomicBool>,
    sent_stats: Arc<SentStats>,
    control_tx: mpsc::Sender<ControlMessage>,
    net_tx: mpsc::Sender<VideoOut>,
    abort_tx: tokio::sync::watch::Sender<bool>,
    pipeline_cancel: CancellationToken,
    runtime: tokio::runtime::Handle,
}

fn pipeline_loop(context: PipelineContext) {
    let _timer = vnt_net::timing::TimerResolution::acquire();
    let PipelineContext {
        expected_monitor,
        region,
        service_desktop,
        portable,
        scale_target,
        color_depth,
        control_connected,
        udp_reconfigure,
        adapt_local_wait,
        codec,
        capture,
        fps,
        bitrate_kbps,
        mut hardware_encode,
        capture_session,
        keyframe_gate,
        receiver_distress,
        feedback,
        background,
        sent_stats,
        control_tx,
        net_tx,
        abort_tx,
        pipeline_cancel,
        runtime,
    } = context;

    /// 致命错误：通知会话结束（否则客户端永久等待、会话槽被占用）
    macro_rules! fatal {
        ($($arg:tt)*) => {{
            tracing::error!($($arg)*);
            let _ = abort_tx.send(true);
        }};
    }

    let mut subscription = match capture.subscribe(fps) {
        Ok(subscription) => subscription,
        Err(e) => {
            fatal!("订阅共享抓屏失败: {e}");
            return;
        }
    };

    if !capture_session.attach(subscription.cancel_handle()) {
        return;
    }

    let mut encoder: Box<dyn VideoEncoder> = match vnt_codec::create_encoder(codec, hardware_encode)
    {
        Ok(e) => e,
        Err(e) => {
            fatal!("创建编码器失败: {e}");
            drop(subscription);
            return;
        }
    };
    let mut cur_params: Option<EncodeParamsNow> = None;
    tracing::info!(
        encoder = encoder.name(),
        hardware = encoder.is_hardware(),
        ?codec,
        "视频管线使用编码器"
    );
    // 发送拥塞自适应：队列持续打满 / 接收端长时间拿不到 IDR 时降码率
    let mut congestion = SendCongestion::new(bitrate_kbps, fps);
    let mut cur_bitrate = bitrate_kbps;
    let mut cur_fps = fps;
    let mut first_capture_pts = None;
    let mut encode_failures = 0;
    let mut configure_failures = 0;
    let mut video_seq: u16 = 0;
    let mut config_revision = 0u64;
    let mut dependencies = vnt_codec::dependencies::FrameTracker::new(codec);
    let mut last_stats = std::time::Instant::now();
    let mut stats_frames: u32 = 0;
    let mut stats_inputs: u32 = 0;
    let mut stats_encode_ms: u64 = 0;
    let mut stats_capture_ms: u64 = 0;
    let mut stats_dropped: u64 = 0; // 已见淘汰帧总数（帧队列 dropped_frames 水位）
    let mut stats_send_wait = Duration::ZERO;
    let mut stats_queue_peak = 0;
    let mut stats_keyframes = 0u32;
    let mut stats_config_skipped = 0u64;
    let mut stats_distress: u64 = 0; // 本秒接收端"拿不到 IDR"求助次数
    let mut distress_seen: u32 = 0; // 已处理的接收端求助累计值

    // 帧到达、采集端退出、取消信号均由帧队列直接唤醒，无睡眠轮询；
    // 取消时残余帧不再交付
    let mut cropper = crate::region::Cropper::default();
    let mut scaler = crate::resolution::Scaler::new(scale_target);
    let mut colors = crate::color_depth::Quantizer::new(color_depth);
    let session_clock = Instant::now();
    let mut desktop_generation = 0;
    loop {
        // Wait *before* taking a raw frame. The capture mailbox keeps only the
        // newest pixels while the network is busy; encoded references survive.
        let Some((permit, waited)) = video::reserve_output(&runtime, &net_tx, &pipeline_cancel)
        else {
            break;
        };
        stats_send_wait += waited;
        // UDP queue waits include our intentional byte pacing, which says
        // nothing about link capacity. Reducing bitrate from that wait creates
        // a self-throttling feedback loop. UDP loss/socket recovery is separate.
        if adapt_local_wait {
            congestion.on_send_wait(Instant::now(), waited);
        }
        let Some(source_frame) = subscription.recv() else {
            break;
        };
        // 原始帧只被本管线持有：编码后（或提前 continue/break 时）随之释放，
        // 不归还采集端、不做缓冲复用
        if portable.as_ref().is_some_and(|p| {
            !p.available()
                || !p.normal_desktop()
                || source_frame.captured_at < *p.invalid_before.lock().unwrap()
        }) {
            continue;
        }
        if let Some(desktop) = &service_desktop {
            let Some(frame_generation) = desktop.frame_generation(&source_frame) else {
                continue;
            };
            if frame_generation != desktop_generation {
                desktop_generation = frame_generation;
                match vnt_codec::create_encoder(codec, hardware_encode) {
                    Ok(fresh) => encoder = fresh,
                    Err(error) => {
                        fatal!("恢复编码器失败: {error}");
                        break;
                    }
                }
                cur_params = None;
                keyframe_gate.request_urgent();
                let snapshot = desktop.snapshot();
                if snapshot.generation != desktop_generation {
                    continue;
                }
                let state = pb::AgentDesktop {
                    desktop: snapshot.desktop.unwrap_or_default(),
                    capture_ready: true,
                    input_ready: false,
                    reason: String::new(),
                    monitors_json: serde_json::to_vec(&snapshot.monitors).unwrap_or_default(),
                };
                if control_tx
                    .blocking_send(agent_message(
                        desktop_generation,
                        pb::agent_message::Payload::Desktop(state),
                    ))
                    .is_err()
                {
                    break;
                }
            }
        }
        if expected_monitor.as_ref().is_some_and(|name| {
            source_frame
                .source_monitor
                .as_ref()
                .is_some_and(|m| &m.name != name)
        }) {
            continue;
        }
        let Some(cropped) = cropper.process(&source_frame, region.as_ref()) else {
            continue;
        };
        let frame = scaler.process(cropped);
        let now = Instant::now();
        // Shared capture may predate this session. Keep the wire timestamps
        // relative to the first frame received by this encoder.
        let pts_ms = if portable.is_some() || service_desktop.is_some() {
            session_clock.elapsed().as_millis() as u64
        } else {
            frame
                .pts_ms
                .saturating_sub(*first_capture_pts.get_or_insert(frame.pts_ms))
        };
        if let Some(portable) = &portable {
            let generation = portable.generation.load(Ordering::Acquire);
            if generation as u64 != desktop_generation {
                desktop_generation = generation as u64;
                match vnt_codec::create_encoder(codec, hardware_encode) {
                    Ok(fresh) => encoder = fresh,
                    Err(error) => {
                        fatal!("恢复编码器失败: {error}");
                        break;
                    }
                }
                cur_params = None;
                keyframe_gate.request_urgent();
                if control_tx
                    .blocking_send(portable.state(true, pts_ms as u32, ""))
                    .is_err()
                {
                    break;
                }
            }
        }

        // Each stream protects the actual encoded rectangle, including dynamic
        // display changes and the result of region cropping/software scaling.
        congestion.set_resolution(frame.width, frame.height);
        if let Some((report, received_at)) = feedback.take() {
            congestion.on_feedback(report, received_at);
        }
        congestion.tick(now);
        congestion.set_background(background.load(Ordering::Acquire));

        // 接收端长时间拿不到关键帧（重复请求）：与队列打满等同的拥塞证据，
        // 覆盖"发送侧队列不满、但链路在丢包"的情形
        let distress = receiver_distress.load(Ordering::Relaxed);
        if distress != distress_seen {
            stats_distress += distress.wrapping_sub(distress_seen) as u64;
            distress_seen = distress;
            congestion.on_pressure(now);
        }

        // 分辨率变化 / 首帧：重配编码器并通知客户端
        let dims_changed = cur_params
            .as_ref()
            .map(|p| p.width != frame.width || p.height != frame.height)
            .unwrap_or(true);
        if dims_changed {
            if !control_connected.load(Ordering::Acquire) {
                udp_reconfigure.store(true, Ordering::Release);
            }
            let target = congestion.target();
            let params =
                vnt_codec::EncodeParams::new(frame.width, frame.height, target.fps, target.kbps);
            let mut configured =
                configure_for_frame(encoder.as_mut(), &params, &mut configure_failures);
            // Drivers can reject valid even region dimensions (for example very small crops).
            // Preserve the negotiated codec and exact visible rectangle with a software encoder.
            if region.is_some() && matches!(configured, Ok(false)) && encoder.is_hardware() {
                if let Ok(mut software) = vnt_codec::create_encoder(codec, false) {
                    if software.configure(&params).is_ok() {
                        tracing::info!(
                            encoder = software.name(),
                            width = params.width,
                            height = params.height,
                            "区域尺寸不受硬件编码器支持，回退同格式软件编码"
                        );
                        encoder = software;
                        hardware_encode = false;
                        configure_failures = 0;
                        configured = Ok(true);
                    }
                }
            }
            match configured {
                Ok(true) => {}
                Ok(false) => {
                    stats_config_skipped += 1;
                    continue;
                }
                Err(e) => {
                    fatal!("编码器连续配置失败: {e}");
                    break;
                }
            }
            if subscription.set_fps(target.fps).is_err() {
                break;
            }
            cur_fps = target.fps;
            cur_bitrate = target.kbps;
            congestion.applied_successfully(target);
            // First capture may already differ from the monitor dimensions in
            // Welcome (a display mode switch can race the handshake).
            if control_tx
                .try_send(stream_config(
                    service_desktop.is_some(),
                    desktop_generation,
                    pb::StreamReset {
                        width: frame.width,
                        height: frame.height,
                        fps: cur_fps,
                        decoder_config: Vec::new(),
                    },
                ))
                .is_err()
            {
                fatal!("流参数更新无法送达，结束会话");
                break;
            }
            config_revision += 1;
            dependencies.reset(config_revision);
            if let Some(old) = cur_params.take() {
                tracing::info!(
                    "分辨率变化 {}x{} → {}x{}",
                    old.width,
                    old.height,
                    frame.width,
                    frame.height
                );
            }
            cur_params = Some(EncodeParamsNow {
                width: frame.width,
                height: frame.height,
            });
            // 客户端要凭这个 IDR 重配解码器：紧急请求，绕过节流
            keyframe_gate.request_urgent();
        }
        // 模式切换回到旧尺寸也代表已恢复，不能累积上次切换的失败次数。
        configure_failures = 0;

        // Commit only after configuration succeeds; failures leave the applied
        // stream and subscription unchanged and are retried at most once/sec.
        if congestion.reconfigure_due(now) {
            let target = congestion.target();
            congestion.attempted(now);
            let params =
                vnt_codec::EncodeParams::new(frame.width, frame.height, target.fps, target.kbps);
            match encoder.configure(&params) {
                Ok(()) => {
                    if target.fps != cur_fps {
                        if !control_connected.load(Ordering::Acquire) {
                            udp_reconfigure.store(true, Ordering::Release);
                        }
                        if subscription.set_fps(target.fps).is_err() {
                            break;
                        }
                        if control_tx
                            .try_send(stream_config(
                                service_desktop.is_some(),
                                desktop_generation,
                                pb::StreamReset {
                                    width: frame.width,
                                    height: frame.height,
                                    fps: target.fps,
                                    decoder_config: Vec::new(),
                                },
                            ))
                            .is_err()
                        {
                            fatal!("自适应流参数无法送达，结束会话");
                            break;
                        }
                        config_revision += 1;
                        dependencies.reset(config_revision);
                        keyframe_gate.request_urgent();
                    }
                    tracing::info!(
                        "自适应调整：{cur_fps}fps/{cur_bitrate}kbps → {}fps/{}kbps，原因：{}",
                        target.fps,
                        target.kbps,
                        congestion.reason
                    );
                    cur_fps = target.fps;
                    cur_bitrate = target.kbps;
                    congestion.applied_successfully(target);
                }
                Err(e) => tracing::warn!(
                    "自适应重配失败，保留 {cur_fps}fps/{cur_bitrate}kbps，至少 1 秒后重试: {e}"
                ),
            }
        }

        // Throttle forced IDRs, not the video stream. A refresh request can be
        // duplicated or arrive after recovery; it does not invalidate frames
        // for receivers whose reference chain is still intact.
        if keyframe_gate.due(now) {
            let _ = encoder.force_keyframe();
        }

        let mut packets = Vec::new();
        let t0 = std::time::Instant::now();
        if service_desktop
            .as_ref()
            .is_some_and(|d| !d.accepts(desktop_generation))
        {
            continue;
        }
        let encode_result = encoder.encode(colors.process(&frame.bgra), pts_ms, &mut packets);
        let encode_ms = t0.elapsed().as_millis() as u64;
        if let Err(e) = encode_result {
            tracing::warn!("编码失败: {e}");
            keyframe_gate.request_urgent(); // A failed output may have broken the reference chain.
            encode_failures += 1;
            if encode_failures >= 5 {
                fatal!("连续编码失败，结束会话");
                break;
            }
            continue;
        }
        encode_failures = 0;
        stats_inputs += 1;
        stats_capture_ms += frame.capture_ms as u64;
        stats_encode_ms += encode_ms;
        // 按实际取到的输出计帧率：异步编码的输出可能延迟到下一轮才取走，
        // 数提交帧数会让上报帧率偏离真实送达速率
        stats_frames += packets.len() as u32;

        let mut downstream_closed = false;
        let mut permit = Some(permit);
        for p in packets {
            if service_desktop
                .as_ref()
                .is_some_and(|d| !d.accepts(desktop_generation))
            {
                break;
            }
            let pts = (p.pts_ms.min(u32::MAX as u64)) as u32;
            if p.data.len() > vnt_proto::udp::MAX_FRAME_BYTES {
                fatal!("编码帧超过传输上限");
                downstream_closed = true;
                break;
            }
            let seq = video_seq;
            video_seq = video_seq.wrapping_add(1);
            let permit = match permit.take() {
                Some(permit) => permit,
                None => {
                    let Some((permit, waited)) =
                        video::reserve_output(&runtime, &net_tx, &pipeline_cancel)
                    else {
                        downstream_closed = true;
                        break;
                    };
                    stats_send_wait += waited;
                    if adapt_local_wait {
                        congestion.on_send_wait(Instant::now(), waited);
                    }
                    permit
                }
            };
            let dependency = dependencies.frame(&p.data, p.keyframe);
            permit.send(VideoOut {
                dependencies: Some(dependency),
                config_revision,
                desktop_generation,
                bitrate_kbps: cur_bitrate,
                seq,
                keyframe: p.keyframe,
                pts_ms: pts,
                data: p.data,
            });
            stats_queue_peak = stats_queue_peak.max(net_tx.max_capacity() - net_tx.capacity());
            if p.keyframe {
                stats_keyframes += 1;
                keyframe_gate.mark_emitted(Instant::now());
            }
        }
        if downstream_closed {
            break;
        }

        // The lease also returns pixels on every continue/error/exit path.
        drop(source_frame);

        // 每秒统计上报
        let el = last_stats.elapsed();
        if el >= Duration::from_secs(1) {
            let secs = el.as_secs_f64();
            let (sent_frames, sent_bytes) = sent_stats.take();
            if control_tx.capacity() > 8 {
                let _ = control_tx.try_send(ControlMessage::StatsReport(pb::StatsReport {
                    fps: (sent_frames as f64 / secs) as u32,
                    bitrate_kbps: (sent_bytes as f64 * 8.0 / 1000.0 / secs) as u32,
                    encode_ms: (stats_encode_ms / stats_inputs.max(1) as u64) as u32,
                    capture_ms: (stats_capture_ms / stats_inputs.max(1) as u64) as u32,
                    target_fps: congestion.target().fps,
                    target_bitrate_kbps: congestion.target().kbps,
                    applied_fps: congestion.applied().fps,
                    applied_bitrate_kbps: congestion.applied().kbps,
                    loss_permille: congestion.loss_permille(now),
                }));
            }
            let cap_dropped = subscription.dropped_frames();
            let target = congestion.target();
            if !stats_send_wait.is_zero()
                || stats_distress > 0
                || cap_dropped > stats_dropped
                || target.fps < fps
                || target.kbps < bitrate_kbps
            {
                // 抓屏邮箱淘汰、发送等待、接收端求助与目标参数。
                tracing::debug!(
                    "管线: 本秒淘汰原始帧 {}、发送等待 {} ms、接收求助 {}、目标 {} fps / {} kbps",
                    cap_dropped - stats_dropped,
                    stats_send_wait.as_millis(),
                    stats_distress,
                    target.fps,
                    target.kbps
                );
            }
            tracing::info!(
                send_wait_ms = stats_send_wait.as_millis() as u64,
                queue_peak = stats_queue_peak,
                raw_dropped = cap_dropped - stats_dropped,
                keyframes = stats_keyframes,
                config_skipped = stats_config_skipped,
                input_fps = stats_inputs as f64 / secs,
                encoded_fps = stats_frames as f64 / secs,
                sent_fps = sent_frames as f64 / secs,
                capture_ms = stats_capture_ms / stats_inputs.max(1) as u64,
                encode_ms = stats_encode_ms / stats_inputs.max(1) as u64,
                "视频发送诊断"
            );
            stats_frames = 0;
            stats_inputs = 0;
            stats_encode_ms = 0;
            stats_capture_ms = 0;
            stats_dropped = cap_dropped;
            stats_send_wait = Duration::ZERO;
            stats_queue_peak = 0;
            stats_keyframes = 0;
            stats_config_skipped = 0;
            stats_distress = 0;
            last_stats = std::time::Instant::now();
        }
    }
    // Remove only this subscriber. The capture source stops with its last lease.
    drop(subscription);
    tracing::info!("管线线程退出");
}

/// 观看模式占位注入器：所有输入都是空操作。
struct NoopInjector;
impl vnt_input::InputInjector for NoopInjector {
    fn mouse_move(&self, _: f32, _: f32) -> vnt_input::InputResult<()> {
        Ok(())
    }
    fn wheel(&self, _: f32, _: f32) -> vnt_input::InputResult<()> {
        Ok(())
    }
    fn mouse_button(&self, _: vnt_input::MouseButton, _: bool) -> vnt_input::InputResult<()> {
        Ok(())
    }
    fn key(&self, _: u16, _: bool, _: bool) -> vnt_input::InputResult<()> {
        Ok(())
    }
}

/// Ownership prevents one departing controller from releasing another's keys.
#[derive(Default)]
struct InputOwners {
    generation: u64,
    keys: HashMap<(u16, bool), std::collections::HashSet<u32>>,
    buttons: HashMap<vnt_input::MouseButton, std::collections::HashSet<u32>>,
}

struct SessionInputInjector {
    desktop: Option<Arc<vnt_capture::DesktopCapture>>,
    id: u32,
    inner: Arc<dyn vnt_input::InputInjector + Send + Sync>,
    owners: Arc<std::sync::Mutex<InputOwners>>,
}

fn owned_input<K: Eq + std::hash::Hash + Copy>(
    owners: &mut HashMap<K, std::collections::HashSet<u32>>,
    key: K,
    id: u32,
    down: bool,
    inject: impl FnOnce() -> vnt_input::InputResult<()>,
) -> vnt_input::InputResult<()> {
    if down {
        inject()?;
        owners.entry(key).or_default().insert(id);
        return Ok(());
    }
    let Some(held) = owners.get_mut(&key) else {
        return Ok(());
    };
    if !held.contains(&id) {
        return Ok(());
    }
    if held.len() > 1 {
        held.remove(&id);
        return Ok(());
    }
    let result = inject();
    if result.is_ok() {
        owners.remove(&key);
    }
    result
}

impl SessionInputInjector {
    fn current_owners(&self) -> std::sync::MutexGuard<'_, InputOwners> {
        let mut owners = self.owners.lock().unwrap();
        if let Some(d) = &self.desktop {
            let generation = d.snapshot().generation;
            if owners.generation != generation {
                owners.keys.clear();
                owners.buttons.clear();
                owners.generation = generation;
            }
        }
        owners
    }
}
impl vnt_input::InputInjector for SessionInputInjector {
    fn mouse_move(&self, x: f32, y: f32) -> vnt_input::InputResult<()> {
        self.inner.mouse_move(x, y)
    }
    fn wheel(&self, dx: f32, dy: f32) -> vnt_input::InputResult<()> {
        self.inner.wheel(dx, dy)
    }
    fn mouse_button(
        &self,
        button: vnt_input::MouseButton,
        down: bool,
    ) -> vnt_input::InputResult<()> {
        owned_input(
            &mut self.current_owners().buttons,
            button,
            self.id,
            down,
            || self.inner.mouse_button(button, down),
        )
    }
    fn key(&self, scancode: u16, extended: bool, down: bool) -> vnt_input::InputResult<()> {
        owned_input(
            &mut self.current_owners().keys,
            (scancode, extended),
            self.id,
            down,
            || self.inner.key(scancode, extended, down),
        )
    }
}

impl Drop for SessionInputInjector {
    fn drop(&mut self) {
        let mut owners = self.current_owners();
        let keys: Vec<_> = owners
            .keys
            .iter()
            .filter(|(_, ids)| ids.contains(&self.id))
            .map(|(&key, _)| key)
            .collect();
        let buttons: Vec<_> = owners
            .buttons
            .iter()
            .filter(|(_, ids)| ids.contains(&self.id))
            .map(|(&button, _)| button)
            .collect();
        for (sc, ext) in keys {
            let _ = owned_input(&mut owners.keys, (sc, ext), self.id, false, || {
                self.inner.key(sc, ext, false)
            });
        }
        for button in buttons {
            let _ = owned_input(&mut owners.buttons, button, self.id, false, || {
                self.inner.mouse_button(button, false)
            });
        }
        // A failed OS release must not leave a dead session as an owner forever.
        owners.keys.retain(|_, ids| {
            ids.remove(&self.id);
            !ids.is_empty()
        });
        owners.buttons.retain(|_, ids| {
            ids.remove(&self.id);
            !ids.is_empty()
        });
    }
}

struct EncodeParamsNow {
    width: u32,
    height: u32,
}

/// 每帧尝试一次；模式切换期间丢弃尺寸不匹配的帧，最多跨后续帧重试两次。
/// 只有成功配置后，调用方才能更新尺寸并发送 StreamReset。
fn configure_for_frame(
    encoder: &mut dyn VideoEncoder,
    params: &vnt_codec::EncodeParams,
    failures: &mut u8,
) -> Result<bool, vnt_codec::EncodeError> {
    match encoder.configure(params) {
        Ok(()) => {
            *failures = 0;
            Ok(true)
        }
        Err(error) => {
            *failures = failures.saturating_add(1);
            if *failures >= 3 {
                return Err(error);
            }
            tracing::warn!(
                "编码器配置 {}x{} 失败（{}/3），等待下一帧重试: {error}",
                params.width,
                params.height,
                failures,
            );
            Ok(false)
        }
    }
}

/// 会话内按住状态（连接异常中断时据此释放，避免远端卡键/卡鼠标）。
#[derive(Default)]
struct HeldInput {
    /// (scancode, extended)
    keys: std::collections::HashSet<(u16, bool)>,
    buttons: std::collections::HashSet<vnt_input::MouseButton>,
}

impl HeldInput {
    fn release_all(&self, injector: &dyn vnt_input::InputInjector) {
        if self.keys.is_empty() && self.buttons.is_empty() {
            return;
        }
        tracing::info!(
            "会话结束，释放 {} 键 / {} 鼠标键",
            self.keys.len(),
            self.buttons.len()
        );
        for &(sc, ext) in &self.keys {
            let _ = injector.key(sc, ext, false);
        }
        for &b in &self.buttons {
            let _ = injector.mouse_button(b, false);
        }
    }
}

enum InputOutcome {
    Applied,
    Rejected,
    Failed(String),
}

fn apply_input(
    injector: &dyn vnt_input::InputInjector,
    ev: &pb::InputEvent,
    held: &mut HeldInput,
) -> InputOutcome {
    if !vnt_proto::validation::valid_input(ev) {
        return InputOutcome::Rejected;
    }
    use vnt_input::MouseButton;
    let Some(ev) = &ev.ev else {
        return InputOutcome::Rejected;
    };
    let r = match ev {
        pb::input_event::Ev::MouseMove(m) => injector.mouse_move(m.x, m.y),
        pb::input_event::Ev::MouseButton(b) => {
            let btn = match b.button() {
                pb::MouseButton::MouseLeft => MouseButton::Left,
                pb::MouseButton::MouseRight => MouseButton::Right,
                pb::MouseButton::MouseMiddle => MouseButton::Middle,
                pb::MouseButton::MouseX1 => MouseButton::X1,
                pb::MouseButton::MouseX2 => MouseButton::X2,
                _ => return InputOutcome::Rejected,
            };
            if b.down {
                held.buttons.insert(btn);
            }
            let result = injector.mouse_button(btn, b.down);
            // 松开失败时仍按“可能按住”跟踪，会话清理才能重试释放。
            if !b.down && result.is_ok() {
                held.buttons.remove(&btn);
            }
            result
        }
        pb::input_event::Ev::Wheel(w) => injector.wheel(w.dx, w.dy),
        pb::input_event::Ev::Key(k) => {
            let sc = (k.scancode as u16, k.extended);
            if k.down {
                held.keys.insert(sc);
            }
            let result = injector.key(sc.0, sc.1, k.down);
            if !k.down && result.is_ok() {
                held.keys.remove(&sc);
            }
            result
        }
    };
    match r {
        Ok(()) => InputOutcome::Applied,
        Err(error) => InputOutcome::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod capture_session_tests {
    use super::*;

    #[test]
    fn cancellation_before_subscription_registration_cancels_the_new_subscription() {
        let session = SessionCapture::default();
        session.cancel();
        let capture = SharedCapture::synthetic(16, 16);
        let mut subscription = capture.subscribe(30).unwrap();
        assert!(!session.attach(subscription.cancel_handle()));
        assert!(subscription.recv().is_none());
    }

    #[test]
    fn concurrent_registration_and_session_cancellation_cannot_lose_the_stop_signal() {
        for _ in 0..32 {
            let capture = SharedCapture::synthetic(16, 16);
            let mut subscription = capture.subscribe(30).unwrap();
            let session = SessionCapture::default();
            let other = session.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let start = barrier.clone();
            let worker = std::thread::spawn(move || {
                start.wait();
                other.cancel();
            });
            barrier.wait();
            session.attach(subscription.cancel_handle());
            worker.join().unwrap();
            assert!(session.is_cancelled());
            assert!(subscription.recv().is_none());
        }
    }

    #[test]
    fn resource_drop_wakes_a_pipeline_blocked_in_capture_receive() {
        let capture = SharedCapture::synthetic(16, 16);
        let mut subscription = capture.subscribe(1).unwrap();
        subscription.recv().unwrap();
        let session = SessionCapture::default();
        session.attach(subscription.cancel_handle());
        let resources = SessionResources {
            capture_session: session,
            pipeline_cancel: CancellationToken::new(),
            tasks: Vec::new(),
        };
        let (done, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            done.send(subscription.recv().is_none()).unwrap();
        });
        drop(resources);
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod configure_tests {
    use super::*;
    use vnt_codec::{EncodeError, EncodeParams, EncodedPacket};

    struct Encoder {
        outcomes: std::collections::VecDeque<bool>,
        configured_width: u32,
        attempts: Vec<u32>,
    }

    impl VideoEncoder for Encoder {
        fn codec(&self) -> vnt_proto::Codec {
            vnt_proto::Codec::H264
        }
        fn is_hardware(&self) -> bool {
            false
        }
        fn name(&self) -> &str {
            "configuration probe"
        }
        fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
            self.attempts.push(p.width);
            if self.outcomes.pop_front().unwrap() {
                self.configured_width = p.width;
                Ok(())
            } else {
                Err(EncodeError::Reconfigure("display mode switching".into()))
            }
        }
        fn encode(
            &mut self,
            _: &[u8],
            _: u64,
            _: &mut Vec<EncodedPacket>,
        ) -> Result<(), EncodeError> {
            panic!("configuration attempts must not encode a mismatched frame")
        }
        fn force_keyframe(&mut self) -> Result<(), EncodeError> {
            Ok(())
        }
    }

    #[test]
    fn transient_failure_can_recover_on_the_third_frame() {
        let mut encoder = Encoder {
            outcomes: [false, false, true, false].into(),
            configured_width: 640,
            attempts: Vec::new(),
        };
        let p = EncodeParams::new(1280, 720, 30, 4000);
        let mut failures = 0;
        for _ in 0..2 {
            assert!(!configure_for_frame(&mut encoder, &p, &mut failures).unwrap());
            assert_eq!(encoder.configured_width, 640);
        }
        assert!(configure_for_frame(&mut encoder, &p, &mut failures).unwrap());
        assert_eq!(encoder.configured_width, 1280);
        assert_eq!(failures, 0);
        // A later display switch gets a fresh retry budget and its own parameters.
        let next = EncodeParams::new(1920, 1080, 30, 4000);
        assert!(!configure_for_frame(&mut encoder, &next, &mut failures).unwrap());
        assert_eq!(encoder.attempts, [1280, 1280, 1280, 1920]);
    }

    #[test]
    fn persistent_failure_is_fatal_after_two_retries_even_if_sizes_keep_changing() {
        let mut encoder = Encoder {
            outcomes: [false, false, false].into(),
            configured_width: 640,
            attempts: Vec::new(),
        };
        let mut failures = 0;
        for width in [1280, 1920] {
            let p = EncodeParams::new(width, 720, 30, 4000);
            assert!(!configure_for_frame(&mut encoder, &p, &mut failures).unwrap());
        }
        let error = configure_for_frame(
            &mut encoder,
            &EncodeParams::new(2560, 1440, 30, 4000),
            &mut failures,
        )
        .unwrap_err();
        assert!(error.to_string().contains("display mode switching"));
        assert_eq!(encoder.attempts.len(), 3);
        assert_eq!(encoder.configured_width, 640);
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use std::sync::Mutex;
    use vnt_input::{InputError, InputInjector, InputResult, MouseButton};

    #[derive(Default)]
    struct Injector {
        fail_release: std::sync::atomic::AtomicBool,
        releases: Mutex<Vec<String>>,
    }

    impl Injector {
        fn release(&self, name: String, down: bool) -> InputResult<()> {
            if down {
                return Ok(());
            }
            self.releases.lock().unwrap().push(name);
            if self.fail_release.load(Ordering::Relaxed) {
                Err(InputError::Os("simulated transient failure".into()))
            } else {
                Ok(())
            }
        }
    }

    impl InputInjector for Injector {
        fn mouse_move(&self, _: f32, _: f32) -> InputResult<()> {
            Ok(())
        }
        fn wheel(&self, _: f32, _: f32) -> InputResult<()> {
            Ok(())
        }
        fn mouse_button(&self, button: MouseButton, down: bool) -> InputResult<()> {
            self.release(format!("{button:?}"), down)
        }
        fn key(&self, scancode: u16, extended: bool, down: bool) -> InputResult<()> {
            self.release(format!("{scancode}:{extended}"), down)
        }
    }

    #[test]
    fn disconnect_releases_only_the_departing_controllers_keys_and_buttons() {
        let injector = Arc::new(Injector::default());
        let owners = Arc::new(Mutex::new(InputOwners::default()));
        let first = SessionInputInjector {
            desktop: None,
            id: 1,
            inner: injector.clone(),
            owners: owners.clone(),
        };
        let second = SessionInputInjector {
            desktop: None,
            id: 2,
            inner: injector.clone(),
            owners: owners.clone(),
        };
        for input in [&first, &second] {
            input.key(30, false, true).unwrap();
            input.mouse_button(MouseButton::Left, true).unwrap();
        }
        // An unrelated peer's keyup cannot release either owner's key.
        let unrelated = SessionInputInjector {
            desktop: None,
            id: 3,
            inner: injector.clone(),
            owners: owners.clone(),
        };
        unrelated.key(30, false, false).unwrap();
        drop(first);
        assert!(injector.releases.lock().unwrap().is_empty());
        drop(second);
        assert_eq!(injector.releases.lock().unwrap().len(), 2);
        assert!(owners.lock().unwrap().keys.is_empty());
        assert!(owners.lock().unwrap().buttons.is_empty());
    }

    #[test]
    fn session_drop_retries_failed_release_without_leaving_a_dead_input_owner() {
        let injector = Arc::new(Injector::default());
        let owners = Arc::new(Mutex::new(InputOwners::default()));
        let input = SessionInputInjector {
            desktop: None,
            id: 1,
            inner: injector.clone(),
            owners: owners.clone(),
        };
        input.key(30, false, true).unwrap();
        injector.fail_release.store(true, Ordering::Relaxed);
        assert!(input.key(30, false, false).is_err());
        assert_eq!(owners.lock().unwrap().keys.len(), 1);
        injector.fail_release.store(false, Ordering::Relaxed);
        drop(input);
        assert!(owners.lock().unwrap().keys.is_empty());
        assert_eq!(injector.releases.lock().unwrap().len(), 2);
    }

    #[test]
    fn failed_releases_remain_tracked_for_session_cleanup() {
        let injector = Injector::default();
        let mut held = HeldInput::default();
        let events = [
            pb::input_event::Ev::Key(pb::KeyEvent {
                scancode: 30,
                extended: true,
                down: true,
            }),
            pb::input_event::Ev::MouseButton(pb::MouseButtonEvent {
                button: pb::MouseButton::MouseLeft.into(),
                down: true,
            }),
        ];
        for ev in &events {
            assert!(matches!(
                apply_input(&injector, &pb::InputEvent { ev: Some(*ev) }, &mut held),
                InputOutcome::Applied
            ));
        }
        injector.fail_release.store(true, Ordering::Relaxed);
        for mut ev in events {
            match &mut ev {
                pb::input_event::Ev::Key(k) => k.down = false,
                pb::input_event::Ev::MouseButton(b) => b.down = false,
                _ => unreachable!(),
            }
            assert!(matches!(
                apply_input(&injector, &pb::InputEvent { ev: Some(ev) }, &mut held),
                InputOutcome::Failed(_)
            ));
        }
        assert_eq!(held.keys.len(), 1);
        assert_eq!(held.buttons.len(), 1);
        injector.fail_release.store(false, Ordering::Relaxed);
        held.release_all(&injector);
        let releases = injector.releases.lock().unwrap();
        assert_eq!(releases.iter().filter(|s| *s == "30:true").count(), 2);
        assert_eq!(releases.iter().filter(|s| *s == "Left").count(), 2);
    }

    #[test]
    fn successful_releases_do_not_repeat_at_session_cleanup() {
        let injector = Injector::default();
        let mut held = HeldInput::default();
        for down in [true, false] {
            for ev in [
                pb::input_event::Ev::Key(pb::KeyEvent {
                    scancode: 30,
                    extended: false,
                    down,
                }),
                pb::input_event::Ev::MouseButton(pb::MouseButtonEvent {
                    button: pb::MouseButton::MouseX1.into(),
                    down,
                }),
            ] {
                apply_input(&injector, &pb::InputEvent { ev: Some(ev) }, &mut held);
            }
        }
        assert!(held.keys.is_empty());
        assert!(held.buttons.is_empty());
        held.release_all(&injector);
        assert_eq!(injector.releases.lock().unwrap().len(), 2);
    }
}

/// Instantiated only by a service-created process in the interactive session.
pub struct AgentHost {
    targets: std::sync::Mutex<HashMap<vnt_capture::ScreenRegion, std::sync::Weak<AgentHost>>>,
    cfg: ServerConfig,
    capture: SharedCapture,
    audio: vnt_audio::SharedAudio,
    owners: Arc<std::sync::Mutex<InputOwners>>,
    slots: SessionRegistry,
    caps: Vec<vnt_proto::negotiate::EncoderCap>,
    desktop: Option<Arc<vnt_capture::DesktopCapture>>,
}
impl AgentHost {
    pub fn new(cfg: ServerConfig) -> Self {
        let caps = vnt_codec::probe_caps()
            .into_iter()
            .filter(|c| cfg.hardware_encode || !c.hardware)
            .collect();
        Self {
            targets: Default::default(),
            desktop: None,
            capture: if cfg.synthetic {
                SharedCapture::synthetic(1280, 720)
            } else {
                SharedCapture::monitor(cfg.monitor_index)
            },
            audio: vnt_audio::SharedAudio::new(cfg.synthetic),
            owners: Default::default(),
            slots: Default::default(),
            caps,
            cfg,
        }
    }
    pub fn with_desktop(cfg: ServerConfig, desktop: Arc<vnt_capture::DesktopCapture>) -> Self {
        let mut host = Self::new(cfg);
        host.capture = if host.cfg.synthetic {
            SharedCapture::switching_synthetic(desktop.clone(), 1280, 720)
        } else {
            SharedCapture::switching(desktop.clone())
        };
        host.desktop = Some(desktop);
        host
    }
    pub fn for_region(&self, region: vnt_capture::ScreenRegion) -> Arc<Self> {
        let mut targets = self.targets.lock().unwrap();
        targets.retain(|_, host| host.strong_count() > 0);
        if let Some(host) = targets.get(&region).and_then(|h| h.upgrade()) {
            return host;
        }
        let mut cfg = self.cfg.clone();
        cfg.region = Some(region.clone());
        let desktop = self
            .desktop
            .as_ref()
            .map(|d| vnt_capture::DesktopCapture::region_view(d.clone(), region.clone()));
        let host = Arc::new(Self {
            targets: Default::default(),
            cfg,
            desktop,
            capture: self.capture.clone(),
            audio: self.audio.clone(),
            owners: Default::default(),
            slots: Default::default(),
            caps: self.caps.clone(),
        });
        targets.insert(region, Arc::downgrade(&host));
        host
    }
    pub async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
        &self,
        stream: S,
    ) -> Result<(), String> {
        self.serve_with_cancel(stream, CancellationToken::new())
            .await
    }
    pub async fn serve_with_cancel<
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    >(
        &self,
        stream: S,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let watcher = tokio::spawn(async move {
            cancel.cancelled().await;
            let _ = stop.send(true);
        });
        let result = handle_conn(
            stream,
            "127.0.0.1:0".parse().unwrap(),
            ServerContext {
                worker: true,
                snapshot: None,
                service_desktop: self.desktop.clone(),
                portable: None,
                capture: self.capture.clone(),
                audio: self.audio.clone(),
                input_owners: self.owners.clone(),
                cfg: self.cfg.clone(),
                caps: self.caps.clone(),
                session_slot: self.slots.clone(),
                udp: None,
                sink: Arc::new(crate::NoopEventSink),
                stop_rx,
                clipboard: self.cfg.clipboard.clone(),
            },
        )
        .await;
        watcher.abort();
        result
    }
}
