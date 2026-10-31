//! Tauri 命令（前端 invoke 入口）。

use crate::client::{ConnectParams, InputArg};
use crate::config::AppConfig;
use crate::AppState;
#[cfg(windows)]
use tauri::Manager;

#[tauri::command]
pub fn computer_name() -> String {
    vnt_host::link::default_device_name()
}

#[tauri::command]
pub fn app_edition() -> &'static str {
    if crate::config::INSTALLED {
        "installed"
    } else {
        "portable"
    }
}

#[tauri::command]
pub fn open_release_page() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::{
            core::PCWSTR,
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        };
        let url = vnt_host::windows::native::wide(
            "https://github.com/vnt-dev/vnt-remote/releases/latest",
        );
        let verb = vnt_host::windows::native::wide("open");
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize > 32 {
            Ok(())
        } else {
            Err(format!(
                "无法打开发布页：Windows 错误 {}",
                result.0 as isize
            ))
        }
    }
    #[cfg(target_os = "macos")]
    {
        vnt_macos::json(
            3,
            &serde_json::json!("https://github.com/vnt-dev/vnt-remote/releases/latest"),
        )
        .map(|_| ())
    }
}

// ---------- 配置 ----------

#[tauri::command]
pub async fn get_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<AppConfig, String> {
    #[cfg(not(windows))]
    let _ = app;
    #[cfg(windows)]
    let cfg = if crate::config::INSTALLED {
        app.state::<crate::desktop::Desktop>().read_config().await?
    } else {
        state.store.load()?
    };
    #[cfg(not(windows))]
    let cfg = state.store.load()?;
    *state.config.lock().unwrap() = cfg.clone();
    Ok(cfg)
}

/// 本机局域网出口地址，用于「允许访问本机」横幅展示与复制。
///
/// UDP connect 只选择路由不实际发包，按常见网段依次探测；
/// 全部失败（如纯离线多网卡）时返回错误，前端降级为仅显示端口。
#[tauri::command]
pub fn local_ip() -> Result<String, String> {
    for probe in ["8.8.8.8:80", "192.168.1.1:80", "10.255.255.5:80"] {
        let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if sock.connect(probe).is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if !addr.ip().is_loopback() {
                    return Ok(addr.ip().to_string());
                }
            }
        }
    }
    Err("未能获取本机局域网地址".into())
}

/// 安装版由服务串行校验并保存；免安装版保留用户目录直接保存。
#[tauri::command]
pub async fn set_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    mut config: AppConfig,
) -> Result<(), String> {
    config.validate()?;
    let _guard = state.config_update.lock().await;
    #[cfg(windows)]
    if crate::config::INSTALLED {
        return app
            .state::<crate::desktop::Desktop>()
            .request(vnt_host::windows::local::Action::SaveConfig(config))
            .await
            .map(|_| ());
    }
    let previous = state.store.load()?;
    config.servers = previous.servers.clone();
    if config.port != previous.port {
        let status = host_status(&app, &state).await?;
        status.validate_port_change(config.port)?;
    }
    config.apply_password_update(&previous);
    if config != previous {
        state.store.save(&config)?;
    }
    vnt_capture::set_capture_border(config.capture_border);
    *state.config.lock().unwrap() = config;
    Ok(())
}

