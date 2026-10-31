use super::{
    management::{self, write_json, SERVICE_NAME},
    native, store,
};
use std::{ffi::c_void, sync::OnceLock, time::Duration};
use tokio_util::sync::CancellationToken;
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Security::Authentication::Identity::SendSAS,
        System::{Registry::*, Services::*},
    },
};
static STOP: OnceLock<CancellationToken> = OnceLock::new();
unsafe extern "system" fn handler(control: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    if matches!(control, SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN) {
        if let Some(c) = STOP.get() {
            c.cancel();
        }
        0
    } else if control == SERVICE_CONTROL_INTERROGATE {
        0
    } else {
        ERROR_CALL_NOT_IMPLEMENTED.0
    }
}
unsafe extern "system" fn service_main(argc: u32, argv: *mut PWSTR) {
    let launcher = if argc > 1 && !argv.is_null() {
        (*argv.add(1))
            .to_string()
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    } else {
        None
    };
    let name = native::wide(SERVICE_NAME);
    let handle = match RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(handler), None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let report = |state: SERVICE_STATUS_CURRENT_STATE, code: u32| {
        let _ = SetServiceStatus(
            handle,
            &SERVICE_STATUS {
                dwServiceType: SERVICE_WIN32_OWN_PROCESS,
                dwCurrentState: state,
                dwControlsAccepted: if state == SERVICE_RUNNING {
                    SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
                } else {
                    0
                },
                dwWin32ExitCode: code,
                dwCheckPoint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING {
                    1
                } else {
                    0
                },
                dwWaitHint: if state == SERVICE_STOP_PENDING {
                    60000
                } else if state == SERVICE_START_PENDING {
                    30000
                } else {
                    0
                },
                ..Default::default()
            },
        );
    };
    report(SERVICE_START_PENDING, 0);
    let cancel = STOP.get_or_init(CancellationToken::new).clone();
    let result = (|| -> anyhow::Result<()> {
        store::save_service_error(None)?;
        let logs = store::secure_log_directory()?;
        crate::logging::init_file(&logs.join("vnt-remote.log"))?;
        // Initialize configuration and enforce service-only access on every startup.
        store::initialize_config()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let mut control = super::local::start(cancel.clone(), launcher).await?;
            let state = control.status.clone();
            // Keep an unconnected instance alive while answering a snapshot, so
            // an unprivileged process cannot claim the fixed pipe between reads.
            let first = native::pipe(
                management::STATUS_PIPE,
                "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;BU)",
                true,
            )?;
            let status_cancel = cancel.clone();
            let status_task = tokio::spawn(async move {
                let mut pipe = first;
                loop {
                    tokio::select! {
                        _=status_cancel.cancelled()=>break,
                        result=pipe.connect()=>{ if result.is_err(){break;} }
                    }
                    let next = match native::pipe(
                        management::STATUS_PIPE,
                        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;BU)",
                        false,
                    ) {
                        Ok(next) => next,
                        Err(error) => {
                            tracing::error!("无法创建服务状态管道：{error}");
                            break;
                        }
                    };
                    let snapshot = state.lock().unwrap().clone();
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        write_json(&mut pipe, &snapshot),
                    )
                    .await;
                    pipe = next;
                }
            });
            report(SERVICE_RUNNING, 0);
            tokio::select! {
                _ = cancel.cancelled() => {},
                result = control.wait() => {
                    status_task.abort();
                    return result;
                }
            }
            report(SERVICE_STOP_PENDING, 0);
            control.shutdown().await?;
            status_task.abort();
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok::<_, anyhow::Error>(())
        })
    })();
    if let Err(e) = &result {
        let reason = format!("{e:#}");
        tracing::error!("系统服务异常退出：{reason}");
        if let Err(error) = store::save_service_error(Some(&reason)) {
            tracing::error!("保存服务故障原因失败：{error:#}");
        }
    }
    report(
        SERVICE_STOPPED,
        if result.is_ok() {
            0
        } else {
            ERROR_SERVICE_SPECIFIC_ERROR.0
        },
    );
}
pub fn dispatch() -> anyhow::Result<()> {
    let name = native::wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_ptr() as *mut _),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr())?;
    }
    Ok(())
}

