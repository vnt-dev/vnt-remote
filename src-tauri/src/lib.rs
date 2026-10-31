// vnt-remote Tauri 应用（被控端 + 控制端）
//!
//! 模块：
//! - `config`:  密码/端口等持久化
//! - `server`:  被控端（抓屏→编码→UDP 推流，输入注入）
//! - `client`:  控制端（协商→UDP 收流→回环 WS 桥→前端 WebCodecs）
//! - `bridge`:  回环 WebSocket（视频帧进 webview 的通道）
//! - `clipboard`: 剪贴板同步（文本 + 文件复制粘贴）
//! - `commands`:前端 invoke 入口

#[cfg(all(feature = "installed", not(windows)))]
compile_error!("installed edition is supported only on Windows");
pub mod bridge;
pub mod client;
pub mod clipboard;
pub mod commands;
pub mod config;
pub mod connections;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod keyboard;
#[cfg(target_os = "macos")]
#[path = "keyboard_macos.rs"]
mod keyboard;
mod logging;
mod platform;
#[cfg(windows)]
mod region_picker;
#[cfg(target_os = "macos")]
#[path = "region_picker_macos.rs"]
mod region_picker;
mod remote_windows;
pub mod server;
mod tray;
mod visibility;

use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

pub type ServerStatus = vnt_host::status::Status;

pub struct AppState {
    pub link: Arc<vnt_host::link::ServerLinks>,
    pub store: config::ConfigStore,
    pub host: AsyncMutex<Option<server::ServerHandle>>,
    pub host_clipboard: Arc<clipboard::ClipboardService>,
    pub config: std::sync::Mutex<config::AppConfig>,
    pub config_update: AsyncMutex<()>,
    pub next_window: std::sync::atomic::AtomicU64,
    pub resolution_notices: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub window_params: std::sync::Mutex<std::collections::HashMap<String, client::ConnectParams>>,
    pub child_windows:
        std::sync::Mutex<std::collections::HashMap<String, remote_windows::ChildWindow>>,
    pub client: std::sync::Mutex<std::collections::HashMap<String, connections::Connections>>,
    /// 本进程主控连接的剪贴板服务；被控剪贴板由用户会话代理持有。
    pub clipboard: Arc<clipboard::ClipboardService>,
}

/// 事件出口：核心逻辑（server/client）通过它通知前端，
/// 与 Tauri 解耦，测试可用空实现。
pub use vnt_host::EventSink;

/// 基于 Tauri 事件的实现（真实应用）。
pub struct TauriEventSink(pub tauri::AppHandle);

impl EventSink for TauriEventSink {
    fn emit_json(&self, event: &str, payload_json: String) {
        use tauri::Emitter;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload_json) {
            let _ = self.0.emit(event, value);
        }
    }
}

pub struct WindowEventSink(
    pub tauri::AppHandle,
    pub String,
    pub u64,
    pub std::sync::Arc<std::sync::atomic::AtomicU64>,
);
impl EventSink for WindowEventSink {
    fn emit_json(&self, event: &str, payload_json: String) {
        if self.3.load(std::sync::atomic::Ordering::Acquire) != self.2 {
            return;
        }
        use tauri::Emitter;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload_json) {
            use tauri::Manager;
            let children: Vec<_> = self
                .0
                .state::<AppState>()
                .child_windows
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, child)| {
                    child.owner == self.1
                        && child.generation == self.2
                        && value["sessionId"]
                            .as_u64()
                            .is_none_or(|id| id == child.session_id as u64)
                })
                .map(|(label, _)| label.clone())
                .collect();
            let _ = self.0.emit_to(self.1.as_str(), event, value.clone());
            for label in children {
                let _ = self.0.emit_to(label.as_str(), event, value.clone());
                if event == "client://closed" {
                    if let Some(window) = self.0.get_webview_window(&label) {
                        // Never enter lifecycle cleanup while the connection registry is locked.
                        let _ = self.0.run_on_main_thread(move || {
                            let _ = window.close();
                        });
                    }
                }
            }
        }
    }
}

/// 空实现（测试）。
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit_json(&self, _event: &str, _payload_json: String) {}
}

impl AppState {
    pub fn close_control_window(&self, label: &str) {
        self.resolution_notices.lock().unwrap().remove(label);
        self.window_params.lock().unwrap().remove(label);
        // Drop outside the registry lock: disconnect emits an event to Tauri.
        let connection = self.client.lock().unwrap().remove(label);
        drop(connection);
    }

    pub async fn shutdown_all(&self) {
        self.link.stop().await;
        self.child_windows.lock().unwrap().clear();
        if let Some(host) = self.host.lock().await.take() {
            host.stop().await;
        }
        self.host_clipboard.set_enabled(false);
        let clients: Vec<_> = self
            .client
            .lock()
            .unwrap()
            .drain()
            .map(|(_, c)| c)
            .collect();
        drop(clients);
        self.resolution_notices.lock().unwrap().clear();
        self.window_params.lock().unwrap().clear();
    }
}

