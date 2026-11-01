//! 控制端会话：连接被控端 → 协商 → TCP/UDP 收流 → 回环 WS 桥 → webview。
//!
//! 输入事件：前端 invoke `client_send_input` → TCP 上行。

pub mod address;
pub mod bridge;
pub mod clipboard;
mod feedback;
pub mod platform;
pub mod screens;
pub mod visibility;
use platform::Outputs;
pub use vnt_proto::{EventSink, NoopEventSink};

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use tokio::sync::mpsc;
use vnt_proto::control::ControlMessage;
use vnt_proto::pb;

use crate::bridge::{BridgeHandle, BridgeMsg};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    #[serde(default)]
    pub server_config_id: Option<String>,
    #[serde(default)]
    pub server_node_id: Option<String>,
    #[serde(default)]
    pub server_network_code: Option<String>,
    pub color_depth: u32,
    pub show_wallpaper: bool,
    pub drag_full_windows: bool,
    pub fec_enabled: bool,
    pub resolution_mode: String,
    pub target_width: u32,
    pub target_height: u32,
    pub transport: vnt_proto::Transport,
    pub host: String,
    pub port: u16,
    pub password: String,
    /// 前端 WebCodecs 探测出的可解码列表（["h264","hevc","av1"] 之类）。
    pub codecs: Vec<String>,
    /// 前端探测且启用的硬件解码能力；无可用能力时传空列表。
    pub hardware_codecs: Vec<String>,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub clipboard_enabled: bool,
    pub audio_enabled: bool,
    /// 本机播放端点 ID，仅用于本地播放，不发送给被控端。
    pub audio_device_id: Option<String>,
}

impl ConnectParams {
    pub fn validate(&self) -> Result<(), String> {
        if !vnt_proto::color_depth::valid(self.color_depth) {
            return Err("颜色深度须为 15、16、24 或 32 位".into());
        }
        if !matches!(self.resolution_mode.as_str(), "none" | "scale" | "change")
            || self.target_width > 16384
            || self.target_height > 16384
        {
            return Err("分辨率参数无效".into());
        }
        if self
            .audio_device_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 2048 || id.contains('\0'))
        {
            return Err("音频播放设备标识无效".into());
        }
        if self
            .server_config_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128)
        {
            return Err("服务端配置标识无效".into());
        }
        if self.transport == vnt_proto::Transport::Vnt
            && (self
                .server_node_id
                .as_ref()
                .is_none_or(|id| id.is_empty() || id.len() > 64)
                || self
                    .server_network_code
                    .as_ref()
                    .is_some_and(|code| code.is_empty() || code.len() > 32))
        {
            return Err("服务端节点身份无效".into());
        }
        if self.port == 0
            || (self.transport != vnt_proto::Transport::Vnt
                && !address::valid_host(self.host.trim()))
            || self.host.trim().is_empty()
        {
            return Err(address::ADDRESS_ERROR.into());
        }
        if self.password.len() > 1024 {
            return Err("密码过长".into());
        }
        if !(1..=120).contains(&self.fps) || !(100..=200_000).contains(&self.bitrate_kbps) {
            return Err("帧率须为 1–120 fps，码率须为 100–200000 kbps".into());
        }
        if self.codecs.is_empty()
            || self
                .codecs
                .iter()
                .any(|codec| !matches!(codec.as_str(), "h264" | "hevc" | "av1"))
        {
            return Err("请选择支持的视频编码协议".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSessionInfo {
    pub capabilities: u32,
    pub fec_enabled: bool,
    pub fec_reason: String,
    pub screens: Vec<serde_json::Value>,
    pub resolution_reason: String,
    pub resolution_mode: String,
    pub target_width: u32,
    pub target_height: u32,
    pub transport: vnt_proto::Transport,
    pub bitrate_kbps: u32,
    pub ws_port: u16,
    pub token: String,
    pub codec: String,
    pub hardware: bool,
    pub encoder_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub session_id: u32,
    pub clipboard_connection_id: String,
    pub clipboard_enabled: bool,
    pub audio_enabled: bool,
    pub audio_reason: String,
    /// 被控端「仅共享屏幕」：本会话只能观看（服务端已忽略输入与剪贴板）。
    pub view_only: bool,
    pub host_edition: &'static str,
}

struct SessionEnd {
    closed: AtomicBool,
    cancel: CancellationToken,
    sink: Arc<dyn EventSink>,
    session_id: u32,
    bridge_shutdown: tokio::sync::watch::Sender<bool>,
}

impl SessionEnd {
    fn finish(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            tracing::info!(session_id = self.session_id, reason, "远程控制会话结束");
            self.cancel.cancel();
            let _ = self.bridge_shutdown.send(true);
            self.sink.emit_json(
                "client://closed",
                serde_json::json!({
                    "reason": reason, "sessionId": self.session_id,
                })
                .to_string(),
            );
        }
    }
}

pub struct ClientHandle {
    pub visibility: Arc<std::sync::Mutex<crate::visibility::Visibility>>,
    pub screens: screens::State,
    pub screen_input_enabled: Arc<AtomicBool>,
    end: Arc<SessionEnd>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pub bridge: BridgeHandle,
    disconnect_writer:
        Option<vnt_net::tcp::TcpWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>>,
    pub control_tx: mpsc::Sender<ControlMessage>,
    control_available: Arc<AtomicBool>,
    sas_allowed: bool,
    paste_allowed: bool,
    paste_pending: Arc<std::sync::Mutex<std::collections::HashMap<u32,tokio::sync::oneshot::Sender<pb::ClipboardPasteResult>>>>,
    /// 剪贴板会话守卫：drop 即注销（取消进行中的收发传输）。
    clipboard: Option<Box<dyn crate::clipboard::ClipboardRegistration>>,
}

impl ClientHandle {
    pub fn is_closed(&self) -> bool {
        self.end.closed.load(Ordering::Acquire)
    }

    pub fn session_id(&self) -> u32 {
        self.end.session_id
    }

    /// One-shot, bounded request. Completion is emitted as client://paste; it is never retried.
    pub fn paste_text(&self,text:String)->Result<u32,String> {
        if self.is_closed() || !self.paste_allowed || !self.control_available()
            || !self.screen_input_enabled.load(Ordering::Acquire) {return Err("当前会话不能粘贴".into());}
        if !vnt_proto::validation::valid_clipboard_text(&text) {return Err("文本超过协议上限".into());}
        static NEXT:AtomicU32=AtomicU32::new(1);
        let request_id=NEXT.try_update(Ordering::Relaxed,Ordering::Relaxed,|id|id.checked_add(1)).map_err(|_|"粘贴请求编号耗尽")?;
        let message=screens::input(&self.screens,ControlMessage::ClipboardPaste(pb::ClipboardPaste{request_id,text}),None).ok_or("当前屏幕不可用")?;
        let (tx,rx)=tokio::sync::oneshot::channel();
        { let mut pending=self.paste_pending.lock().unwrap();
          if pending.len()>=8 {return Err("粘贴请求过多，请稍后再试".into());} pending.insert(request_id,tx); }
        if self.control_tx.try_send(message).is_err() {self.paste_pending.lock().unwrap().remove(&request_id);return Err("粘贴通道拥塞".into());}
        let pending=self.paste_pending.clone();let end=self.end.clone();
        tokio::spawn(async move {
            let result=tokio::select!{biased; _=end.cancel.cancelled()=>None,
                result=tokio::time::timeout(Duration::from_secs(5),rx)=>Some(match result {
                    Ok(Ok(result))=>result,
                    _=>pb::ClipboardPasteResult{request_id,success:false,reason:"远端粘贴未确认，请检查远端；请求不会自动重试".into()},
                })};
            pending.lock().unwrap().remove(&request_id);
            if let Some(result)=result {end.sink.emit_json("client://paste",serde_json::json!({"requestId":request_id,"success":result.success,"reason":result.reason,"sessionId":end.session_id}).to_string());}
        });
        Ok(request_id)
    }
    pub fn send_sas(&self) -> Result<(), String> {
        if !self.sas_allowed {
            return Err("当前被控不支持 Ctrl+Alt+Del（需要安装版且允许控制）".into());
        }
        self.control_tx
            .try_send(ControlMessage::SendSas)
            .map_err(|e| e.to_string())
    }

    pub fn reset_screen_input(&self) -> Result<(), String> {
        if let Some(message) = screens::input(
            &self.screens,
            ControlMessage::ResetInput(pb::ResetInput {}),
            None,
        ) {
            self.control_tx
                .try_send(message)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn control_flag(&self) -> Arc<AtomicBool> {
        self.control_available.clone()
    }

    pub fn control_available(&self) -> bool {
        self.control_available.load(Ordering::Acquire)
    }

    pub fn end(&self, reason: &str) {
        self.end.finish(reason);
    }

    pub fn failure_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let end = self.end.clone();
        Arc::new(move || end.finish("键盘输入通道拥塞或已断开"))
    }

    pub fn disconnect(self) {
        // Drop performs a bounded graceful close before cancelling the reader.
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.bridge.shutdown();
        let tasks = std::mem::take(&mut self.tasks);
        let clipboard = self.clipboard.take();
        let end = self.end.clone();
        if !end.closed.load(Ordering::Acquire) && !end.cancel.is_cancelled() {
            if let Some(mut writer) = self.disconnect_writer.take() {
                let cleanup = async move {
                    // Keep the receiver and clipboard registration alive until the
                    // terminal packet is sent. Dropping either first cancels the
                    // managed session before SessionEnd can reach the host.
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        writer
                            .send_owned(ControlMessage::SessionEnd(pb::SessionEnd {
                                reason: "客户端主动断开".into(),
                            }))
                            .await?;
                        end.cancel.cancelled().await;
                        Ok::<_, std::io::Error>(())
                    })
                    .await;
                    drop(clipboard);
                    end.finish("连接已结束");
                    for task in tasks {
                        task.abort();
                    }
                };
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(cleanup);
                } else {
                    std::thread::spawn(move || {
                        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            rt.block_on(cleanup);
                        }
                    });
                }
                return;
            }
        }
        drop(clipboard);
        end.finish("连接已结束");
        for task in tasks {
            task.abort();
        }
    }
}