#[tauri::command]
pub async fn server_link_status(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<vnt_host::link::ProfileStatus>, String> {
    #[cfg(windows)]
    if crate::config::INSTALLED {
        return app.state::<crate::desktop::Desktop>().link_status().await;
    }
    #[cfg(not(windows))]
    let _ = app;
    Ok(state.link.snapshot().await)
}
#[tauri::command]
pub async fn server_link_apply(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config: vnt_host::link::LinkConfig,
) -> Result<(), String> {
    config.validate()?;
    let _guard = state.config_update.lock().await;
    #[cfg(windows)]
    if crate::config::INSTALLED {
        return app
            .state::<crate::desktop::Desktop>()
            .request(vnt_host::windows::local::Action::ConfigureLink(config))
            .await
            .map(|_| ());
    }
    #[cfg(not(windows))]
    let _ = app;
    let mut saved = state.store.load()?;
    saved.upsert_server(config)?;
    state.store.save(&saved)?;
    *state.config.lock().unwrap() = saved.clone();
    state.link.configure(saved.servers).await
}

#[tauri::command]
pub async fn server_link_remove(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config_id: String,
) -> Result<(), String> {
    let _guard = state.config_update.lock().await;
    #[cfg(windows)]
    if crate::config::INSTALLED {
        return app
            .state::<crate::desktop::Desktop>()
            .request(vnt_host::windows::local::Action::RemoveLink(config_id))
            .await
            .map(|_| ());
    }
    #[cfg(not(windows))]
    let _ = app;
    let previous = state.store.load()?;
    let mut saved = previous.clone();
    saved.servers.retain(|s| s.id != config_id);
    // Stop removed instances before committing the deletion.
    state.link.configure(saved.servers.clone()).await?;
    if let Err(error) = state.store.save(&saved) {
        state.link.configure(previous.servers).await?;
        return Err(error);
    }
    *state.config.lock().unwrap() = saved;
    Ok(())
}

// ---------- 剪贴板 ----------

/// 取消一个剪贴板文件传输（前端"取消"按钮）。
///
/// 被控端与控制端会话共用：找到即广播取消，收发任务各自清理。
#[tauri::command]
pub fn clipboard_cancel_transfer(
    state: tauri::State<'_, AppState>,
    connection_id: String,
    transfer_id: u32,
) -> Result<(), String> {
    if !state.clipboard.cancel_transfer(
        connection_id.parse().map_err(|_| "连接标识无效")?,
        transfer_id,
    ) && !state.host_clipboard.cancel_transfer(
        connection_id.parse().map_err(|_| "连接标识无效")?,
        transfer_id,
    ) {
        return Err("传输不存在或已结束".into());
    }
    Ok(())
}

#[tauri::command]
pub fn client_clipboard_enabled(
    state: tauri::State<'_, AppState>,
    connection_id: String,
) -> Result<bool, String> {
    Ok(state
        .clipboard
        .session_enabled(connection_id.parse().map_err(|_| "连接标识无效")?))
}

/// 控制窗口的剪贴板同步开关（运行时，不持久化）。
#[tauri::command]
pub fn client_set_clipboard_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    connection_id: String,
    enabled: bool,
) -> Result<(), String> {
    if !state
        .clipboard
        .set_session_enabled(connection_id.parse().map_err(|_| "连接标识无效")?, enabled)
    {
        return Err("会话不存在".into());
    }
    let _ = tauri::Emitter::emit(
        &app,
        "clipboard://enabled",
        serde_json::json!({ "connectionId": connection_id, "enabled": enabled }),
    );
    Ok(())
}

// ---------- 被控端 ----------