const SAS_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const SAS_NAME: &str = "SoftwareSASGeneration";
fn sas_value() -> anyhow::Result<Option<u32>> {
    unsafe {
        let mut value = 0u32;
        let mut size = 4;
        let result = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(native::wide(SAS_KEY).as_ptr()),
            PCWSTR(native::wide(SAS_NAME).as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut _ as *mut c_void),
            Some(&mut size),
        );
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        result.ok()?;
        Ok(Some(value))
    }
}
fn set_sas(value: Option<u32>) -> anyhow::Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(native::wide(SAS_KEY).as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let result = if let Some(v) = value {
            RegSetValueExW(
                key,
                PCWSTR(native::wide(SAS_NAME).as_ptr()),
                None,
                REG_DWORD,
                Some(&v.to_le_bytes()),
            )
        } else {
            RegDeleteValueW(key, PCWSTR(native::wide(SAS_NAME).as_ptr()))
        };
        let _ = RegCloseKey(key);
        if result != ERROR_FILE_NOT_FOUND {
            result.ok()?;
        }
        Ok(())
    }
}
fn sas_allowed(value: Option<u32>) -> bool {
    matches!(value, Some(1 | 3))
}
/// Physical-console SAS must run as the SCM service itself. In particular, do
/// not impersonate the logged-on user (which loses the service security context)
/// or synthesize another session's SYSTEM token. Windows selects the console.
fn send_console_sas(
    target: u32,
    console: Option<u32>,
    caller_pid: u32,
    service_pid: u32,
    call: impl FnOnce(bool),
) -> anyhow::Result<()> {
    anyhow::ensure!(
        caller_pid != 0 && caller_pid == service_pid,
        "Ctrl+Alt+Del 必须由已注册的系统服务进程直接执行"
    );
    anyhow::ensure!(console == Some(target), "物理控制台会话已变化，请重试");
    call(false);
    Ok(())
}
/// A fresh OS thread retains the service's primary identity throughout this call.
/// SendSAS itself has no result; the broker observes the desktop afterwards.
pub async fn sas(session: u32) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("send-sas".into())
        .spawn(move || {
            let _ = tx.send(sas_on_thread(session));
        })?;
    tokio::time::timeout(Duration::from_secs(3), rx)
        .await
        .map_err(|_| anyhow::anyhow!("调用 Windows SendSAS 超时，执行结果未确认"))?
        .map_err(|_| anyhow::anyhow!("发送 Ctrl+Alt+Del 的线程异常退出"))?
}
fn sas_on_thread(session: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        sas_allowed(sas_value()?),
        "系统策略未允许服务发送 Ctrl+Alt+Del"
    );
    let caller_pid = std::process::id();
    native::require_system(caller_pid)?;
    let service_pid = management::service_pid()?;
    let console = native::console_session();
    tracing::info!(session_id=session, caller_pid, service_pid, console_session=?console,
        "由 SCM LocalSystem 服务直接调用 SendSAS(false)，未模拟用户身份");
    send_console_sas(
        session,
        console,
        caller_pid,
        service_pid,
        |as_user| unsafe {
            SendSAS(as_user);
        },
    )
}