/// 前端输入事件（serde 反序列化目标）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputArg {
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    Wheel {
        dx: f32,
        dy: f32,
    },
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenInputTarget {
    pub stream_id: u32,
    pub generation: u32,
    pub revision: u64,
}

pub async fn send_screen_input_events(
    tx: &mpsc::Sender<ControlMessage>,
    events: Vec<InputArg>,
    state: &screens::State,
    target: Option<ScreenInputTarget>,
) -> Result<(), String> {
    if events.len() > 64 {
        return Err("输入批次超过 64 个事件".into());
    }
    for event in events {
        let event = event.into_proto();
        if !vnt_proto::validation::valid_input(&event) {
            return Err("无效的输入事件".into());
        }
        if let Some(message) = screens::input(
            state,
            ControlMessage::InputEvent(event),
            target.map(|t| (t.stream_id, t.generation, t.revision)),
        ) {
            tokio::time::timeout(Duration::from_secs(2), tx.send(message))
                .await
                .map_err(|_| "输入通道超时".to_string())?
                .map_err(|_| "输入通道关闭".to_string())?;
        }
    }
    Ok(())
}

impl InputArg {
    fn into_proto(self) -> pb::InputEvent {
        let ev = match self {
            InputArg::MouseMove { x, y } => pb::input_event::Ev::MouseMove(pb::MouseMove { x, y }),
            InputArg::MouseButton { button, down } => {
                let b = match button {
                    1 => pb::MouseButton::MouseLeft,
                    2 => pb::MouseButton::MouseRight,
                    3 => pb::MouseButton::MouseMiddle,
                    4 => pb::MouseButton::MouseX1,
                    5 => pb::MouseButton::MouseX2,
                    _ => pb::MouseButton::MouseNone,
                };
                pb::input_event::Ev::MouseButton(pb::MouseButtonEvent {
                    button: b.into(),
                    down,
                })
            }
            InputArg::Wheel { dx, dy } => pb::input_event::Ev::Wheel(pb::WheelEvent { dx, dy }),
            InputArg::Key {
                scancode,
                extended,
                down,
            } => pb::input_event::Ev::Key(pb::KeyEvent {
                scancode: scancode as u32,
                extended,
                down,
            }),
        };
        pb::InputEvent { ev: Some(ev) }
    }
}