#[cfg(windows)]
fn require_service() -> Result<(), String> {
    let status = vnt_host::windows::management::scm_status().map_err(|e| e.to_string())?;
    if !status.installed {
        return Err("请先安装程序".into());
    }
    Ok(())
}
#[tauri::command]
pub async fn server_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let _guard = state.config_update.lock().await;
    if !crate::config::INSTALLED {
        let mut host = state.host.lock().await;
        if host.is_some() {
            return Ok(());
        }
        let cfg = state.store.load()?;
        crate::platform::check_host_permissions(cfg.allow_control)?;
        vnt_capture::set_capture_border(cfg.capture_border);
        state
            .host_clipboard
            .set_enabled(cfg.clipboard && cfg.allow_control);
        *host = Some(
            crate::server::start_portable(
                std::sync::Arc::new(crate::TauriEventSink(app)),
                crate::server::ServerConfig {
                    password: cfg.password,
                    port: cfg.port,
                    monitor_index: cfg.monitor_index,
                    region: None,
                    fps: 60,
                    bitrate_kbps: 30000,
                    hardware_encode: cfg.hardware_encode,
                    audio: cfg.audio,
                    view_only: !cfg.allow_control,
                    clipboard: state.host_clipboard.clone(),
                    synthetic: false,
                },
                state.store.portable_path(),
            )
            .await?,
        );
        return Ok(());
    }
    #[cfg(windows)]
    {
        require_service()?;
        app.state::<crate::desktop::Desktop>()
            .request(vnt_host::windows::local::Action::StartHost)
            .await
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        Err("macOS 不支持 Windows 服务操作".into())
    }
}
#[tauri::command]
pub async fn server_stop(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    #[cfg(not(windows))]
    let _ = app;
    let _guard = state.config_update.lock().await;
    if !crate::config::INSTALLED {
        if let Some(host) = state.host.lock().await.take() {
            host.stop().await;
        }
        state.host_clipboard.set_enabled(false);
        return Ok(());
    }
    #[cfg(windows)]
    {
        require_service()?;
        app.state::<crate::desktop::Desktop>()
            .request(vnt_host::windows::local::Action::StopHost)
            .await
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        Err("macOS 不支持 Windows 服务操作".into())
    }
}
#[tauri::command]
pub async fn server_set_auto_start(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    #[cfg(not(windows))]
    let _ = enabled;
    let _guard = state.config_update.lock().await;
    if !crate::config::INSTALLED {
        return Err("免安装版不支持开机启动".into());
    }
    #[cfg(windows)]
    {
        require_service()?;
        vnt_host::windows::management::elevated(vnt_host::windows::management::Request::AutoStart(
            enabled,
        ))
        .await
        .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err("macOS 不支持 Windows 服务操作".into())
    }
}
#[tauri::command]
pub async fn server_status(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<crate::ServerStatus, String> {
    host_status(&app, &state).await
}

#[tauri::command]
pub async fn desktop_update_handoff(app: tauri::AppHandle, preparing: bool) -> Result<(), String> {
    #[cfg(not(windows))]
    let _ = (app, preparing);
    if !crate::config::INSTALLED {
        return Ok(());
    }
    #[cfg(windows)]
    {
        app.state::<crate::desktop::Desktop>()
            .request(if preparing {
                vnt_host::windows::local::Action::PrepareUpdate
            } else {
                vnt_host::windows::local::Action::CancelUpdate
            })
            .await
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        Err("macOS 不支持 Windows 服务操作".into())
    }
}

async fn host_status(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<crate::ServerStatus, String> {
    #[cfg(not(windows))]
    let _ = app;
    #[cfg(windows)]
    if crate::config::INSTALLED {
        let mut status = app
            .state::<crate::desktop::Desktop>()
            .request(vnt_host::windows::local::Action::Status)
            .await?;
        let service = vnt_host::windows::management::scm_status().map_err(|e| e.to_string())?;
        status.auto_start = service.auto_start;
        status.service_state = service.service_state;
        status.error = service.error.or(status.error);
        return Ok(status);
    }
    let host = state.host.lock().await;
    let cfg = state.store.load()?;
    Ok(crate::ServerStatus {
        installed: false,
        running: host.is_some(),
        state: if host.is_some() { "running" } else { "stopped" }.into(),
        port: host.as_ref().map_or(cfg.port, |h| h.port),
        sessions: host.as_ref().map_or_else(Vec::new, |h| h.active_sessions()),
        password_configured: cfg.password_configured,
        monitors: vnt_capture::list_monitors().unwrap_or_default(),
        ..Default::default()
    })
}
#[tauri::command]
pub async fn list_monitors(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<vnt_capture::MonitorInfo>, String> {
    let status = host_status(&app, &state).await?;
    // The broker reports running before its asynchronous metadata probe completes.
    // Keep enumerating locally until the service has a usable monitor snapshot.
    if status.installed && status.state == "running" && !status.monitors.is_empty() {
        return Ok(status.monitors);
    }
    tokio::task::spawn_blocking(vnt_capture::list_monitors)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_audio_playback_devices() -> Result<Vec<vnt_audio::PlaybackDevice>, String> {
    tokio::task::spawn_blocking(vnt_audio::list_playback_devices)
        .await
        .map_err(|e| e.to_string())?
}

// ---------- 控制端 ----------

#[tauri::command]
pub async fn client_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    window: tauri::WebviewWindow,
    mut params: ConnectParams,
) -> Result<crate::client::ClientSessionInfo, String> {
    params.validate()?;
    let mut resolution_notice = state
        .resolution_notices
        .lock()
        .unwrap()
        .get(window.label())
        .cloned()
        .unwrap_or_default();
    if params.resolution_mode != "none" && (params.target_width == 0 || params.target_height == 0) {
        match window.current_monitor() {
            Ok(Some(monitor)) => {
                params.target_width = monitor.size().width;
                params.target_height = monitor.size().height;
            }
            _ => {
                params.resolution_mode = "none".into();
                resolution_notice = "无法获取主控屏幕尺寸，按原分辨率连接".into();
            }
        }
    }
    if state
        .child_windows
        .lock()
        .unwrap()
        .contains_key(window.label())
    {
        return Err("子窗口不能建立独立连接".into());
    }
    let label = window.label().to_string();
    crate::remote_windows::close_children(&app, &label);
    let (generation, cancel) = state
        .client
        .lock()
        .unwrap()
        .get_mut(&label)
        .ok_or("控制窗口已关闭")?
        .begin();
    let sink = std::sync::Arc::new(crate::WindowEventSink(
        app.clone(),
        label.clone(),
        generation,
        state
            .client
            .lock()
            .unwrap()
            .get(&label)
            .ok_or("控制窗口已关闭")?
            .event_generation
            .clone(),
    ));
    let clipboard = state.clipboard.clone();
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("连接已取消".to_string()),
        result = async {
            let prepared = if params.transport == vnt_proto::Transport::Vnt {
                let node = params.server_node_id.clone().ok_or("请选择服务端在线设备")?;
                let config_id = params.server_config_id.clone().ok_or("请选择设备所属的服务端配置")?;
                let target = vnt_host::link::LinkTarget { config_id, network_code: params.server_network_code.clone(), node };
                #[cfg(windows)]
                let prepared = if crate::config::INSTALLED {
                    app.state::<crate::desktop::Desktop>().connect_overlay(target).await?
                } else {
                    let link = state.link.select(&target).await.map_err(|e| format!("{e:#}"))?;
                    vnt_net::overlay::prepare(link.peer(&target.node).await.map_err(|e| format!("{e:#}"))?).await.map_err(|e| e.to_string())?
                };
                #[cfg(not(windows))]
                let prepared = {
                    let link = state.link.select(&target).await.map_err(|e| format!("{e:#}"))?;
                    vnt_net::overlay::prepare(link.peer(&target.node).await.map_err(|e| format!("{e:#}"))?).await.map_err(|e| e.to_string())?
                };
                params.host = prepared.peer.ip().to_string(); params.port = prepared.peer.port();
                Some(prepared)
            } else { None };
            crate::client::connect_prepared(sink, params, clipboard, prepared).await
        } => result,
    };
    let mut clients = state.client.lock().unwrap();
    let connections = clients.get_mut(&label).ok_or("控制窗口已关闭")?;
    connections.finish_pending(generation);
    let (handle, mut info) = result?;
    if !resolution_notice.is_empty() {
        info.resolution_reason = resolution_notice;
    }
    connections.commit(generation, handle)?;
    connections.info = Some(info.clone());
    Ok(info)
}

#[tauri::command]
pub async fn client_disconnect(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    session_id: Option<u32>,
) -> Result<(), String> {
    let child = state
        .child_windows
        .lock()
        .unwrap()
        .get(window.label())
        .cloned();
    if let Some(child) = child {
        if session_id.is_none_or(|id| id == child.session_id) {
            let _ = window.close();
        }
        return Ok(());
    }
    // An obsolete frontend must not tear down a replacement session's children.
    let matches = state
        .client
        .lock()
        .unwrap()
        .get(window.label())
        .is_some_and(|c| {
            session_id.is_none()
                || c.handle
                    .as_ref()
                    .is_some_and(|h| Some(h.session_id()) == session_id)
        });
    if !matches {
        return Ok(());
    }
    crate::remote_windows::close_children(&app, window.label());
    let mut clients = state.client.lock().unwrap();
    let Some(connections) = clients.get_mut(window.label()) else {
        return Ok(());
    };
    if session_id.is_none()
        || connections
            .handle
            .as_ref()
            .is_some_and(|handle| Some(handle.session_id()) == session_id)
    {
        connections.cancel();
    }
    Ok(())
}

#[tauri::command]
pub async fn client_send_input(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    event: InputArg,
    session_id: u32,
) -> Result<(), String> {
    send_input_batch_target(&window, state, vec![event], session_id, None).await
}

/// Only the requesting window's current session can own keyboard capture.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardCaptureArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[tauri::command]
pub async fn client_set_keyboard_capture(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    session_id: u32,
    enabled: bool,
    area: Option<KeyboardCaptureArea>,
    target: Option<crate::client::ScreenInputTarget>,
) -> Result<bool, String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        // Hover owns the keyboard, including a visible background window.
        // Native hit testing verifies the cursor against this client rectangle
        // and the topmost window, independently of delayed webview events.
        let owner = crate::remote_windows::owner(&state, window.label(), session_id)?;
        let lane = state
            .client
            .lock()
            .unwrap()
            .get(&owner)
            .ok_or("会话已结束")?
            .input_lane
            .clone();
        let _guard = lane.lock().await;
        if !state
            .client
            .lock()
            .unwrap()
            .get(&owner)
            .is_some_and(|c| std::sync::Arc::ptr_eq(&c.input_lane, &lane))
        {
            return Ok(false);
        }
        if enabled {
            if let Some(target) = target {
                if crate::remote_windows::claim(&window, &state, &owner, session_id, target)
                    .await?
                    .is_none()
                {
                    return Ok(false);
                }
            }
        }
        #[cfg(windows)]
        let hwnd = window.hwnd().map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        let hwnd = window.ns_window().map_err(|e| e.to_string())? as usize;
        let mut clients = state.client.lock().unwrap();
        let Some(connection) = clients.get_mut(&owner) else {
            return Ok(false);
        };
        let Some(handle) = connection
            .handle
            .as_ref()
            .filter(|h| h.session_id() == session_id)
        else {
            return Ok(false);
        };
        if !enabled {
            if connection.keyboard_owner.as_deref() != Some(window.label()) {
                return Ok(false);
            }
            connection.keyboard.take();
            connection.keyboard_owner = None;
            return Ok(false);
        }
        let area = area
            .filter(|r| r.right > r.left && r.bottom > r.top)
            .ok_or("远程画面范围无效")?;
        if let Some(capture) = connection.keyboard.as_ref() {
            capture.set_area(area);
        } else {
            #[cfg(windows)]
            let native_window = windows::Win32::Foundation::HWND(hwnd.0);
            #[cfg(target_os = "macos")]
            let native_window = hwnd;
            connection.keyboard = Some(handle.capture_keyboard(native_window, area, target)?);
            connection.keyboard_owner = Some(window.label().to_owned());
        }
        Ok(true)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (window, state, session_id, enabled, area, target);
        Ok(false)
    }
}

