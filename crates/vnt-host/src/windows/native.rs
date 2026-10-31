use std::{ffi::c_void, os::windows::io::AsRawHandle, path::Path, time::Duration};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Security::Authorization::*,
        Security::*,
        System::{
            Environment::*, JobObjects::*, Pipes::*, RemoteDesktop::*, StationsAndDesktops::*,
            Threading::*,
        },
        UI::WindowsAndMessaging::*,
    },
};

pub fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(Some(0)).collect()
}
pub struct Handle(pub HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
impl Handle {
    pub fn new(h: HANDLE) -> anyhow::Result<Self> {
        anyhow::ensure!(!h.is_invalid(), "无效 Windows 句柄");
        Ok(Self(h))
    }
}

pub struct Descriptor(PSECURITY_DESCRIPTOR);
impl Descriptor {
    pub fn new(sddl: &str) -> anyhow::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide(sddl).as_ptr()),
                1,
                &mut sd,
                None,
            )?;
        }
        Ok(Self(sd))
    }
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}
pub fn pipe(name: &str, sddl: &str, first: bool) -> anyhow::Result<NamedPipeServer> {
    let sd = Descriptor::new(sddl)?;
    let sa = sd.attributes();
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first)
        .reject_remote_clients(true)
        .in_buffer_size(64 * 1024)
        .out_buffer_size(64 * 1024);
    Ok(unsafe { opts.create_with_security_attributes_raw(name, &sa as *const _ as *mut c_void)? })
}
pub fn client_pid(pipe: &impl AsRawHandle) -> anyhow::Result<u32> {
    let mut pid = 0;
    unsafe {
        GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)?;
    }
    Ok(pid)
}
pub fn server_pid(pipe: &impl AsRawHandle) -> anyhow::Result<u32> {
    let mut pid = 0;
    unsafe {
        GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)?;
    }
    Ok(pid)
}
pub fn process_path(pid: u32) -> anyhow::Result<std::path::PathBuf> {
    unsafe {
        let h = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let mut name = vec![0u16; 32768];
        let mut n = name.len() as u32;
        QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(name.as_mut_ptr()), &mut n)?;
        Ok(std::path::PathBuf::from(String::from_utf16_lossy(
            &name[..n as usize],
        )))
    }
}
pub fn random() -> anyhow::Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(b.iter().map(|v| format!("{v:02x}")).collect())
}
pub fn console_session() -> Option<u32> {
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}
/// Interactive GUI sessions are independent of the physical capture target.
/// Exclude disconnected RDP sessions and Session 0; callers validate the login.
pub fn desktop_sessions() -> anyhow::Result<Vec<u32>> {
    unsafe {
        let mut sessions = std::ptr::null_mut();
        let mut count = 0;
        WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count)?;
        let mut active = Vec::new();
        for index in 0..count as usize {
            let session = &*sessions.add(index);
            if session.SessionId != 0 && session.State == WTSActive {
                active.push(session.SessionId);
            }
        }
        WTSFreeMemory(sessions as *mut c_void);
        active.sort_unstable();
        Ok(active)
    }
}
/// Prefer a connected console, otherwise capture the connected RDP login.
/// With no connected login, retain the console for pre-login/lock-screen access.
pub fn capture_session() -> anyhow::Result<Option<u32>> {
    select_capture_session(console_session(), &desktop_sessions()?)
}

fn select_capture_session(console: Option<u32>, active: &[u32]) -> anyhow::Result<Option<u32>> {
    if console.is_some_and(|session| active.contains(&session)) {
        return Ok(console);
    }
    match active {
        [] => Ok(console),
        [session] => Ok(Some(*session)),
        _ => anyhow::bail!("多个 RDP 会话同时连接，无法确定被控桌面，请仅保留一个已连接会话"),
    }
}

pub fn token(session: u32, user: bool) -> anyhow::Result<Handle> {
    unsafe {
        let mut original = HANDLE::default();
        if user {
            WTSQueryUserToken(session, &mut original)?;
        } else {
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut original)?;
        }
        let original = Handle(original);
        let mut duplicate = HANDLE::default();
        DuplicateTokenEx(
            original.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut duplicate,
        )?;
        let duplicate = Handle(duplicate);
        if !user {
            SetTokenInformation(
                duplicate.0,
                TokenSessionId,
                &session as *const _ as *const c_void,
                4,
            )?;
        }
        Ok(duplicate)
    }
}
pub fn user_sid(token: HANDLE) -> anyhow::Result<String> {
    unsafe {
        let mut size = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        let mut bytes = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token,
            TokenUser,
            Some(bytes.as_mut_ptr() as *mut c_void),
            size,
            &mut size,
        )?;
        let user = &*(bytes.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s)?;
        let value = s.to_string()?;
        LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
        Ok(value)
    }
}
pub struct Process {
    pub handle: Option<Handle>,
    pub pid: u32,
    job: Option<Handle>,
}
impl Process {
    pub fn running(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle.as_ref().unwrap().0, 0) == WAIT_TIMEOUT }
    }
    /// SCM must not report Stopped while an unresponsive desktop still locks
    /// the installed executable. Allow cleanup, close its job, then join exit.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let process = self.handle.take().unwrap();
        let job = self.job.take();
        tokio::task::spawn_blocking(move || {
            unsafe {
                WaitForSingleObject(process.0, 1000);
            }
            drop(job);
            let exited = unsafe { WaitForSingleObject(process.0, 5000) } == WAIT_OBJECT_0;
            drop(process);
            anyhow::ensure!(exited, "等待桌面进程退出超时");
            Ok(())
        })
        .await?
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Closing the IPC stream lets the agent release held input and OLE state.
        // Keep the job alive for that bounded cleanup, then kill any stuck child.
        if let (Some(process), Some(job)) = (self.handle.take(), self.job.take()) {
            std::thread::spawn(move || {
                unsafe {
                    WaitForSingleObject(process.0, 1000);
                }
                drop(job);
                drop(process);
            });
        }
    }
}
pub fn spawn_agent(
    path: &Path,
    session: u32,
    user: bool,
    pipe_name: &str,
    parent: u32,
) -> anyhow::Result<Process> {
    spawn_in_session(
        path,
        session,
        user,
        &format!(
            "{} {} {}",
            if user { "--user-agent" } else { "--agent" },
            pipe_name,
            parent
        ),
        true,
    )
}