/// 连接并建立会话，返回句柄与前端需要的会话信息。
///
/// `clipboard` 是进程内剪贴板同步服务：本会话注册后，本机剪贴板变化
/// 会送给对端，对端剪贴板也会写进本机。句柄 drop 即注销。
pub async fn connect_with(
    sink: Arc<dyn EventSink>,
    params: ConnectParams,
    clipboard: Arc<dyn clipboard::ClipboardService>,
    outputs: Outputs,
    prepared: Option<vnt_net::overlay::Prepared>,
) -> Result<(ClientHandle, ClientSessionInfo), String> {
    params.validate()?;
    let transport = params.transport;
    // Resolve only direct connections; prepared overlay sessions already have a peer.
    let (addr, mut channel, media_socket) = if let Some(prepared) = prepared {
        (prepared.peer, prepared.channel, Some(prepared.media))
    } else {
        if transport == vnt_proto::Transport::Vnt {
            return Err("VNT 连接必须通过服务端节点入口".into());
        }
        let addresses = address::resolve(&params.host, params.port).await?;
        let mut connected = None;
        let mut last_error = String::new();
        // DNS can return both IPv4 and IPv6. Try each address and retain the
        // successful peer for the media channel as well as the control channel.
        for addr in addresses {
            let attempt = tokio::time::timeout(Duration::from_secs(8), async {
                if transport == vnt_proto::Transport::Quic {
                    vnt_net::quic::connect(addr, params.password.clone())
                        .await
                        .map(|(channel, media)| (channel, Some(media)))
                } else {
                    vnt_net::session::connect(addr)
                        .await
                        .map(|channel| (channel, None))
                }
            })
            .await;
            match attempt {
                Ok(Ok((channel, media))) => {
                    connected = Some((addr, channel, media));
                    break;
                }
                Ok(Err(error)) => last_error = format!("连接 {addr} 失败: {error}"),
                Err(_) => last_error = format!("连接 {addr} 超时"),
            }
        }
        connected.ok_or(last_error)?
    };
    let traffic = channel.traffic();
    let codecs: Vec<i32> = params
        .codecs
        .iter()
        .map(|c| {
            match c.as_str() {
                "hevc" | "h265" => pb::Codec::Hevc,
                "av1" => pb::Codec::Av1,
                _ => pb::Codec::H264,
            }
            .into()
        })
        .collect();
    channel
        .send(&ControlMessage::Hello(pb::Hello {
            color_depth: params.color_depth,
            desktop_effects: Some(pb::DesktopEffects {
                show_wallpaper: params.show_wallpaper,
                drag_full_windows: params.drag_full_windows,
            }),
            capabilities: clipboard.capabilities(),
            fec_version: if params.fec_enabled && transport != vnt_proto::Transport::Tcp {
                vnt_net::fec::VERSION
            } else {
                0
            },
            resolution_mode: match params.resolution_mode.as_str() {
                "scale" => 1,
                "change" => 2,
                _ => 0,
            },
            target_width: params.target_width,
            target_height: params.target_height,
            audio_enabled: params.audio_enabled,
            transport: transport.to_proto() as i32,
            proto_ver: vnt_proto::PROTO_VER,
            fps: params.fps,
            bitrate_kbps: params.bitrate_kbps,
            codecs,
            hardware_codecs: params
                .hardware_codecs
                .iter()
                .filter(|c| params.codecs.contains(c))
                .filter_map(|c| match c.as_str() {
                    "h264" => Some(pb::Codec::H264 as i32),
                    "hevc" => Some(pb::Codec::Hevc as i32),
                    "av1" => Some(pb::Codec::Av1 as i32),
                    _ => None,
                })
                .collect(),
            client: Some(pb::ClientInfo {
                name: outputs.client_name.clone(),
            }),
        }))
        .await
        .map_err(|e| format!("发送握手失败: {e}"))?;

    vnt_net::auth::respond(&mut channel, &params.password)
        .await
        .map_err(|e| format!("密码认证失败: {e}"))?;

    let welcome = tokio::time::timeout(Duration::from_secs(30), channel.recv())
        .await
        .map_err(|_| "等待 Welcome 超时".to_string())?;
    let welcome = match welcome {
        Ok(Some(ControlMessage::Welcome(w))) => w,
        Ok(Some(ControlMessage::Denied(d))) => return Err(format!("被拒绝: {}", d.reason)),
        Ok(_) => return Err("连接被关闭".into()),
        Err(e) => return Err(format!("握手失败: {e}")),
    };
    if welcome.proto_ver != vnt_proto::PROTO_VER {
        return Err("协议版本不匹配".into());
    }
    if !vnt_proto::color_depth::confirmed(params.color_depth, welcome.color_depth) {
        return Err("被控返回的颜色深度与请求不一致".into());
    }
    let codec = vnt_proto::Codec::from_proto(welcome.codec()).ok_or("未知编码")?;
    validate_welcome(&welcome, &params.codecs, transport)?;
    if welcome.audio_enabled && !params.audio_enabled {
        return Err("服务端启用了未请求的音频".into());
    }
    if !matches!(welcome.resolution_mode, 0..=2) {
        return Err("被控返回了无效的分辨率模式".into());
    }
    let audio_enabled = welcome.audio_enabled;
    let audio_reason = if !params.audio_enabled {
        "主控未启用音频"
    } else if !audio_enabled {
        "被控未共享系统声音"
    } else {
        "正在初始化音频"
    }
    .to_string();
    let (audio_tx, mut audio_rx) = mpsc::channel::<pb::AudioFrame>(6);
    let (audio_state_tx, mut audio_state_rx) = tokio::sync::watch::channel(pb::AudioState {
        reason: audio_reason.clone(),
        ..Default::default()
    });
    let screen_state: screens::State = Arc::new(std::sync::Mutex::new(pb::ScreensState {
        revision: 0,
        active_stream_id: welcome
            .screens
            .iter()
            .find(|s| s.available)
            .map_or(0, |s| s.stream_id),
        screens: welcome.screens.clone(),
    }));
    let screen_input_enabled = Arc::new(AtomicBool::new(true));
    let paste_pending=Arc::new(std::sync::Mutex::new(std::collections::HashMap::<u32,tokio::sync::oneshot::Sender<pb::ClipboardPasteResult>>::new()));
    let bind_token = welcome.bind_token;
    tracing::info!(
        "接入成功: transport={transport:?} codec={codec:?} hw={} {}x{}@{} encoder={} view_only={}",
        welcome.hardware,
        welcome.width,
        welcome.height,
        welcome.fps,
        welcome.encoder_name,
        welcome.view_only
    );

    validate_fec(params.fec_enabled, transport, welcome.fec_version)?;
    // ---- UDP 接收 ----
    let receiver = if transport != vnt_proto::Transport::Tcp {
        let mut receiver = if let Some(media) = media_socket {
            vnt_net::udp::UdpVideoReceiver::from_socket(media, addr)
        } else {
            vnt_net::udp::UdpVideoReceiver::bind(addr).await
        }
        .map_err(|e| format!("初始化媒体通道失败: {e}"))?;
        receiver.set_session(welcome.session_id);
        receiver.set_fec(welcome.fec_version == vnt_net::fec::VERSION);
        receiver.retain_screens(
            &welcome
                .screens
                .iter()
                .map(|s| s.stream_id)
                .collect::<Vec<_>>(),
        );
        receiver.set_traffic(traffic.clone());
        if audio_enabled {
            receiver.set_audio_sender(audio_tx.clone());
        }
        Some(receiver)
    } else {
        None
    };
    let (tcp_video_tx, mut tcp_video_rx) =
        mpsc::channel::<(u32, u32, pb::VideoFrame)>(2 * welcome.screens.len().max(1));

    // ---- 回环桥 ----
    // 流信息缓存：初始值 + StreamReset 更新；桥在每个新 WS 连接时重发，
    // 避免前端连接前到达的分辨率更新被广播通道丢弃
    let info = ClientSessionInfo {
        capabilities: welcome.capabilities & clipboard.capabilities(),
        fec_enabled: transport != vnt_proto::Transport::Tcp
            && welcome.fec_version == vnt_net::fec::VERSION,
        fec_reason: if transport == vnt_proto::Transport::Tcp {
            "适用于混合模式、QUIC 和 VNT"
        } else if !params.fec_enabled {
            "未启用"
        } else {
            "自适应 FEC 已启用"
        }
        .into(),
        screens: welcome.screens.iter().map(screens::info).collect(),
        resolution_mode: match welcome.resolution_mode {
            1 => "scale",
            2 => "change",
            _ => "none",
        }
        .into(),
        resolution_reason: welcome.resolution_reason.clone(),
        target_width: params.target_width,
        target_height: params.target_height,
        transport,
        bitrate_kbps: params.bitrate_kbps,
        ws_port: 0, // 桥启动后回填
        token: String::new(),
        codec: codec.as_str().to_string(),
        hardware: welcome.hardware,
        encoder_name: welcome.encoder_name.clone(),
        width: welcome.width,
        height: welcome.height,
        fps: welcome.fps,
        session_id: welcome.session_id,
        clipboard_connection_id: String::new(),
        clipboard_enabled: params.clipboard_enabled && clipboard.is_enabled() && !welcome.view_only,
        audio_enabled,
        audio_reason: audio_reason.clone(),
        view_only: welcome.view_only,
        host_edition: if welcome.host_edition == pb::HostEdition::Installed as i32 {
            "installed"
        } else {
            "portable"
        },
    };
    let stream_info_json = screens::json(&screen_state.lock().unwrap()).to_string();
    let stream_cache: crate::bridge::StreamInfoCache =
        std::sync::Arc::new(std::sync::Mutex::new(Some(stream_info_json)));
    let bridge = if let Some(media) = outputs.media.clone() {
        bridge::native(stream_cache.clone(), media)
    } else {
        bridge::start_with_capacity(stream_cache.clone(), 2 * welcome.screens.len().max(1) + 8)
            .await
            .map_err(|e| format!("启动视频桥失败: {e}"))?
    };
    tracing::info!(session_id = welcome.session_id, port = bridge.port, peer = %addr,
        "远程会话已绑定本地视频桥");
    let info = ClientSessionInfo {
        ws_port: bridge.port,
        token: bridge.token.clone(),
        ..info
    };

    let control_available = Arc::new(AtomicBool::new(true));
    let visibility = Arc::new(std::sync::Mutex::new(
        crate::visibility::Visibility::default(),
    ));
    let media_generation = Arc::new(AtomicU32::new(0));
    let (reader, writer) = channel.into_split();
    let disconnect_writer = writer.fork();
    let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(128);
    // 剪贴板批量通道：按需读取的数据回复。容量小 → 天然背压；控制消息
    // （输入/心跳/反馈）在 writer 的 select 中优先级更高，不被分片挤后。
    let (clip_tx, clip_rx) = mpsc::channel::<ControlMessage>(crate::clipboard::OUT_QUEUE);
    let (clip_in_tx, clip_in_rx) =
        mpsc::channel::<crate::clipboard::InboundClipboard>(crate::clipboard::INBOUND_QUEUE);
    let end = Arc::new(SessionEnd {
        closed: AtomicBool::new(false),
        cancel: CancellationToken::new(),
        sink: sink.clone(),
        session_id: welcome.session_id,
        bridge_shutdown: bridge.shutdown_sender(),
    });
    let clipboard_registration = clipboard.clone().register(crate::clipboard::SessionSpec {
        capabilities: welcome.capabilities & clipboard.capabilities(),
        session_id: welcome.session_id,
        file_tx: clip_tx,
        control_tx: control_tx.clone(),
        in_rx: clip_in_rx,
        session_cancel: end.cancel.child_token(),
        sink: sink.clone(),
        // 发布会话前应用主控偏好及观看模式限制，避免等待界面加载的同步窗口。
        enabled: info.clipboard_enabled,
    });
    let clipboard_key = clipboard_registration.key();
    let info = ClientSessionInfo {
        clipboard_connection_id: clipboard_key.to_string(),
        ..info
    };
    let mut tasks = Vec::new();
    if transport == vnt_proto::Transport::Hybrid {
        if let Some(receiver) = &receiver {
            tasks.push(tokio::spawn(receiver.binding_keepalive(
                welcome.session_id,
                bind_token.clone(),
                end.cancel.clone(),
            )));
        }
    }
    {
        let end = end.clone();
        // This task owns the native playback guard; cancellation and task abort
        // both stop its thread, even while the closed window still shows an overlay.
        let audio_device_id = params.audio_device_id.clone();
        let audio_factory = outputs.audio.clone();
        tasks.push(tokio::spawn(async move {
            let playback = if audio_enabled { Some(audio_factory.start(audio_device_id)) } else { None };
            let sender = playback.as_ref().and_then(|p| p.as_ref().ok()).map(|p| p.sender());
            if let Some(sender) = &sender { sender.set_state(audio_state_rx.borrow_and_update().clone()); }
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    biased;
                    _ = end.cancel.cancelled() => break,
                    state = audio_state_rx.changed() => {
                        if state.is_err() { break; }
                        let state = audio_state_rx.borrow_and_update().clone();
                        if let Some(sender) = &sender { sender.set_state(state); }
                    }
                    frame = audio_rx.recv() => {
                        let Some(frame) = frame else { break; };
                        if let Some(sender) = &sender { sender.send(frame); }
                    }
                    _ = interval.tick() => {
                        let status = sender.as_ref().map(|sender| sender.status());
                        let reason = match &playback {
                            Some(Err(error)) => format!("音频播放启动失败：{error}"),
                            _ => status.as_ref().map_or_else(|| audio_reason.clone(), |s| s.reason.clone()),
                        };
                        end.sink.emit_json("client://audio", serde_json::json!({
                            "sessionId": end.session_id, "enabled": audio_enabled,
                            "available": status.as_ref().is_some_and(|s| s.available),
                            "reason": reason, "bufferMs": status.as_ref().map_or(0, |s| s.buffer_ms),
                        }).to_string());
                    }
                }
            }
            drop(playback);
        }));
    }
    {
        let end = end.clone();
        tasks.push(tokio::spawn(async move {
            let mut last = traffic.snapshot();
            let mut sampled_at = tokio::time::Instant::now();
            let mut interval = tokio::time::interval_at(
                sampled_at + Duration::from_secs(1),
                Duration::from_secs(1),
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = end.cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let now = tokio::time::Instant::now();
                        let seconds = now.duration_since(sampled_at).as_secs_f64();
                        let current = traffic.snapshot();
                        end.sink.emit_json(
                            "client://traffic",
                            serde_json::json!({
                                "sessionId": end.session_id,
                                "connectionId": clipboard_key.to_string(),
                                "uploadBytesPerSecond": (current.0 - last.0) as f64 / seconds,
                                "downloadBytesPerSecond": (current.1 - last.1) as f64 / seconds,
                            }).to_string(),
                        );
                        last = current;
                        sampled_at = now;
                    }
                }
            }
        }));
    }
    // Echo matching uses a monotonic clock, independent of wall-clock changes.
    let pending_ping = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<(
        u64,
        tokio::time::Instant,
    )>::new()));
    let recent_rtt = Arc::new(std::sync::Mutex::new(None::<(tokio::time::Instant, u32)>));

    let mut clip_rx = clip_rx;
    let separate_files = writer.fork();
    let files_managed = separate_files.is_some();
    if let Some(mut files) = separate_files {
        let end = end.clone();
        let (_, empty) = mpsc::channel(1);
        let mut incoming = std::mem::replace(&mut clip_rx, empty);
        tasks.push(tokio::spawn(async move {
            loop {
                let message = tokio::select! { _=end.cancel.cancelled()=>break, m=incoming.recv()=>match m { Some(m)=>m, None=>break } };
                tokio::select! { _=end.cancel.cancelled()=>break, result=files.send_owned(message)=>if result.is_err() {break;} }
            }
        }));
    }
    // Every task is owned by the handle and observes the same cancellation token.
    {
        let end = end.clone();
        tasks.push(tokio::spawn(async move {
            let mut writer = writer;
            let mut control_rx = control_rx;
            let mut clip_rx = clip_rx;
            let mut input_sent = false;
            loop {
                tokio::select! {
                    biased;
                    _ = end.cancel.cancelled() => break,
                    msg = control_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let is_input = matches!(&msg, ControlMessage::InputEvent(_));
                        let result = tokio::select! {
                            biased;
                            _ = end.cancel.cancelled() => break,
                            result = tokio::time::timeout(Duration::from_secs(2), writer.send_owned(msg)) => result,
                        };
                        if !matches!(result, Ok(Ok(()))) {
                            end.finish("控制通道发送失败或超时");
                            break;
                        }
                        if !input_sent && is_input {
                            input_sent = true;
                            tracing::info!("已通过远程控制通道发送首个键鼠输入事件");
                        }
                    }
                    clip = clip_rx.recv(), if !files_managed => {
                        let Some(msg) = clip else { break };
                        let result = tokio::select! {
                            biased;
                            _ = end.cancel.cancelled() => break,
                            result = tokio::time::timeout(Duration::from_secs(2), writer.send(&msg)) => result,
                        };
                        if !matches!(result, Ok(Ok(()))) {
                            end.finish("剪贴板通道发送失败或超时");
                            break;
                        }
                    }
                }
            }
        }));
    }
    {
        let end = end.clone();
        let heartbeat_tx = control_tx.clone();
        let pending_ping = pending_ping.clone();
        tasks.push(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    biased;
                    _ = end.cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64).unwrap_or(0);
                        {
                            let mut pending = pending_ping.lock().unwrap();
                            if pending.len() == 8 { pending.pop_front(); }
                            pending.push_back((ts, tokio::time::Instant::now()));
                        }
                        if heartbeat_tx.try_send(ControlMessage::Ping(pb::Ping { timestamp_ms: ts })).is_err() {
                            end.finish("控制通道拥塞或已关闭");
                            break;
                        }
                    }
                }
            }
        }));
    }
    {
        let end = end.clone();
        let control_tx = control_tx.clone();
        let bridge_tx = bridge.sender.clone();
        let sid = welcome.session_id;
        let screen_state = screen_state.clone();
        let recent_rtt = recent_rtt.clone();
        let media_generation = media_generation.clone();
        tasks.push(tokio::spawn(async move {
            let mut receiver = receiver;
            if let Some(receiver) = &mut receiver { receiver.set_recovery_sender(control_tx.clone()); }
            let mut fec_telemetry=feedback::FecTelemetry::default();
            let mut feedback = feedback::ReceiveWindow::new(tokio::time::Instant::now(), receiver.as_ref().map_or(0, |r| r.stats.lost_frames));
            let mut windows=std::collections::HashMap::<u32,feedback::ReceiveWindow>::new();
            let mut screen_last=std::collections::HashMap::<u32,tokio::time::Instant>::new();
            let mut screen_generations=std::collections::HashMap::<u32,u32>::new();
            let mut screen_requests=std::collections::HashMap::<u32,tokio::time::Instant>::new();
            let mut received_video = false;
            let mut observed_generation = 0;
            let mut last_udp_diagnostic = tokio::time::Instant::now();
            let mut previous_reassembly = vnt_proto::udp::ReassemblyStats::default();

            loop {
                let now = tokio::time::Instant::now();
                if now.duration_since(last_udp_diagnostic) >= Duration::from_secs(5) {
                    if let Some(receiver) = &receiver {
                        let stats = receiver.stats;
                        tracing::info!(session_id = sid,
                            lost_frames = stats.lost_frames.wrapping_sub(previous_reassembly.lost_frames),
                            timed_out_frames = stats.timed_out_frames.wrapping_sub(previous_reassembly.timed_out_frames),
                            late_fragments = stats.late_frags.wrapping_sub(previous_reassembly.late_frags),
                            "UDP 接收诊断（5 秒窗口）");
                        for (route, recovery) in receiver.recovery_diagnostics() {
                            tracing::info!(session_id = sid, stream_id = route.stream, generation = route.generation,
                                nack_requests = recovery.nack_requests, recovered_frames = recovery.recovered_frames,
                                observed_missing_fragments = recovery.observed_missing_fragments,
                                observed_missing_whole_frames = recovery.observed_missing_whole_frames,
                                timed_out_frames = recovery.timed_out_frames, lost_frames = recovery.lost_frames,
                                skipped_frames = recovery.skipped_frames, idr_requests = recovery.idr_requests,
                                idr_missing_reference = recovery.idr_missing_reference,
                                idr_unknown_dependencies = recovery.idr_unknown_dependencies,
                                "UDP 恢复诊断（累计）");
                        }
                        previous_reassembly = stats;
                    }

                    last_udp_diagnostic = now;
                }
                let rtt = recent_rtt.lock().unwrap().filter(|(at, _)| now.duration_since(*at) <= Duration::from_secs(4)).map(|(_, ms)| ms);
                if let Some(receiver) = &mut receiver { receiver.set_rtt(rtt); }
                if let Some(mut report) = feedback.report(now, receiver.as_ref().map_or(0, |r| r.stats.lost_frames), rtt) {
                    if let Some(r)=receiver.as_mut(){report.fec=r.fec_reports(None);}
                    if let Some(value)=fec_telemetry.update(sid,&report.fec){end.sink.emit_json("client://fec",value.to_string());}
                    // Telemetry cannot consume the reliable input/heartbeat reserve.
                    if control_tx.capacity() > 16 {
                        let _ = control_tx.try_send(ControlMessage::LinkFeedback(report));
                    }
                }
                {
                    let state=screen_state.lock().unwrap().clone();
                    let ids:Vec<_>=state.screens.iter().map(|s|s.stream_id).collect();
                    if let Some(receiver)=receiver.as_mut(){receiver.retain_screens(&ids);}
                    windows.retain(|id,_|ids.contains(id));screen_last.retain(|id,_|ids.contains(id));
                    for screen in &state.screens {
                        let lost=receiver.as_ref().map_or(0,|r|r.screen_lost(screen.stream_id));
                        if screen_generations.insert(screen.stream_id,screen.generation)!=Some(screen.generation){if let Some(window)=windows.get_mut(&screen.stream_id){window.reset_generation(now,lost);}screen_last.remove(&screen.stream_id);}
                        let window=windows.entry(screen.stream_id).or_insert_with(||feedback::ReceiveWindow::new(now,lost));
                        if let Some(mut report)=window.report(now,lost,rtt){if let Some(r)=receiver.as_mut(){report.fec=r.fec_reports(Some(screen.stream_id));}
                                if let Some(value)=fec_telemetry.update(sid,&report.fec){end.sink.emit_json("client://fec",value.to_string());}let _=control_tx.try_send(vnt_proto::screen::wrap(screen.stream_id,screen.generation,ControlMessage::LinkFeedback(report)));}
                        if screen.available && !screen_last.contains_key(&screen.stream_id) && screen_requests.get(&screen.stream_id).is_none_or(|last|now.duration_since(*last)>=Duration::from_secs(1)) {
                            screen_requests.insert(screen.stream_id,now);
                            let _=control_tx.try_send(vnt_proto::screen::wrap(screen.stream_id,screen.generation,ControlMessage::RequestKeyframe));
                        }
                    }
                }
                let current_generation = media_generation.load(Ordering::Acquire);
                if current_generation != observed_generation {
                    observed_generation = current_generation;
                    if transport == vnt_proto::Transport::Quic {
                        if let Some(receiver) = receiver.as_mut() { receiver.set_session(sid); }
                    }
                    while tcp_video_rx.try_recv().is_ok() {}
                    received_video = false;
                }
                if !received_video {
                    if let Some(receiver) = receiver.as_mut() {
                        let _ = receiver.send_bind(sid, &bind_token).await;
                    }
                }
                let frame = tokio::select! {
                    biased;
                    _ = end.cancel.cancelled() => break,
                    result = tokio::time::timeout(Duration::from_millis(200), async {
                        if let Some(receiver) = receiver.as_mut() {
                            receiver.recv_screen_frame().await.map(|f| f.map(|(id,generation,f)| (id,generation,pb::VideoFrame { dependencies: None, keyframe: f.keyframe, pts_ms: f.pts_ms, data: f.data })))
                        } else {
                            Ok(tcp_video_rx.recv().await)
                        }
                    }) => result,
                };
                match frame {
                    Ok(Ok(Some((stream_id,generation,f)))) => {
                        if stream_id==0{continue;}
                        {
                            let valid=screen_state.lock().unwrap().screens.iter().any(|s|s.stream_id==stream_id && s.generation==generation && s.available);
                            if !valid{continue;}
                            let now=tokio::time::Instant::now();screen_last.insert(stream_id,now);
                            if let Some(window)=windows.get_mut(&stream_id){window.received(now,f.data.len());}
                            received_video = true;
                            let _=bridge_tx.send(BridgeMsg::Video{stream_id,generation,keyframe:f.keyframe,pts_ms:f.pts_ms,data:f.data.into()});continue;
                        }
                    }
                    Ok(Err(_)) => {
                        end.finish("视频通道接收失败");
                        break;
                    }
                    _ => {}
                }
            }
        }));
    }
    {
        let end = end.clone();
        let control_tx = control_tx.clone();
        let bridge_tx = bridge.sender.clone();
        let info = info.clone();
        let screen_state = screen_state.clone();
        let screen_input_enabled = screen_input_enabled.clone();
        let visibility = visibility.clone();
        let control_available = control_available.clone();
        let media_generation = media_generation.clone();
        let paste_pending=paste_pending.clone();
        tasks.push(tokio::spawn(async move {
            let mut reader = reader;
            let mut rtt_ms = 0;
            let mut clipboard_before_switch = false;
            let mut source_audio_generation = 0;
            let mut playback_generation = 0u32;
            let mut reset_audio_generation = true;
            let mut previous_desktop_ready = true;
            let reason = loop {
                let result = tokio::select! {
                    biased;
                    _ = end.cancel.cancelled() => return,
                    result = tokio::time::timeout(Duration::from_secs(15), reader.recv()) => result,
                };
                let msg = match result {
                    Ok(Ok(Some(msg))) => msg,
                    Ok(Ok(None)) => break "连接已断开".to_string(),
                    Ok(Err(e)) => break format!("连接错误: {e}"),
                    Err(_) => break "连接无响应（心跳超时）".to_string(),
                };
                match msg {
                    ControlMessage::ScreensState(state) => {
                        let mut current=screen_state.lock().unwrap();
                        if state.revision<current.revision {continue;}
                        if state.screens.is_empty() || state.screens.len()>128 || !state.screens.iter().any(|s|s.stream_id==state.active_stream_id) {break "被控返回了无效的屏幕列表".into();}
                        *current=state;
                        screen_input_enabled.store(true,Ordering::Release);
                        let json=screens::cache_state(&stream_cache,&current);
                        let _=bridge_tx.send(BridgeMsg::Text(json));continue;
                    }
                    ControlMessage::ScreenPacket(packet) => {
                        let Some(message)=vnt_proto::screen::unwrap(&packet) else {continue;};
                        let screen=screen_state.lock().unwrap().screens.iter().find(|s|s.stream_id==packet.stream_id).cloned();
                        let Some(screen)=screen else {continue;};
                        if packet.generation<screen.generation{continue;}
                        match message {
                            ControlMessage::VideoFrame(frame)=>{
                                if transport!=vnt_proto::Transport::Tcp {break "混合模式收到 TCP 视频".into();}
                                if tcp_video_tx.send((packet.stream_id,packet.generation,frame)).await.is_err(){break "视频接收任务已结束".into();}
                            }
                            ControlMessage::StreamReset(reset)=>{
                                if !vnt_proto::validation::valid_video_size(reset.width,reset.height)||!(1..=120).contains(&reset.fps){break "无效的屏幕流参数".into();}
                                let json=serde_json::json!({"type":"streamInfo","streamId":packet.stream_id,"generation":packet.generation,"codec":vnt_proto::Codec::from_proto(screen.codec()).map(|c|c.as_str()).unwrap_or("h264"),"hardware":screen.hardware,"width":reset.width,"height":reset.height,"fps":reset.fps,"decoderConfig":reset.decoder_config});
                                screens::cache_stream(&stream_cache,packet.stream_id,json.clone());let _=bridge_tx.send(BridgeMsg::Text(json.to_string()));
                            }
                            ControlMessage::DesktopState(state)=>{
                                let json=serde_json::json!({"type":"desktopState","streamId":packet.stream_id,"generation":packet.generation,"ready":state.ready,"reason":state.reason});
                                screens::cache_stream(&stream_cache,packet.stream_id,json.clone());let _=bridge_tx.send(BridgeMsg::Text(json.to_string()));
                            }
                            ControlMessage::StatsReport(stats)=>{
                                let json=serde_json::json!({"type":"screenStats","streamId":packet.stream_id,"serverFps":stats.fps,"serverBitrateKbps":stats.bitrate_kbps,"captureMs":stats.capture_ms,"encodeMs":stats.encode_ms,"targetFps":stats.target_fps,"targetBitrateKbps":stats.target_bitrate_kbps,"appliedFps":stats.applied_fps,"appliedBitrateKbps":stats.applied_bitrate_kbps,"lossPermille":stats.loss_permille});
                                let _=bridge_tx.send(BridgeMsg::Text(json.to_string()));
                                if screen_state.lock().unwrap().active_stream_id == packet.stream_id {
                                    let mut stats = json;
                                    stats["sessionId"] = info.session_id.into(); stats["rttMs"] = rtt_ms.into();
                                    sink.emit_json("client://stats", stats.to_string());
                                }
                            }
                            _=>{}
                        }
                        continue;
                    }
                    ControlMessage::TransportState(state) => {
                        if state.kind == 0 {
                            visibility.lock().unwrap().invalidate();
                            if state.synchronizing {
                                let _ = control_tx.send(ControlMessage::SessionPacket(pb::SessionPacket {
                                    op: pb::SessionOperation::SessionSyncAck as i32, kind: 0,
                                    session_id: info.session_id, generation: state.generation, state_revision: state.state_revision, ..Default::default()
                                })).await;
                            }
                            if state.available && transport != vnt_proto::Transport::Tcp { media_generation.fetch_add(1, Ordering::AcqRel); }
                            control_available.store(state.available, Ordering::Release);
                            clipboard.control_changed(clipboard_key, state.available);
                        }
                        if state.kind == 1 { clipboard.transport_changed(clipboard_key, state.available); }
                        if state.kind == 2 {
                            media_generation.fetch_add(1, Ordering::AcqRel);
                            if !state.available {
                                reset_audio_generation = true;
                                audio_state_tx.send_replace(pb::AudioState { reason: "音视频通道恢复中".into(), ..Default::default() });
                            }
                        }
                        {
                            let mut cache = stream_cache.lock().unwrap();
                            if let Some(mut value) = cache.as_ref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) {
                                let key = ["control", "files", "media"].get(state.kind as usize).copied();
                                if let Some(key) = key { value["transportState"][key] = state.available.into(); }
                                *cache = Some(value.to_string());
                            }
                        }
                        let _ = bridge_tx.send(BridgeMsg::Text(serde_json::json!({
                            "type":"transportState", "kind":state.kind, "available":state.available,
                            "generation":state.generation
                        }).to_string()));
                        continue;
                    }
                    ControlMessage::VideoFrame(_) | ControlMessage::StreamReset(_) | ControlMessage::StatsReport(_) => {
                        break "收到未指定屏幕的视频消息".into();
                    }
                    ControlMessage::AudioState(mut state) if audio_enabled => {
                        if transport == vnt_proto::Transport::Tcp {
                            if reset_audio_generation || source_audio_generation != state.generation {
                                source_audio_generation = state.generation;
                                playback_generation = playback_generation.wrapping_add(1).max(1);
                                reset_audio_generation = false;
                            }
                            state.generation = playback_generation;
                        }
                        audio_state_tx.send_replace(state);
                        continue;
                    }
                    ControlMessage::AudioFrame(mut frame) if audio_enabled && transport == vnt_proto::Transport::Tcp => {
                        if reset_audio_generation || frame.generation != source_audio_generation { continue; }
                        frame.generation = playback_generation;
                        let _ = audio_tx.try_send(frame);
                        continue;
                    }
                    ControlMessage::Pong(p) => {
                        let mut pending = pending_ping.lock().unwrap();
                        if let Some(index) = pending.iter().position(|(token, _)| *token == p.timestamp_ms) {
                            let (_, sent) = pending.remove(index).unwrap();
                            let now = tokio::time::Instant::now();
                            let ms = now.duration_since(sent).as_millis().min(60_000) as u32;
                            rtt_ms = ms;
                            *recent_rtt.lock().unwrap() = Some((now, ms));
                        }
                    }
                    ControlMessage::DesktopState(state) => {
                        if !state.ready && previous_desktop_ready {
                            clipboard_before_switch = clipboard.session_enabled(clipboard_key);
                            clipboard.set_session_enabled(clipboard_key, false);
                        } else if state.ready && !previous_desktop_ready {
                            clipboard.set_session_enabled(clipboard_key, clipboard_before_switch && !info.view_only && clipboard.is_enabled());
                        }
                        previous_desktop_ready = state.ready;
                        let json=serde_json::json!({"type":"desktopState","ready":state.ready,"desktop":state.desktop,"reason":state.reason,"generation":state.generation}).to_string();
                        {
                            let mut cached=stream_cache.lock().unwrap();
                            if let Some(value)=cached.as_ref().and_then(|s|serde_json::from_str::<serde_json::Value>(s).ok()) {
                                let mut value=value;value["desktopReady"]=state.ready.into();value["desktopReason"]=state.reason.clone().into();*cached=Some(value.to_string());
                            }
                        }
                        let _=bridge_tx.send(BridgeMsg::Text(json));
                        if state.ready { let _=control_tx.try_send(ControlMessage::RequestKeyframe); }
                    }
                    ControlMessage::ClipboardPasteResult(result)=>{
                        if let Some(tx)=paste_pending.lock().unwrap().remove(&result.request_id){let _=tx.send(result);}
                    }
                    ControlMessage::SessionEnd(se) => break se.reason,
                    ControlMessage::Ping(p)
                        if control_tx
                            .try_send(ControlMessage::Pong(pb::Pong {
                                timestamp_ms: p.timestamp_ms,
                            }))
                            .is_err() =>
                    {
                        break "控制通道拥塞".to_string()
                    }
                    msg @ (ControlMessage::ClipboardImageReady(_) | ControlMessage::ClipboardImage(_) | ControlMessage::ClipboardImageChunk(_) | ControlMessage::ClipboardImageCancel(_) | ControlMessage::ClipboardText(_) | ControlMessage::ClipboardOffer(_)
                        | ControlMessage::ClipboardRequest(_) | ControlMessage::ClipboardReply(_)) => {
                        dispatch_clipboard(clipboard.as_ref(), clipboard_key, &clip_in_tx, msg).await;
                    }
                    _ => {}
                }

            };
            tracing::info!(session_id=info.session_id, reason, "控制通道结束，最后一次远端管线统计");
            end.finish(&reason);
        }));
    }
    Ok((
        ClientHandle {
            visibility,
            screens: screen_state,
            screen_input_enabled,
            end,
            tasks,
            bridge,
            disconnect_writer,
            control_tx,
            control_available,
            sas_allowed: welcome.host_edition == pb::HostEdition::Installed as i32
                && !welcome.view_only,
            paste_allowed: info.clipboard_enabled && !info.view_only && info.capabilities & vnt_proto::CAP_TEXT !=0,
            paste_pending,
            clipboard: Some(clipboard_registration),
        },
        info,
    ))
}