/// 批量上行输入事件（顺序即数组顺序）。
///
/// 相比逐事件 invoke：拥塞时不会出现"旧坐标占着 IPC、松键事件排在后面"
/// 的队头阻塞——整批一次往返送达，批内顺序由发送端保证。
#[tauri::command]
pub async fn client_send_input_batch(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    events: Vec<InputArg>,
    session_id: u32,
    target: Option<crate::client::ScreenInputTarget>,
) -> Result<(), String> {
    send_input_batch_target(&window, state, events, session_id, target).await
}

async fn send_input_batch_target(
    window: &tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    events: Vec<InputArg>,
    session_id: u32,
    target: Option<crate::client::ScreenInputTarget>,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let owner = crate::remote_windows::owner(&state, window.label(), session_id)?;
    let label = owner.as_str();
    let lane = state
        .client
        .lock()
        .unwrap()
        .get(label)
        .ok_or("会话已结束")?
        .input_lane
        .clone();
    let _guard = lane.lock().await;
    if !state
        .client
        .lock()
        .unwrap()
        .get(label)
        .is_some_and(|c| std::sync::Arc::ptr_eq(&c.input_lane, &lane))
    {
        return Ok(());
    }
    let target = if let Some(target) = target {
        let Some(target) =
            crate::remote_windows::claim(window, &state, label, session_id, target).await?
        else {
            return Ok(());
        };
        Some(target)
    } else {
        if label != window.label() {
            return Ok(());
        }
        None
    };
    // 锁内只取 Sender 克隆，避免 MutexGuard 跨 await
    let tx = state
        .client
        .lock()
        .unwrap()
        .get(label)
        .filter(|connections| std::sync::Arc::ptr_eq(&connections.input_lane, &lane))
        .and_then(|connections| connections.handle.as_ref())
        .filter(|h| {
            h.session_id() == session_id
                && h.control_available()
                && h.screen_input_enabled
                    .load(std::sync::atomic::Ordering::Acquire)
        })
        .map(|h| (h.control_tx.clone(), h.screens.clone()));
    let Some((tx, screens)) = tx else {
        return Ok(());
    };
    if crate::client::send_screen_input_events(&tx, events, &screens, target)
        .await
        .is_err()
    {
        let mut clients = state.client.lock().unwrap();
        let Some(connections) = clients.get_mut(label) else {
            return Ok(());
        };
        if let Some(handle) = connections.handle.as_ref() {
            if handle.control_tx.same_channel(&tx) {
                handle.end("输入通道拥塞或已断开");
                connections.cancel();
            }
        }
        return Err("输入通道发送失败，会话已结束".into());
    }
    Ok(())
}

