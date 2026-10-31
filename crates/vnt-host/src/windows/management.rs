use super::{native, store};
use serde::{Deserialize, Serialize};
use std::{mem::size_of, time::Duration};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::*,
        System::{Services::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
    },
};
pub const SERVICE_NAME: &str = "VntRemoteService";
pub const STATUS_PIPE: &str = r"\\.\pipe\VntRemoteService.Status.v1";
pub use crate::status::Status;
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "camelCase")]
pub enum Request {
    Start,
    LaunchDesktop(u32),
    Stop,
    AutoStart(bool),
}
struct Sc(SC_HANDLE);
impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}
fn open(access: u32) -> anyhow::Result<Sc> {
    unsafe {
        let mgr = Sc(OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT,
        )?);
        Ok(Sc(OpenServiceW(
            mgr.0,
            PCWSTR(native::wide(SERVICE_NAME).as_ptr()),
            access,
        )?))
    }
}
pub fn service_executable() -> anyhow::Result<Option<std::path::PathBuf>> {
    unsafe {
        let svc = match open(SERVICE_QUERY_CONFIG) {
            Ok(svc) => svc,
            Err(error)
                if error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|error| {
                        error.code()
                            == windows::core::HRESULT::from_win32(ERROR_SERVICE_DOES_NOT_EXIST.0)
                    }) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        let mut size = 0;
        let _ = QueryServiceConfigW(svc.0, None, 0, &mut size);
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        QueryServiceConfigW(svc.0, Some(buffer.as_mut_ptr() as *mut _), size, &mut size)?;
        let config = &*(buffer.as_ptr() as *const QUERY_SERVICE_CONFIGW);
        let command = config.lpBinaryPathName.to_string()?;
        Ok(Some(executable_from_command(&command)?))
    }
}
fn executable_from_command(command: &str) -> anyhow::Result<std::path::PathBuf> {
    let command = command.trim();
    let path = if let Some(quoted) = command.strip_prefix('"') {
        quoted
            .split_once('"')
            .map(|(path, _)| path)
            .ok_or_else(|| anyhow::anyhow!("服务程序路径引号不完整"))?
    } else {
        command
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow::anyhow!("服务程序路径为空"))?
    };
    let path = std::path::PathBuf::from(path);
    anyhow::ensure!(path.is_absolute(), "服务程序路径必须为绝对路径");
    Ok(path)
}
pub fn service_pid() -> anyhow::Result<u32> {
    unsafe {
        let svc = open(SERVICE_QUERY_STATUS)?;
        let mut raw = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        QueryServiceStatusEx(
            svc.0,
            SC_STATUS_PROCESS_INFO,
            Some(std::slice::from_raw_parts_mut(
                &mut raw as *mut _ as *mut u8,
                size_of::<SERVICE_STATUS_PROCESS>(),
            )),
            &mut needed,
        )?;
        Ok(raw.dwProcessId)
    }
}
/// SCM metadata only: the GUI calls this before its authenticated TCP session
/// exists and must never open or decrypt the service configuration here.
pub fn scm_status() -> anyhow::Result<Status> {
    unsafe {
        let svc = match open(SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG) {
            Ok(s) => s,
            Err(e) => {
                if e.downcast_ref::<windows::core::Error>().is_some_and(|e| {
                    e.code() == windows::core::HRESULT::from_win32(ERROR_SERVICE_DOES_NOT_EXIST.0)
                }) {
                    return Ok(Status {
                        state: "notInstalled".into(),
                        ..Default::default()
                    });
                }
                return Err(e);
            }
        };
        let mut raw = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        QueryServiceStatusEx(
            svc.0,
            SC_STATUS_PROCESS_INFO,
            Some(std::slice::from_raw_parts_mut(
                &mut raw as *mut _ as *mut u8,
                size_of::<SERVICE_STATUS_PROCESS>(),
            )),
            &mut needed,
        )?;
        let mut size = 0;
        let _ = QueryServiceConfigW(svc.0, None, 0, &mut size);
        let mut buf = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        QueryServiceConfigW(svc.0, Some(buf.as_mut_ptr() as *mut _), size, &mut size)?;
        let cfg = &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW);
        let state = match raw.dwCurrentState {
            SERVICE_STOPPED => "stopped",
            SERVICE_START_PENDING => "starting",
            SERVICE_STOP_PENDING => "stopping",
            SERVICE_RUNNING => "running",
            _ => "fault",
        };
        let operation_error = store::service_error()
            .unwrap_or_else(|error| Some(format!("读取服务故障记录失败：{error:#}")));
        let error = status_error(raw.dwWin32ExitCode, operation_error, None);
        Ok(Status {
            installed: true,
            state: state.into(),
            service_state: state.into(),
            auto_start: cfg.dwStartType == SERVICE_AUTO_START,
            error,
            ..Default::default()
        })
    }
}
fn status_error(
    exit_code: u32,
    operation_error: Option<String>,
    config_error: Option<String>,
) -> Option<String> {
    operation_error
        .or(config_error)
        .or_else(|| (exit_code != 0).then(|| format!("服务退出错误：{exit_code}")))
}
pub async fn status() -> anyhow::Result<Status> {
    let base = scm_status()?;
    if base.state != "running" {
        return Ok(base);
    }
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut p = native::connect(STATUS_PIPE).await?;
        anyhow::ensure!(
            native::server_pid(&p)? == service_pid()?,
            "状态管道不属于被控服务"
        );
        let raw = read_json::<_, Status>(&mut p).await?;
        Ok::<_, anyhow::Error>(raw)
    })
    .await;
    match result {
        Ok(Ok(mut s)) => {
            s.auto_start = base.auto_start;
            s.error = base.error.or(s.error);
            Ok(s)
        }
        other => Ok(Status {
            error: Some(format!("服务状态管道不可用：{other:?}")),
            state: "fault".into(),
            ..base
        }),
    }
}
// Bounded JSON is reserved for management and agent bootstrap, never media.
pub async fn write_json<S: tokio::io::AsyncWrite + Unpin, T: Serialize>(
    s: &mut S,
    v: &T,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let data = serde_json::to_vec(v)?;
    anyhow::ensure!(data.len() <= 65536, "本机消息过长");
    s.write_u32_le(data.len() as u32).await?;
    s.write_all(&data).await?;
    Ok(())
}
pub async fn read_json<S: tokio::io::AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    s: &mut S,
) -> anyhow::Result<T> {
    use tokio::io::AsyncReadExt;
    let n = s.read_u32_le().await? as usize;
    anyhow::ensure!(n <= 65536, "本机消息过长");
    let mut data = vec![0; n];
    s.read_exact(&mut data).await?;
    Ok(serde_json::from_slice(&data)?)
}
pub fn start() -> anyhow::Result<()> {
    start_for_launcher(None)
}
fn start_for_launcher(launcher: Option<u32>) -> anyhow::Result<()> {
    unsafe {
        let svc = open(SERVICE_START | SERVICE_QUERY_STATUS)?;
        let state = scm_status()?.state;
        if state == "stopping" {
            wait_state("stopped")?;
        }
        if !matches!(state.as_str(), "running" | "starting") {
            let arg = launcher.map(|pid| native::wide(pid.to_string()));
            let args = arg.as_ref().map(|arg| [PCWSTR(arg.as_ptr())]);
            if let Err(error) = StartServiceW(svc.0, args.as_ref().map(|args| args.as_slice())) {
                anyhow::bail!(
                    "{}",
                    store::service_error()?.unwrap_or_else(|| format!("启动服务失败：{error}"))
                );
            }
        }
    }
    wait_state("running")
}
pub fn stop() -> anyhow::Result<()> {
    unsafe {
        let svc = open(SERVICE_STOP | SERVICE_QUERY_STATUS)?;
        let state = scm_status()?.state;
        if state == "stopped" {
            return Ok(());
        }
        if state == "starting" {
            wait_state("running")?;
        }
        if state != "stopping" {
            let mut status = SERVICE_STATUS::default();
            ControlService(svc.0, SERVICE_CONTROL_STOP, &mut status)?;
        }
    }
    wait_state("stopped")
}
fn wait_state(target: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = scm_status()?;
        if s.state == target {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "等待服务{target}超时{}",
            s.error
                .as_ref()
                .map(|error| format!("：{error}"))
                .unwrap_or_default()
        );
        if target == "running" && s.state == "stopped" {
            anyhow::bail!(
                "服务启动失败：{}",
                s.error.as_deref().unwrap_or("服务未能进入运行状态")
            )
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
pub(crate) fn startup(enabled: bool) -> anyhow::Result<()> {
    unsafe {
        let s = open(SERVICE_CHANGE_CONFIG)?;
        ChangeServiceConfigW(
            s.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            if enabled {
                SERVICE_AUTO_START
            } else {
                SERVICE_DEMAND_START
            },
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )?;
    }
    Ok(())
}
pub fn apply(request: Request) -> anyhow::Result<()> {
    let _lock = native::management_lock()?;
    store::save_service_error(None)?;
    let result = match request {
        Request::Start => start(),
        Request::LaunchDesktop(pid) => restart_for_launcher(pid, stop, start_for_launcher),
        Request::Stop => stop(),
        Request::AutoStart(on) => startup(on),
    };
    if let Err(error) = &result {
        let _ = store::save_service_error(Some(&format!("{error:#}")));
    }
    result
}

fn restart_for_launcher(
    pid: u32,
    stop_service: impl FnOnce() -> anyhow::Result<()>,
    start_service: impl FnOnce(Option<u32>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // stop() waits for SCM Stopped, including managed desktop/agent cleanup.
    // Pass the launcher to the new service so it waits for bootstrap exit.
    stop_service()?;
    start_service(Some(pid))
}

/// The UAC helper accepts only a fixed request from this installed GUI process.
fn launch_helper() -> anyhow::Result<(
    tokio::net::windows::named_pipe::NamedPipeServer,
    native::Handle,
    u32,
)> {
    let exe = std::env::current_exe()?.with_file_name("vnt-service.exe");
    anyhow::ensure!(exe.exists(), "未找到系统服务程序，请重新运行安装包");
    let name = format!(r"\\.\pipe\VntRemote.Manage.{}", native::random()?);
    let current = unsafe { GetCurrentProcessId() };
    let sid = {
        let mut t = HANDLE::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                windows::Win32::Security::TOKEN_QUERY,
                &mut t,
            )?;
        }
        let token = native::Handle(t);
        native::user_sid(token.0)?
    };
    let pipe = native::pipe(
        &name,
        &format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"),
        true,
    )?;
    let file = native::wide(&exe);
    let args = native::wide(format!("--elevated {} {}", name, current));
    let verb = native::wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| anyhow::anyhow!("管理员授权未完成：{e}"))?;
    }
    let process = native::Handle(info.hProcess);
    let pid = unsafe { GetProcessId(process.0) };
    Ok((pipe, process, pid))
}
pub async fn elevated(request: Request) -> anyhow::Result<()> {
    let (mut pipe, _process, pid) = launch_helper()?;
    tokio::time::timeout(Duration::from_secs(60), async {
        pipe.connect().await?;
        anyhow::ensure!(native::client_pid(&pipe)? == pid, "管理助手身份不匹配");
        write_json(&mut pipe, &request).await?;
        let reply: Result<(), String> = read_json(&mut pipe).await?;
        reply.map_err(anyhow::Error::msg)
    })
    .await??;
    Ok(())
}
pub async fn helper(name: &str, parent: u32) -> anyhow::Result<()> {
    let mut p = native::connect(name).await?;
    anyhow::ensure!(native::server_pid(&p)? == parent, "管理请求进程不匹配");
    let expected = std::env::current_exe()?.with_file_name("vnt-remote.exe");
    anyhow::ensure!(
        native::process_path(parent)?.canonicalize()? == expected.canonicalize()?,
        "管理请求必须来自安装目录中的主程序"
    );
    let req: Request = read_json(&mut p).await?;
    if let Request::LaunchDesktop(pid) = &req {
        anyhow::ensure!(*pid == parent, "桌面交接进程必须是管理请求进程");
    }
    let result = tokio::task::spawn_blocking(|| apply(req))
        .await?
        .map_err(|e| format!("{e:#}"));
    write_json(&mut p, &result).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_launch_stops_before_starting_and_preserves_launcher() {
        let calls = std::cell::RefCell::new(Vec::new());
        restart_for_launcher(
            123,
            || {
                calls.borrow_mut().push("stopped");
                Ok(())
            },
            |launcher| {
                assert_eq!(*calls.borrow(), ["stopped"]);
                assert_eq!(launcher, Some(123));
                calls.borrow_mut().push("started");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["stopped", "started"]);
    }

    #[test]
    fn desktop_launch_does_not_start_when_stop_fails() {
        let error = restart_for_launcher(
            123,
            || anyhow::bail!("停止服务失败"),
            |_| panic!("must not start before the old service stops"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "停止服务失败");
    }

    #[test]
    fn concrete_start_and_stop_failures_survive_when_the_service_is_not_running() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service-state.json");
        for reason in [
            "无法启动被控服务：TCP 端口 29875，端口已被占用",
            "停止服务失败：拒绝访问",
        ] {
            std::fs::write(&path, serde_json::to_vec(&Some(reason)).unwrap()).unwrap();
            let persisted = store::service_error_from(&path).unwrap();
            assert_eq!(
                status_error(ERROR_SERVICE_SPECIFIC_ERROR.0, persisted, None).as_deref(),
                Some(reason)
            );
        }
        std::fs::write(&path, b"null").unwrap();
        assert!(status_error(0, store::service_error_from(&path).unwrap(), None).is_none());
    }
    #[test]
    fn installed_service_paths_handle_spaces_and_custom_drives() {
        assert_eq!(
            executable_from_command(r#""D:\Remote Tools\vnt-service.exe" --service"#).unwrap(),
            std::path::PathBuf::from(r"D:\Remote Tools\vnt-service.exe")
        );
        assert_eq!(
            executable_from_command(r"D:\VntRemote\vnt-service.exe --service").unwrap(),
            std::path::PathBuf::from(r"D:\VntRemote\vnt-service.exe")
        );
        assert!(executable_from_command(r#""D:\unclosed"#).is_err());
        assert!(executable_from_command("vnt-service.exe --service").is_err());
    }
}

#[cfg(test)]
mod port_config_tests {
    use super::Status;

    #[test]
    fn active_host_locks_port_but_idle_windows_service_does_not() {
        for state in ["running", "starting", "stopping"] {
            let status = Status {
                state: state.into(),
                port: 29875,
                ..Default::default()
            };
            assert!(status.validate_port_change(29876).is_err());
            assert!(status.validate_port_change(29875).is_ok());
        }
        let status = Status {
            state: "stopped".into(),
            service_state: "running".into(),
            port: 29875,
            ..Default::default()
        };
        assert!(status.validate_port_change(29876).is_ok());
    }
}