fn checked(exe: &str, args: &[&str]) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let result = std::process::Command::new(super::native::system_executable(exe)?)
        .args(args)
        .creation_flags(0x08000000)
        .output()?;
    anyhow::ensure!(
        result.status.success(),
        "{exe} 失败：{}",
        String::from_utf8_lossy(&result.stdout)
    );
    Ok(())
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Upgrade {
    auto_start: bool,
    running: bool,
}
pub fn prepare_upgrade() -> anyhow::Result<()> {
    let _lock = native::management_lock()?;
    let d = store::secure_directory()?;
    let s = management::scm_status()?;
    if s.installed {
        std::fs::write(
            d.join("private/upgrade.json"),
            serde_json::to_vec(&Upgrade {
                auto_start: s.auto_start,
                running: s.state == "running",
            })?,
        )?;
        if s.state != "stopped" {
            management::stop()?;
        }
    }
    Ok(())
}
pub fn install() -> anyhow::Result<()> {
    let _lock = native::management_lock()?;
    let d = store::secure_directory()?;
    let path = std::env::current_exe()?;
    let binary = format!("\"{}\" --service", path.display());
    let existing = management::scm_status()?.installed;
    let result = (|| -> anyhow::Result<()> {
        if existing {
            checked(
                "sc.exe",
                &[
                    "config",
                    SERVICE_NAME,
                    "binPath=",
                    &binary,
                    "obj=",
                    "LocalSystem",
                ],
            )?;
        } else {
            checked(
                "sc.exe",
                &[
                    "create",
                    SERVICE_NAME,
                    "binPath=",
                    &binary,
                    "start=",
                    "demand",
                    "obj=",
                    "LocalSystem",
                    "DisplayName=",
                    "VNT Remote 被控服务",
                ],
            )?;
        }
        checked(
            "sc.exe",
            &[
                "failure",
                SERVICE_NAME,
                "reset=",
                "86400",
                "actions=",
                "restart/3000/restart/10000/restart/30000",
            ],
        )?;
        let value = sas_value()?;
        if !matches!(value, Some(1 | 3)) {
            let marker = d.join("private/sas-policy.json");
            let applied = value.unwrap_or(0) | 1;
            if !marker.exists() {
                std::fs::write(&marker, serde_json::to_vec(&(value, applied))?)?;
            }
            set_sas(Some(applied))?;
            anyhow::ensure!(sas_value()? == Some(applied), "策略不允许启用服务 SAS");
        }
        store::initialize_config()?;
        store::host_enabled()?;
        if let Ok(data) = std::fs::read(d.join("private/upgrade.json")) {
            let old: Upgrade = serde_json::from_slice(&data)?;
            management::startup(old.auto_start)?;
            if old.running {
                management::apply(management::Request::Start)?;
            }
            let _ = std::fs::remove_file(d.join("private/upgrade.json"));
        }
        Ok(())
    })();
    if result.is_err() && !existing {
        let _ = uninstall(false);
    }
    result
}
pub fn uninstall(upgrade: bool) -> anyhow::Result<()> {
    let _lock = native::management_lock()?;
    if upgrade {
        prepare_upgrade()?;
    }
    if management::scm_status()?.installed {
        management::stop()?;
        checked("sc.exe", &["delete", SERVICE_NAME])?;
    }
    store::remove_firewall()?;
    if upgrade {
        return Ok(());
    }
    let _ = std::fs::remove_file(store::directory()?.join("private/upgrade.json"));
    let marker = store::directory()?.join("private/sas-policy.json");
    if marker.exists() {
        let (old, applied): (Option<u32>, u32) = serde_json::from_slice(&std::fs::read(&marker)?)?;
        if sas_value()? == Some(applied) {
            set_sas(old)?;
        }
        std::fs::remove_file(marker)?;
    }
    Ok(())
}

#[cfg(test)]
mod sas_tests {
    use super::*;
    #[test]
    fn sas_requires_explicit_service_policy() {
        for value in [None, Some(0), Some(2), Some(4), Some(u32::MAX)] {
            assert!(!sas_allowed(value));
        }
        assert!(sas_allowed(Some(1)));
        assert!(sas_allowed(Some(3)));
    }
    #[test]
    fn console_sas_uses_service_mode_and_rejects_agents_or_stale_sessions_before_calling_windows() {
        let mut calls = Vec::new();
        send_console_sas(1, Some(1), 100, 100, |as_user| calls.push(as_user)).unwrap();
        assert_eq!(calls, [false]);
        for (target, console, caller, service) in [
            (1, Some(1), 101, 100), // An agent cannot act as the SCM service.
            (1, Some(2), 100, 100),
            (1, None, 100, 100),
            (1, Some(1), 0, 0),
        ] {
            assert!(
                send_console_sas(target, console, caller, service, |_| panic!(
                    "must reject before SendSAS"
                ))
                .is_err()
            );
        }
    }
}
