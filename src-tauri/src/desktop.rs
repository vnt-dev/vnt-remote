//! Installed desktop startup/exit. Bootstrap entry processes never create a UI
//! and never send Quit when handing ownership over to the service.
use tauri::Manager;
use vnt_host::windows::{
    local::{Action, Client, Event},
    management,
};

pub struct Desktop {
    client: tokio::sync::Mutex<Client>,
    quitting: std::sync::atomic::AtomicBool,
}
impl Desktop {
    pub async fn link_status(&self) -> Result<Vec<vnt_host::link::ProfileStatus>, String> {
        self.client
            .lock()
            .await
            .link_status()
            .await
            .map_err(|e| format!("{e:#}"))
    }
    pub async fn connect_overlay(
        &self,
        target: vnt_host::link::LinkTarget,
    ) -> Result<vnt_net::overlay::Prepared, String> {
        self.client
            .lock()
            .await
            .connect_overlay(target)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    pub async fn read_config(&self) -> Result<crate::config::AppConfig, String> {
        self.client
            .lock()
            .await
            .read_config()
            .await
            .map_err(|e| format!("{e:#}"))
    }
    pub async fn request(&self, action: Action) -> Result<crate::ServerStatus, String> {
        self.client
            .lock()
            .await
            .request(action)
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

pub fn prepare() -> anyhow::Result<Option<Client>> {
    let port = vnt_host::windows::local::parse_port(&std::env::args().skip(1).collect::<Vec<_>>())?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            if let Some(port) = port {
                let client = Client::connect(Some(port)).await?;
                client.request(Action::Register).await?;
                return Ok(Some(client));
            }
            let status = management::scm_status()?;
            anyhow::ensure!(status.installed, "请先使用安装包安装 Windows 服务");
            // The instance guard already restored any existing desktop. A fresh
            // launch resets a surviving service before handing UI ownership back.
            management::elevated(management::Request::LaunchDesktop(std::process::id())).await?;
            let client = Client::connect(None).await?;
            client.request(Action::ShowDesktop).await?;
            Ok(None)
        })
}

pub fn setup(app: &tauri::App, client: Client) {
    app.manage(Desktop {
        client: tokio::sync::Mutex::new(client.clone()),
        quitting: false.into(),
    });
    let app = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        let mut client = client;
        loop {
            match client.events().await {
                Ok(mut events) => loop {
                    match Client::next_event(&mut events).await {
                        Ok(Event::Show) => crate::tray::show_main_window(&app),
                        Ok(Event::Close) => {
                            if !app
                                .state::<Desktop>()
                                .quitting
                                .load(std::sync::atomic::Ordering::SeqCst)
                            {
                                app.exit(0);
                            }
                            return;
                        }
                        Err(error) => {
                            tracing::warn!("服务通信中断：{error:#}");
                            break;
                        }
                    }
                },
                Err(error) => tracing::warn!("连接服务通知失败：{error:#}"),
            }
            if management::scm_status().is_ok_and(|s| s.state == "stopped") {
                app.exit(0);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Ok(fresh) = Client::connect(None).await {
                *app.state::<Desktop>().client.lock().await = fresh.clone();
                client = fresh;
            }
        }
    });
}

pub fn quit(app: &tauri::AppHandle) {
    let app = app.clone();
    let state = app.state::<Desktop>();
    if state
        .quitting
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let result = app.state::<Desktop>().request(Action::Quit).await;
        match result {
            Ok(_) => app.exit(0),
            Err(error) => {
                app.state::<Desktop>()
                    .quitting
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                show_error(&format!("退出失败：{error}"));
            }
        }
    });
}

pub fn show_error(error: &str) {
    use windows::{
        core::PCWSTR,
        Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
    };
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(vnt_host::windows::native::wide(error).as_ptr()),
            PCWSTR(vnt_host::windows::native::wide("VNT Remote").as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