pub fn run() {
    use tauri::Manager;
    #[cfg(windows)]
    let instance = match instance::Instance::acquire(config::INSTALLED) {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(error) => {
            desktop::show_error(&format!("检查程序运行状态失败：{error:#}"));
            return;
        }
    };
    #[cfg(windows)]
    let desktop_client = if config::INSTALLED {
        match desktop::prepare() {
            Ok(Some(client)) => Some(client),
            Ok(None) => return,
            Err(error) => {
                desktop::show_error(&format!("启动失败：{error:#}"));
                return;
            }
        }
    } else {
        None
    };
    let builder = tauri::Builder::default();
    let builder = if config::INSTALLED {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };
    // Tao's default Raw Input registration can bypass our WH_KEYBOARD_LL
    // callback while this process has focus. Filter device events entirely
    // (Always means always filter): on Windows this removes that registration.
    // Pointer/DOM input still uses normal window messages.
    // https://github.com/tauri-apps/wry/issues/1761
    #[cfg(windows)]
    let builder = builder.device_event_filter(tauri::DeviceEventFilter::Always);
    builder
        .setup(move |app| {
            logging::init(&app.path().app_data_dir()?)?;
            let store = config::ConfigStore::new(&app.path().app_data_dir()?);
            #[cfg(windows)]
            let loaded = if let Some(client) = &desktop_client {
                tauri::async_runtime::block_on(client.read_config()).map_err(|e| format!("{e:#}"))
            } else {
                store.load()
            };
            #[cfg(target_os = "macos")]
            let loaded = store.load();
            let cfg = loaded.unwrap_or_else(|error| {
                tracing::warn!(%error, "读取被控配置失败");
                config::AppConfig::default()
            });
            tracing::info!("配置已加载，端口 {}", cfg.port);
            let clipboard = clipboard::ClipboardService::shared();
            // Host clipboard policy belongs to the Windows user agent. The GUI
            // is a controller; each remote window applies its own sync preference.
            // A disabled local host must not disable copying to another machine.
            let portable_server = cfg.servers.clone();
            let links = if config::INSTALLED {
                vnt_host::link::ServerLinks::default()
            } else {
                vnt_host::link::ServerLinks::new(
                    &app.path().app_data_dir()?.join("portable/device_id"),
                )?
            };
            app.manage(AppState {
                link: Arc::new(links),
                store,
                host: AsyncMutex::new(None),
                host_clipboard: if config::INSTALLED {
                    clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}))
                } else {
                    clipboard::ClipboardService::shared()
                },
                config: std::sync::Mutex::new(cfg),
                config_update: AsyncMutex::new(()),
                next_window: std::sync::atomic::AtomicU64::new(1),
                resolution_notices: Default::default(),
                window_params: Default::default(),
                client: Default::default(),
                child_windows: Default::default(),
                clipboard,
            });
            if !config::INSTALLED {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let link = app_handle.state::<AppState>().link.clone();
                    if let Err(error) = link.configure(portable_server).await {
                        tracing::warn!("恢复服务端连接失败：{error}");
                    }
                    loop {
                        if let Some((mut channel, peer, media)) = link.next_incoming().await {
                            let state = app_handle.state::<AppState>();
                            let host = state.host.lock().await;
                            if let Some(host) = host.as_ref() {
                                host.accept_overlay(channel, peer, media);
                            } else {
                                let _ = channel
                                    .send(&vnt_proto::control::ControlMessage::Denied(
                                        vnt_proto::pb::Denied {
                                            reason: "目标设备未开启被控".into(),
                                        },
                                    ))
                                    .await;
                            }
                        }
                    }
                });
            }
            tray::setup(app)?;
            visibility::start(app.handle().clone());
            #[cfg(windows)]
            {
                instance.listen(app.handle().clone());
                app.manage(instance);
            }
            #[cfg(windows)]
            if let Some(client) = desktop_client {
                desktop::setup(app, client);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if let Err(error) = window.hide() {
                        tracing::warn!(%error, "无法将主窗口隐藏到托盘");
                    }
                    return;
                }
            }
            if window.label() != "main"
                && matches!(
                    event,
                    tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
                )
            {
                remote_windows::close_window(window.app_handle(), window.label());
                return;
            }
            if let tauri::WindowEvent::Destroyed = event {
                let app = window.app_handle();
                let state = app.state::<AppState>();
                if window.label() == "main" {
                    tauri::async_runtime::block_on(state.shutdown_all());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_edition,
            commands::computer_name,
            platform::platform_info,
            platform::request_platform_permission,
            commands::desktop_update_handoff,
            commands::open_release_page,
            commands::get_config,
            commands::set_config,
            commands::server_link_apply,
            commands::server_link_remove,
            commands::server_link_status,
            commands::server_start,
            commands::server_stop,
            commands::server_status,
            commands::server_set_auto_start,
            commands::client_send_sas,
            commands::list_monitors,
            commands::current_screen_size,
            commands::pick_screen_region,
            commands::list_audio_playback_devices,
            commands::local_ip,
            commands::open_control_window,
            commands::control_window_params,
            remote_windows::detach_screen,
            remote_windows::screen_window_context,
            commands::client_connect,
            commands::client_disconnect,
            commands::client_send_input,
            commands::client_send_input_batch,
            commands::client_set_keyboard_capture,
            commands::client_request_keyframe,
            commands::client_screen_command,
            visibility::client_view_screen,
            commands::clipboard_cancel_transfer,
            commands::client_set_clipboard_enabled,
            commands::client_clipboard_enabled,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                tray::show_main_window(app);
            }
            if let tauri::RunEvent::Exit = event {
                tauri::async_runtime::block_on(app.state::<AppState>().shutdown_all());
            }
        });
}