#[tauri::command]
pub fn client_request_keyframe(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    session_id: Option<u32>,
    stream_id: Option<u32>,
    generation: Option<u32>,
) -> Result<(), String> {
    let owner = match session_id {
        Some(id) => crate::remote_windows::owner(&state, window.label(), id)?,
        None => window.label().to_owned(),
    };
    let guard = state.client.lock().unwrap();
    if let Some(h) = guard
        .get(&owner)
        .and_then(|connections| connections.handle.as_ref())
    {
        if session_id.is_some_and(|id| id != h.session_id()) {
            return Ok(());
        }
        let message = if let (Some(id), Some(generation)) = (stream_id, generation) {
            let views = h.bridge.views.lock().unwrap();
            if views.get(&id).is_some_and(|v| v.label != window.label())
                || (window.label() != owner
                    && !views.get(&id).is_some_and(|v| v.label == window.label()))
            {
                return Ok(());
            }
            drop(views);
            vnt_proto::screen::wrap(
                id,
                generation,
                vnt_proto::control::ControlMessage::RequestKeyframe,
            )
        } else {
            vnt_proto::control::ControlMessage::RequestKeyframe
        };
        let _ = h.control_tx.try_send(message);
    }
    Ok(())
}

#[tauri::command]
pub async fn client_screen_command(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    session_id: u32,
    stream_id: u32,
    revision: u64,
    close: bool,
) -> Result<(), String> {
    let lane = state
        .client
        .lock()
        .unwrap()
        .get(window.label())
        .ok_or("子窗口不能切换屏幕")?
        .input_lane
        .clone();
    let _guard = lane.lock().await;
    let mut clients = state.client.lock().unwrap();
    let connection = clients
        .get_mut(window.label())
        .filter(|c| std::sync::Arc::ptr_eq(&c.input_lane, &lane))
        .ok_or("子窗口不能切换屏幕")?;
    let handle = connection
        .handle
        .as_ref()
        .filter(|h| h.session_id() == session_id)
        .ok_or("会话已变化")?;
    if !handle.control_available() {
        return Err("控制连接恢复中，请稍后重试".into());
    }
    let screens = handle.screens.lock().unwrap();
    if !screens.screens.iter().any(|s| s.stream_id == stream_id) {
        return Err("屏幕已关闭".into());
    }
    let views = handle.bridge.views.lock().unwrap();
    if views.contains_key(&stream_id) {
        return Err("该屏幕已拆分或正在关闭".into());
    }
    if close
        && screens
            .screens
            .iter()
            .filter(|s| !views.contains_key(&s.stream_id))
            .count()
            <= 1
    {
        return Err("主窗口至少保留一个屏幕".into());
    }
    drop(views);
    drop(screens);
    #[cfg(any(windows, target_os = "macos"))]
    connection.keyboard.take();
    connection.keyboard_owner = None;
    handle.reset_screen_input()?;
    handle
        .screen_input_enabled
        .store(false, std::sync::atomic::Ordering::Release);
    if let Err(error) =
        handle
            .control_tx
            .try_send(vnt_proto::control::ControlMessage::ScreenCommand(
                vnt_proto::pb::ScreenCommand {
                    stream_id,
                    revision,
                    action: if close { 1 } else { 0 },
                },
            ))
    {
        handle
            .screen_input_enabled
            .store(true, std::sync::atomic::Ordering::Release);
        return Err(error.to_string());
    }
    Ok(())
}

