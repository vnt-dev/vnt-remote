use super::{
    management::{read_json, write_json},
    native,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use vnt_net::tcp::ControlChannel;
use vnt_proto::{control::ControlMessage, pb};
#[derive(Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    pub secret: String,
    pub config: crate::config::AppConfig,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub desktop: String,
    pub monitors: Vec<vnt_capture::MonitorInfo>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DesktopEvent {
    pub generation: u64,
    pub metadata: Metadata,
    pub capture_ready: bool,
    pub reason: String,
}

#[derive(Serialize, Deserialize)]
pub struct OpenSession {
    #[serde(default)]
    pub effects: Option<super::desktop_effects::Settings>,
    pub pipe_name: String,
    #[serde(default)]
    pub region: Option<vnt_capture::ScreenRegion>,
    pub display: Option<super::display::Operation>,
    #[serde(default)]
    pub metadata: bool,
    #[serde(default)]
    pub available_only: bool,
}

#[derive(Serialize, Deserialize)]
// Bootstrap messages are serialized immediately; keeping the payload inline avoids an extra allocation.
#[allow(clippy::large_enum_variant)]
pub enum RootRequest {
    Open(OpenSession),
    CaptureBorder(bool),
}

pub async fn run(name: &str, parent: u32, user: bool) -> anyhow::Result<()> {
    let mut root = native::connect(name).await?;
    anyhow::ensure!(
        native::server_pid(&root)? == parent,
        "拒绝非服务创建的代理管道"
    );
    anyhow::ensure!(
        super::management::service_pid()? == parent,
        "代理父进程不是已注册的被控服务"
    );
    // A normal user may not open a LocalSystem process. SCM's installed image
    // plus its live PID and the pipe's kernel PID establish the same boundary
    // without requiring PROCESS_QUERY_LIMITED_INFORMATION on SYSTEM.
    let image = super::management::service_executable()?
        .ok_or_else(|| anyhow::anyhow!("未安装被控服务"))?;
    anyhow::ensure!(
        image.canonicalize()? == std::env::current_exe()?.canonicalize()?,
        "代理程序与已注册服务映像不匹配"
    );
    if !user {
        native::require_system(parent)?;
    }
    let boot: Bootstrap = read_json(&mut root).await?;
    vnt_capture::set_capture_border(boot.config.capture_border);
    let initialized = (|| -> anyhow::Result<_> {
        let desktop = native::desktop_name()?;
        let session = native::current_session()?;
        let secure = !desktop.eq_ignore_ascii_case("default");
        let (monitors, clipboard) = if user {
            anyhow::ensure!(!secure, "安全桌面暂停剪贴板");
            let clipboard = crate::clipboard::ClipboardService::try_shared()
                .map_err(|e| anyhow::anyhow!("初始化用户剪贴板失败：{e}"))?;
            (Vec::new(), Some(clipboard))
        } else {
            vnt_capture::enable_service_desktop(secure);
            (
                vnt_capture::list_monitors()
                    .map_err(|e| anyhow::anyhow!("Windows 会话 {session}，桌面 {desktop}：{e}"))?,
                None,
            )
        };
        Ok((desktop, session, secure, monitors, clipboard))
    })();
    let (desktop, session, _secure, monitors, clipboard) = match initialized {
        Ok(value) => value,
        Err(error) => {
            write_json(&mut root, &Err::<Metadata, _>(format!("{error:#}"))).await?;
            return Err(error);
        }
    };
    if user {
        super::desktop_effects::initialize();
    }
    write_json(
        &mut root,
        &Ok::<_, String>(Metadata {
            desktop: desktop.clone(),
            monitors: monitors.clone(),
        }),
    )
    .await?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let watch_cancel = cancel.clone();
    let identity = if user {
        String::new()
    } else {
        native::session_identity(session)?
    };
    let switching = (!user).then(|| {
        vnt_capture::DesktopCapture::named(
            boot.config.monitor_index,
            boot.config.selected_targets.first().and_then(|t| match t {
                crate::config::CaptureTarget::Monitor { name } => Some(name.clone()),
                crate::config::CaptureTarget::Region { id } => boot
                    .config
                    .screen_regions
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| r.monitor_name.clone()),
            }),
            Some(desktop.clone()),
        )
    });
    let watch_desktop = switching.clone();
    let watcher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if native::capture_session().ok().flatten() != Some(session) {
                tracing::info!(session, "被控交互会话变化或不可用，退出代理");
                watch_cancel.cancel();
                break;
            }
            let observed = if user {
                Ok(identity.clone())
            } else {
                native::session_identity(session)
            };
            if observed.as_ref().is_ok_and(|value| value != &identity) {
                tracing::info!(session, "登录身份变化，退出代理");
                watch_cancel.cancel();
                break;
            }
            // A transient identity-query failure pauses capture rather than
            // incorrectly classifying lock/UAC as a logout.
            let current = observed.ok().and_then(|_| native::desktop_name().ok());
            if let Some(target) = &watch_desktop {
                target.set_desktop(current);
                let state = target.snapshot();
                if target.restart_requested() || target.recovery_overdue(std::time::Instant::now())
                {
                    tracing::warn!(generation=state.generation,error=%state.error,"桌面恢复超时，重建代理");
                    watch_cancel.cancel();
                    break;
                }
            } else if current.as_deref() != Some(&desktop) {
                watch_cancel.cancel();
                break;
            }
        }
    });
    let host = if !user {
        let mut cfg = boot.config.clone();
        cfg.password = boot.secret.clone();
        let clipboard = crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
        clipboard.set_enabled(false);
        Some(Arc::new(crate::server::AgentHost::with_desktop(
            crate::server::ServerConfig {
                password: cfg.password,
                port: 0,
                monitor_index: cfg.monitor_index,
                region: None,
                fps: 30,
                bitrate_kbps: 30_000,
                hardware_encode: cfg.hardware_encode,
                audio: false,
                view_only: !cfg.allow_control,
                clipboard,
                synthetic: false,
            },
            switching.clone().unwrap(),
        )))
    } else {
        None
    };
    let audio = vnt_audio::SharedAudio::new(false);
    let (request_tx, mut requests) = tokio::sync::mpsc::channel(1);
    let (mut root_read, mut root_write) = tokio::io::split(root);
    let event_desktop = switching.clone();
    let event_cancel = cancel.clone();
    let root_events = tokio::spawn(async move {
        let mut previous = None;
        let mut timer = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! { _ = event_cancel.cancelled() => break, _ = timer.tick() => {} }
            let Some(desktop) = &event_desktop else {
                continue;
            };
            let snapshot = desktop.snapshot();
            let next = (snapshot.generation, snapshot.capture_ready);
            if previous == Some(next) {
                continue;
            }
            previous = Some(next);
            let event = DesktopEvent {
                generation: snapshot.generation,
                metadata: Metadata {
                    desktop: snapshot.desktop.unwrap_or_default(),
                    monitors: snapshot.monitors,
                },
                capture_ready: snapshot.capture_ready,
                reason: snapshot.error.chars().take(256).collect(),
            };
            let sent = tokio::select! {
                _ = event_cancel.cancelled() => break,
                result = write_json(&mut root_write, &event) => result,
            };
            if sent.is_err() {
                event_cancel.cancel();
                break;
            }
        }
    });
    let root_reader = tokio::spawn(async move {
        loop {
            let request = read_json::<_, RootRequest>(&mut root_read).await;
            let failed = request.is_err();
            if request_tx.send(request).await.is_err() || failed {
                break;
            }
        }
    });
    let mut tasks = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            _=cancel.cancelled()=>break Ok(()),
            Some(result)=tasks.join_next(),if !tasks.is_empty()=>{
                if let Err(e) = result {
                    tracing::warn!("代理会话任务异常：{e}");
                }
            },
            request=requests.recv()=>{
                let request = match request {
                    Some(Ok(RootRequest::Open(r))) => r,
                    Some(Ok(RootRequest::CaptureBorder(enabled))) => {
                        vnt_capture::set_capture_border(enabled);
                        continue;
                    },
                    Some(Err(e)) => break Err(e),
                    None => break Ok(()),
                };
                if !request.pipe_name.starts_with(r"\\.\pipe\VntRemote.Session.") {
                    break Err(anyhow::anyhow!("会话管道名称无效"));
                               }
                if let Some(settings) = request.effects {
                    // Dedicated logical-session lease, independent of audio,
                    // clipboard, capture workers and desktop transitions.
                    anyhow::ensure!(user, "桌面效果只能由用户代理处理");
                    let cancel = cancel.clone();
                    let allowed = boot.config.allow_control;
                    tasks.spawn(async move {
                        let result = async {
                            let mut pipe = native::connect(&request.pipe_name).await?;
                            anyhow::ensure!(native::server_pid(&pipe)? == parent, "桌面效果管道不属于服务");
                            let lease = allowed.then(|| super::desktop_effects::Lease::acquire(settings)).flatten();
                            write_json(&mut pipe, &lease.is_some()).await?;
                            let mut byte = [0u8; 1];
                            tokio::select! {
                                _ = cancel.cancelled() => {},
                                _ = tokio::io::AsyncReadExt::read(&mut pipe, &mut byte) => {},
                            }
                            drop(lease);
                            // Acknowledge only after restoration/reaggregation.
                            let _ = write_json(&mut pipe, &true).await;
                            Ok::<_, anyhow::Error>(())
                        }.await;
                        if let Err(error) = result { tracing::debug!("桌面效果会话结束：{error:#}"); }
                    });
                    continue;
                }
                if request.metadata {
                    let cancel = cancel.clone();
                    tasks.spawn(async move {
                        let result = async {
                            let mut pipe = native::connect(&request.pipe_name).await?;
                            anyhow::ensure!(native::server_pid(&pipe)? == parent, "元数据管道不属于服务");
                            let result = async {
                                loop {
                                    if cancel.is_cancelled() {
                                        anyhow::bail!("代理已停止");
                                    }
                                    let before = native::desktop_name()?;
                                    let monitors = tokio::task::spawn_blocking(move || {
                                        if request.available_only {
                                            vnt_capture::list_available_monitors()
                                        } else {
                                            vnt_capture::list_monitors()
                                        }
                                    }).await?;
                                    if native::desktop_name().ok().as_ref() == Some(&before) {
                                        let monitors = monitors.map_err(|error| anyhow::anyhow!(
                                            "Windows 会话 {session}，桌面 {before}：{error}"
                                        ))?;
                                        return Ok::<_, anyhow::Error>(Metadata { desktop: before, monitors });
                                    }
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                }
                            };
                            let result = tokio::time::timeout(Duration::from_secs(10), result)
                                .await
                                .map_err(|e| e.to_string())
                                .and_then(|r| r.map_err(|e| e.to_string()));
                            write_json(&mut pipe, &result).await
                        }
                        .await;
                        if let Err(error) = result {
                            tracing::debug!("元数据查询失败：{error:#}");
                        }
                    });
                    continue;
                }
                if let Some(operation) = request.display {
                    // Serialize mode operations on the authenticated root channel.
                    // Restoration cannot overtake an already submitted apply.
                    let response = async {
                        let mut pipe = native::connect(&request.pipe_name).await?;
                        anyhow::ensure!(native::server_pid(&pipe)? == parent, "显示管道不属于服务");
                        let result = super::display::execute(operation).map_err(|error| format!("{error:#}"));
                        write_json(&mut pipe, &result).await
                    }.await;
                    if let Err(error) = response { tracing::warn!("显示模式请求失败：{error:#}"); }
                    continue;
                }
                if tasks.len() >= crate::server::MAX_CONNECTIONS { continue; }
                let host = host.clone();
                let boot = boot.clone();
                let cancel = cancel.clone();
                let clipboard = clipboard.clone();
                let audio = audio.clone();
                tasks.spawn(async move {
                    let result = async {
                        let pipe = native::connect(&request.pipe_name).await?;
                        anyhow::ensure!(native::server_pid(&pipe)? == parent, "会话管道不属于服务");
                        if let Some(host) = host {
                            let host = request.region.map(|region| host.for_region(region)).unwrap_or(host);
                            host.serve_with_cancel(pipe, cancel)
                                .await
                                .map_err(anyhow::Error::msg)
                        } else {
                            user_session(pipe, boot, cancel, clipboard.unwrap(), audio).await
                        }
                    }
                    .await;
                    if let Err(e) = result {
                        tracing::warn!("代理会话结束：{e}");
                    }
                });
            }
        }
    };
    cancel.cancel();
    // Close admission immediately. Other sessions must not reuse this root
    // while its old desktop resources are finishing their bounded cleanup.
    root_reader.abort();
    root_events.abort();
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.abort_all();
    audio.stop();
    watcher.abort();
    result
}