/// 把对端剪贴板消息转交接收任务。
///
/// reader 循环调用：失败只记录——输入/心跳的分发不能被剪贴板积压阻塞。
async fn dispatch_clipboard(
    clipboard: &dyn crate::clipboard::ClipboardService,
    connection_key: u64,
    tx: &mpsc::Sender<crate::clipboard::InboundClipboard>,
    message: crate::clipboard::InboundClipboard,
) {
    if let Err(e) = clipboard
        .dispatch(connection_key, tx.clone(), message)
        .await
    {
        tracing::debug!("剪贴板消息分发失败: {e}");
    }
}

/// 把前端输入事件发往被控端。
///
/// 前端已经合并相邻移动；这里必须可靠发送所有事件，否则丢弃点击前的
/// 最后一次移动会让随后的点击/滚轮作用于旧位置。超时由调用方结束会话。
pub async fn send_input(tx: &mpsc::Sender<ControlMessage>, ev: InputArg) -> Result<(), String> {
    let ev = ev.into_proto();
    if !vnt_proto::validation::valid_input(&ev) {
        return Err("无效的输入事件".into());
    }
    let msg = ControlMessage::InputEvent(ev);
    tokio::time::timeout(Duration::from_secs(2), tx.send(msg))
        .await
        .map_err(|_| "可靠输入发送超时".to_string())?
        .map_err(|_| "输入通道已关闭".to_string())
}