/// Credentials remain in memory, never in the window URL.
/// 返回窗口 label，前端用它把「远程中」状态与设备关联。
#[tauri::command]
pub async fn open_control_window(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    window: tauri::WebviewWindow,
    mut params: ConnectParams,
) -> Result<String, String> {
    params.validate()?;
    let size = window
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| (m.size().width, m.size().height));
    let notice = resolve_screen_target(&mut params, size);
    let id = state
        .next_window
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let label = format!("remote-{id}");
    if let Some(notice) = notice {
        state
            .resolution_notices
            .lock()
            .unwrap()
            .insert(label.clone(), notice);
    }
    state
        .window_params
        .lock()
        .unwrap()
        .insert(label.clone(), params.clone());
    state
        .client
        .lock()
        .unwrap()
        .insert(label.clone(), Default::default());
    let result = tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App("index.html?remote=1".into()),
    )
    .title(format!("{} · VNT Remote", params.host))
    .inner_size(1280.0, 800.0)
    .min_inner_size(800.0, 520.0)
    // 自绘标题栏：连接信息合并进标题栏，与主窗口保持一致
    .decorations(false)
    .build();
    if let Err(error) = result {
        state.resolution_notices.lock().unwrap().remove(&label);
        state.window_params.lock().unwrap().remove(&label);
        if let Some(mut connection) = state.client.lock().unwrap().remove(&label) {
            connection.cancel();
        }
        return Err(format!("创建控制窗口失败: {error}"));
    }
    Ok(label)
}