// OLE and audio run as the logged-on user, never as SYSTEM.
pub(super) async fn user_session<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    pipe: S,
    boot: Bootstrap,
    desktop_cancel: tokio_util::sync::CancellationToken,
    clipboard: Arc<crate::clipboard::ClipboardService>,
    audio: vnt_audio::SharedAudio,
) -> anyhow::Result<()> {
    let mut channel = ControlChannel::new(pipe);
    let Some(ControlMessage::Hello(h)) = channel.recv().await? else {
        anyhow::bail!("代理握手缺失")
    };
    anyhow::ensure!(h.proto_ver == vnt_proto::PROTO_VER, "代理协议版本不匹配");
    let (nonce, proof) = vnt_net::auth::challenge(&mut channel).await?;
    anyhow::ensure!(
        vnt_proto::auth::verify(&boot.secret, &nonce, &proof),
        "代理认证失败"
    );
    let (mut reader, mut writer) = channel.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let (input, receive) = tokio::sync::mpsc::channel(crate::clipboard::INBOUND_QUEUE);
    clipboard.set_enabled(boot.config.clipboard && boot.config.allow_control);
    let cancel = tokio_util::sync::CancellationToken::new();
    let registration = clipboard.register(crate::clipboard::SessionSpec {
        capabilities: h.capabilities & vnt_proto::platform_capabilities(),
        session_id: 1,
        file_tx: tx.clone(),
        control_tx: tx.clone(),
        in_rx: receive,
        session_cancel: cancel.clone(),
        sink: Arc::new(crate::NoopEventSink),
        enabled: clipboard.is_enabled() && !h.audio_enabled,
    });
    if boot.config.allow_control {
        clipboard.allow_remote_paste(registration.key());
    }
    // The broker must not publish this channel until its clipboard receiver is
    // registered. The media handshake alone does not make the user agent ready.
    writer
        .send(&ControlMessage::Welcome(pb::Welcome::default()))
        .await?;
    let subscription = if boot.config.audio && h.audio_enabled {
        match audio.subscribe() {
            Ok(subscription) => Some(subscription),
            Err(error) => {
                let _ = tx.try_send(ControlMessage::AudioState(pb::AudioState {
                    generation: 1,
                    available: false,
                    reason: format!("音频不可用：{error}"),
                }));
                None
            }
        }
    } else {
        None
    };
    let audio_task=subscription.map(|mut subscription| {
        let tx=tx.clone();
        tokio::spawn(async move {
            let initial=subscription.state.borrow_and_update().clone();
            let _=tx.send(ControlMessage::AudioState(initial)).await;
            loop {
                tokio::select! {
                    changed=subscription.state.changed()=>{
                        if changed.is_err() {
                            break;
                        }
                        let state = subscription.state.borrow_and_update().clone();
                        if tx.send(ControlMessage::AudioState(state)).await.is_err() {
                            break;
                        }
                    }
                    frame=subscription.frames.recv()=>{
                        match frame {
                            Ok(frame) if frame.captured_at.elapsed() < Duration::from_millis(60) => {
                                let _ = tx.try_send(ControlMessage::AudioFrame((*frame.packet).clone()));
                            }
                            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(_) => break,
                        }
                    }
                }
            }
        })
    });
    let result = loop {
        tokio::select! {
            _=desktop_cancel.cancelled()=>break Ok(()),
            msg=reader.recv()=>{
                match msg {
                    Ok(Some(
                        msg @ (ControlMessage::ClipboardImageReady(_) | ControlMessage::ClipboardImage(_) | ControlMessage::ClipboardImageChunk(_) | ControlMessage::ClipboardImageCancel(_) | ControlMessage::ClipboardText(_) | ControlMessage::ClipboardPaste(_)
                        | ControlMessage::ClipboardOffer(_)
                        | ControlMessage::ClipboardRequest(_)
                        | ControlMessage::ClipboardReply(_)),
                    )) if boot.config.allow_control => {
                        crate::clipboard::dispatch_inbound(&clipboard, registration.key(), &input, msg)
                            .await
                            .map_err(anyhow::Error::msg)?;
                    }
                    Ok(Some(ControlMessage::ResetInput(_))) => {clipboard.cancel_remote_paste(registration.key());}
                    Ok(Some(ControlMessage::TransportState(state))) => {
                        if state.kind == 1 { clipboard.transport_changed(registration.key(), state.available); }
                        if state.kind == 0 { clipboard.control_changed(registration.key(), state.available); }
                    }
                    Ok(Some(ControlMessage::Ping(p))) => {
                        let _ = tx.try_send(ControlMessage::Pong(pb::Pong {
                            timestamp_ms: p.timestamp_ms,
                        }));
                    }
                    Ok(Some(ControlMessage::SessionEnd(_))) | Ok(None) => break Ok(()),
                    Err(e) => break Err(e.into()),
                    _ => {}
                }
            },
            msg=rx.recv()=>{
                let Some(msg) = msg else { break Ok(()) };
                if let Err(e) = writer.send(&msg).await {
                    break Err(e.into());
                }
            }
        }
    };
    cancel.cancel();
    drop(registration);
    if let Some(t) = audio_task {
        t.abort();
    }
    result
}