pub fn spawn_desktop(path: &Path, session: u32, port: u16) -> anyhow::Result<Process> {
    spawn_in_session(
        path,
        session,
        true,
        &format!("--local-service-port {port}"),
        false,
    )
}

fn spawn_in_session(
    path: &Path,
    session: u32,
    user: bool,
    args: &str,
    hidden: bool,
) -> anyhow::Result<Process> {
    unsafe {
        let token = token(session, user)?;
        let job = Handle(CreateJobObjectW(None, PCWSTR::null())?);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if !hidden {
            // Updater/NSIS must survive the desktop and service stopping during
            // upgrade. Only the managed GUI itself belongs to this job.
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        }
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of_val(&limits) as u32,
        )?;
        let desktop = wide("winsta0\\default");
        let executable = wide(path);
        let mut environment = std::ptr::null_mut();
        CreateEnvironmentBlock(&mut environment, Some(token.0), false)?;
        struct Environment(*mut c_void);
        impl Drop for Environment {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyEnvironmentBlock(self.0);
                }
            }
        }
        let environment = Environment(environment);
        let mut cmd = wide(format!("\"{}\" {}", path.display(), args));
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_ptr() as *mut _),
            dwFlags: STARTF_USESHOWWINDOW,
            wShowWindow: if hidden { SW_HIDE.0 } else { SW_SHOWNORMAL.0 } as u16,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(executable.as_ptr()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.0),
            PCWSTR::null(),
            &si,
            &mut pi,
        )?;
        let process = Handle(pi.hProcess);
        let thread = Handle(pi.hThread);
        if let Err(e) = AssignProcessToJobObject(job.0, process.0) {
            let _ = TerminateProcess(process.0, 1);
            return Err(e.into());
        }
        if ResumeThread(thread.0) == u32::MAX {
            let _ = TerminateProcess(process.0, 1);
            anyhow::bail!("启动会话代理失败");
        }
        Ok(Process {
            handle: Some(process),
            pid: pi.dwProcessId,
            job: Some(job),
        })
    }
}