#[tauri::command]
pub fn control_window_params(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<ConnectParams, String> {
    state
        .window_params
        .lock()
        .unwrap()
        .get(window.label())
        .cloned()
        .ok_or("缺少连接参数".into())
}

#[tauri::command]
pub fn client_send_sas(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    session_id: u32,
) -> Result<(), String> {
    let owner = crate::remote_windows::owner(&state, window.label(), session_id)?;
    let clients = state.client.lock().unwrap();
    let h = clients
        .get(&owner)
        .and_then(|c| c.handle.as_ref())
        .filter(|h| h.session_id() == session_id)
        .ok_or("远程连接已关闭")?;
    h.send_sas()
}

/// The overlay owns its Win32 message loop on a dedicated native UI thread.
#[tauri::command]
pub async fn pick_screen_region(
    window: tauri::WebviewWindow,
) -> Result<Option<vnt_capture::ScreenRegion>, String> {
    static PICKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if PICKING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return Err("正在选择屏幕区域".into());
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PICKING.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let reset = Reset;
    window.hide().map_err(|e| e.to_string())?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let restore = window.clone();
    let spawn = std::thread::Builder::new()
        .name("region-picker".into())
        .spawn(move || {
            let _reset = reset;
            // Let the desktop compositor remove the main window before taking the snapshot.
            std::thread::sleep(std::time::Duration::from_millis(150));
            let result = std::panic::catch_unwind(crate::region_picker::pick)
                .unwrap_or_else(|_| Err("屏幕区域选择器异常退出".into()));
            let _ = restore.show();
            let _ = restore.set_focus();
            let _ = tx.send(result);
        });
    if let Err(e) = spawn {
        PICKING.store(false, std::sync::atomic::Ordering::Release);
        let _ = window.show();
        let _ = window.set_focus();
        return Err(e.to_string());
    }
    rx.await.map_err(|e| e.to_string())?
}

/// Physical pixels of the monitor containing the calling configuration window.
#[tauri::command]
pub fn current_screen_size(window: tauri::WebviewWindow) -> Result<Option<ScreenSize>, String> {
    window
        .current_monitor()
        .map(|monitor| {
            monitor.map(|m| ScreenSize {
                width: m.size().width,
                height: m.size().height,
            })
        })
        .map_err(|e| e.to_string())
}
#[derive(serde::Serialize)]
pub struct ScreenSize {
    width: u32,
    height: u32,
}

fn resolve_screen_target(params: &mut ConnectParams, screen: Option<(u32, u32)>) -> Option<String> {
    if params.resolution_mode == "none" {
        params.target_width = 0;
        params.target_height = 0;
        return None;
    }
    match screen {
        Some((width, height)) => {
            if params.target_width < 2
                || params.target_height < 2
                || params.target_width > width
                || params.target_height > height
            {
                params.target_width = width;
                params.target_height = height;
            }
            None
        }
        None => {
            params.resolution_mode = "none".into();
            params.target_width = 0;
            params.target_height = 0;
            Some("无法获取主控屏幕尺寸，按原分辨率连接".into())
        }
    }
}

#[cfg(test)]
mod screen_target_tests {
    use super::*;
    fn params(mode: &str, width: u32, height: u32) -> ConnectParams {
        serde_json::from_value(serde_json::json!({
            "colorDepth":24, "showWallpaper":true, "dragFullWindows":false,
            "fecEnabled":false, "resolutionMode":mode, "targetWidth":width,
            "targetHeight":height, "transport":"tcp", "host":"127.0.0.1",
            "port":29875, "password":"pw", "codecs":["h264"], "hardwareCodecs":[],
            "fps":30, "bitrateKbps":16000, "clipboardEnabled":true, "audioEnabled":false, "audioDeviceId":null
        }))
        .unwrap()
    }
    #[test]
    fn resolves_targets_from_configuration_monitor() {
        for mode in ["scale", "change"] {
            for (input, expected) in [
                ((0, 0), (2560, 1440)),
                ((1280, 720), (1280, 720)),
                ((3840, 2160), (2560, 1440)),
            ] {
                let mut p = params(mode, input.0, input.1);
                assert!(resolve_screen_target(&mut p, Some((2560, 1440))).is_none());
                assert_eq!((p.target_width, p.target_height), expected);
                assert_eq!(p.resolution_mode, mode);
            }
        }
        let mut p = params("none", 1280, 720);
        assert!(resolve_screen_target(&mut p, Some((2560, 1440))).is_none());
        assert_eq!((p.target_width, p.target_height), (0, 0));
    }
    #[test]
    fn unavailable_monitor_uses_original_resolution_with_notice() {
        let mut p = params("change", 1280, 720);
        assert!(resolve_screen_target(&mut p, None)
            .unwrap()
            .contains("无法获取主控屏幕尺寸"));
        assert_eq!(p.resolution_mode, "none");
        assert_eq!((p.target_width, p.target_height), (0, 0));
    }
}