pub fn validate_fec(
    enabled: bool,
    transport: vnt_proto::Transport,
    confirmed: u32,
) -> Result<(), String> {
    let expected = if enabled && transport != vnt_proto::Transport::Tcp {
        vnt_net::fec::VERSION
    } else {
        0
    };
    if confirmed != expected {
        return Err("对端返回无效的 FEC 协商结果".into());
    }
    Ok(())
}

pub fn validate_welcome(
    welcome: &pb::Welcome,
    codecs: &[String],
    transport: vnt_proto::Transport,
) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    if welcome.screens.is_empty()
        || welcome.screens.len() > 128
        || welcome.screens.iter().any(|s| {
            s.stream_id == 0
                || s.stream_id > 128
                || !ids.insert(s.stream_id)
                || s.generation == 0
                || s.name.len() > 256
                || s.reason.len() > 1024
                || (s.available
                    && (!vnt_proto::validation::valid_video_size(s.width, s.height)
                        || !(1..=120).contains(&s.fps)
                        || !vnt_proto::Codec::from_proto(s.codec()).is_some_and(|codec| {
                            codecs.iter().any(|offered| offered == codec.as_str())
                        })))
        })
    {
        return Err("服务端返回了无效的多屏参数".into());
    }
    if vnt_proto::Transport::from_wire(welcome.transport)? != transport {
        return Err("服务端确认的通信协议与请求不一致".into());
    }
    if !matches!(
        pb::HostEdition::try_from(welcome.host_edition),
        Ok(pb::HostEdition::Installed | pb::HostEdition::Portable)
    ) {
        return Err("服务端返回了无效的被控版本类型".into());
    }
    let codec = vnt_proto::Codec::from_proto(welcome.codec()).ok_or("未知编码")?;
    if welcome.session_id == 0
        || (transport != vnt_proto::Transport::Tcp && welcome.bind_token.len() != 16)
    {
        return Err("服务端返回了无效的会话凭证".into());
    }
    if !codecs.iter().any(|offered| offered == codec.as_str()) {
        return Err("服务端选择了客户端未提供的编码格式".into());
    }
    if !vnt_proto::validation::valid_video_size(welcome.width, welcome.height)
        || !(1..=120).contains(&welcome.fps)
    {
        return Err("服务端返回了无效的视频参数".into());
    }
    Ok(())
}

pub async fn send_input_events(
    tx: &mpsc::Sender<ControlMessage>,
    events: Vec<InputArg>,
) -> Result<(), String> {
    // Never truncate: the omitted tail may contain the only key/button release.
    if events.len() > 256 {
        return Err("输入批次超过 256 个事件".into());
    }
    for event in events {
        send_input(tx, event).await?;
    }
    Ok(())
}