/// Bind only threads without windows/hooks. All desktop resources die with this guard.
pub struct Desktop(pub HDESK, pub String);
unsafe impl Send for Desktop {}
impl Desktop {
    pub fn input() -> anyhow::Result<Self> {
        Self::open_input(false)
    }
    pub fn input_for_injection() -> anyhow::Result<Self> {
        Self::open_input(true)
    }
    fn open_input(injection: bool) -> anyhow::Result<Self> {
        unsafe {
            let h = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(
                    DESKTOP_READOBJECTS.0
                        | DESKTOP_WRITEOBJECTS.0
                        | DESKTOP_SWITCHDESKTOP.0
                        | DESKTOP_ENUMERATE.0
                        // SendInput checks journal-playback access even though
                        // no journal hook is installed. Capture rights alone
                        // allow video while rejecting both mouse and keyboard.
                        | if injection { DESKTOP_JOURNALPLAYBACK.0 } else { 0 },
                ),
            )?;
            let mut bytes = [0u16; 256];
            let mut needed = 0;
            GetUserObjectInformationW(
                HANDLE(h.0),
                UOI_NAME,
                Some(bytes.as_mut_ptr() as *mut c_void),
                512,
                Some(&mut needed),
            )?;
            let n = bytes.iter().position(|v| *v == 0).unwrap_or(bytes.len());
            Ok(Self(h, String::from_utf16_lossy(&bytes[..n])))
        }
    }
    pub fn bind(&self) -> anyhow::Result<()> {
        unsafe {
            SetThreadDesktop(self.0)?;
        }
        Ok(())
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseDesktop(self.0);
        }
    }
}
pub fn desktop_name() -> anyhow::Result<String> {
    Ok(Desktop::input()?.1.clone())
}
pub async fn connect(
    name: &str,
) -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(name) {
            Ok(p) => return Ok(p),
            Err(e)
                if tokio::time::Instant::now() < deadline
                    && matches!(e.raw_os_error(), Some(2 | 231)) =>
            {
                tokio::time::sleep(Duration::from_millis(30)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn session_identity(session: u32) -> anyhow::Result<String> {
    match token(session, true) {
        Ok(token) => token_identity(&token),
        Err(error)
            if error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|e| e.code() == windows::core::HRESULT::from_win32(1008)) =>
        {
            Ok(String::new())
        }
        Err(error) => Err(error),
    }
}

pub fn process_identity(pid: u32) -> anyhow::Result<(u32, String)> {
    unsafe {
        let mut session = 0;
        ProcessIdToSessionId(pid, &mut session)?;
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let mut token = HANDLE::default();
        OpenProcessToken(process.0, TOKEN_QUERY, &mut token)?;
        Ok((session, token_identity(&Handle(token))?))
    }
}

pub fn watch_process(pid: u32) -> anyhow::Result<Handle> {
    Ok(Handle(unsafe {
        OpenProcess(PROCESS_SYNCHRONIZE, false, pid)?
    }))
}

pub fn process_running(process: &Handle) -> bool {
    unsafe { WaitForSingleObject(process.0, 0) == WAIT_TIMEOUT }
}

fn token_identity(token: &Handle) -> anyhow::Result<String> {
    unsafe {
        let mut stats = TOKEN_STATISTICS::default();
        let mut size = 0;
        GetTokenInformation(
            token.0,
            TokenStatistics,
            Some(&mut stats as *mut _ as *mut c_void),
            std::mem::size_of_val(&stats) as u32,
            &mut size,
        )?;
        Ok(format!(
            "{}:{}:{}",
            user_sid(token.0)?,
            stats.AuthenticationId.HighPart,
            stats.AuthenticationId.LowPart
        ))
    }
}
pub fn require_system(pid: u32) -> anyhow::Result<()> {
    unsafe {
        let p = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let mut t = HANDLE::default();
        OpenProcessToken(p.0, TOKEN_QUERY, &mut t)?;
        let t = Handle(t);
        anyhow::ensure!(user_sid(t.0)? == "S-1-5-18", "父进程不是 Windows 系统服务");
    }
    Ok(())
}

pub fn system_executable(name: &str) -> anyhow::Result<std::path::PathBuf> {
    unsafe {
        let mut b = vec![0u16; 32768];
        let n = windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut b));
        anyhow::ensure!(n > 0 && (n as usize) < b.len(), "无法定位 Windows 系统目录");
        Ok(std::path::PathBuf::from(String::from_utf16_lossy(&b[..n as usize])).join(name))
    }
}

pub fn current_session() -> anyhow::Result<u32> {
    let mut id = 0;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut id)?;
    }
    Ok(id)
}
pub struct ManagementLock(Handle);
impl Drop for ManagementLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0 .0);
        }
    }
}
pub fn management_lock() -> anyhow::Result<ManagementLock> {
    let sd = Descriptor::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)")?;
    let sa = sd.attributes();
    let mutex = Handle(unsafe {
        CreateMutexW(
            Some(&sa),
            false,
            PCWSTR(wide(r"Global\VntRemoteService.Management.v1").as_ptr()),
        )?
    });
    let result = unsafe { WaitForSingleObject(mutex.0, 30000) };
    anyhow::ensure!(
        result == WAIT_OBJECT_0 || result == WAIT_ABANDONED,
        "另一个服务管理操作尚未完成"
    );
    Ok(ManagementLock(mutex))
}

pub fn file_owner(path: &Path) -> anyhow::Result<String> {
    unsafe {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(
            PCWSTR(wide(path).as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
        .ok()?;
        let mut text = PWSTR::null();
        let result = ConvertSidToStringSidW(owner, &mut text);
        if let Err(e) = result {
            LocalFree(Some(HLOCAL(sd.0)));
            return Err(e.into());
        }
        let value = text.to_string();
        LocalFree(Some(HLOCAL(text.0 as *mut c_void)));
        LocalFree(Some(HLOCAL(sd.0)));
        Ok(value?)
    }
}

#[cfg(test)]
mod capture_session_tests {
    use super::*;

    #[test]
    fn connected_rdp_replaces_disconnected_console_and_returns_on_disconnect() {
        assert_eq!(select_capture_session(Some(2), &[1]).unwrap(), Some(1));
        assert_eq!(select_capture_session(None, &[1]).unwrap(), Some(1));
        assert_eq!(select_capture_session(Some(2), &[]).unwrap(), Some(2));
        assert_eq!(select_capture_session(None, &[]).unwrap(), None);
    }

    #[test]
    fn connected_console_has_priority_and_ambiguous_rdp_fails_closed() {
        assert_eq!(select_capture_session(Some(2), &[1, 2]).unwrap(), Some(2));
        assert!(select_capture_session(Some(3), &[1, 2]).is_err());
        assert!(select_capture_session(None, &[1, 2]).is_err());
    }
}
