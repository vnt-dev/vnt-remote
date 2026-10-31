//! User-context desktop effects, shared across installed/portable processes.
//! Persist the baseline before changing Windows. A named mutex serializes the
//! journal, and process creation times distinguish dead owners from reused PIDs.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::{mpsc, OnceLock},
    time::Duration,
};
use windows::{
    core::{BOOL, PCWSTR},
    Win32::{
        Foundation::*,
        System::{Com::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub show_wallpaper: bool,
    pub drag_full_windows: bool,
}
impl From<&vnt_proto::pb::DesktopEffects> for Settings {
    fn from(value: &vnt_proto::pb::DesktopEffects) -> Self {
        Self {
            show_wallpaper: value.show_wallpaper,
            drag_full_windows: value.drag_full_windows,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    pid: u32,
    created: u64,
    settings: Settings,
}
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Journal {
    wallpaper: Option<bool>,
    drag: Option<bool>,
    leases: BTreeMap<String, Owner>,
}
trait Backend {
    fn get(&self, wallpaper: bool) -> anyhow::Result<bool>;
    fn set(&self, wallpaper: bool, enabled: bool) -> anyhow::Result<()>;
}
impl Journal {
    fn desired(&self, wallpaper: bool) -> Option<bool> {
        if self.leases.is_empty() {
            if wallpaper {
                self.wallpaper
            } else {
                self.drag
            }
        } else {
            Some(self.leases.values().all(|o| {
                if wallpaper {
                    o.settings.show_wallpaper
                } else {
                    o.settings.drag_full_windows
                }
            }))
        }
    }
    fn snapshot(&mut self, backend: &impl Backend) {
        // Only called after the previous generation has been fully restored.
        self.wallpaper = backend.get(true).ok();
        self.drag = backend.get(false).ok();
    }
    fn apply(&mut self, backend: &impl Backend) {
        for wallpaper in [true, false] {
            let original = if wallpaper { self.wallpaper } else { self.drag };
            if original.is_none() {
                continue;
            }
            let desired = self.desired(wallpaper).unwrap();
            if backend.get(wallpaper).ok() == Some(desired)
                || backend.set(wallpaper, desired).is_ok()
            {
                if self.leases.is_empty() {
                    if wallpaper {
                        self.wallpaper = None;
                    } else {
                        self.drag = None;
                    }
                }
            } else {
                tracing::warn!(wallpaper, "应用或还原被控桌面效果失败，将重试");
            }
        }
    }
    fn restored(&self) -> bool {
        self.wallpaper.is_none() && self.drag.is_none()
    }
}

struct WindowsBackend {
    wallpaper: Option<IDesktopWallpaper>,
}
impl WindowsBackend {
    fn new() -> Self {
        Self {
            wallpaper: unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL) }.ok(),
        }
    }
}
impl Backend for WindowsBackend {
    fn get(&self, wallpaper: bool) -> anyhow::Result<bool> {
        unsafe {
            if wallpaper {
                let status = self
                    .wallpaper
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("桌面背景接口不可用"))?
                    .GetStatus()?;
                Ok(status.0 & DSS_ENABLED.0 != 0)
            } else {
                let mut enabled = BOOL(0);
                SystemParametersInfoW(
                    SPI_GETDRAGFULLWINDOWS,
                    0,
                    Some((&mut enabled as *mut BOOL).cast()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )?;
                Ok(enabled.as_bool())
            }
        }
    }
    fn set(&self, wallpaper: bool, enabled: bool) -> anyhow::Result<()> {
        unsafe {
            if wallpaper {
                self.wallpaper
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("桌面背景接口不可用"))?
                    .Enable(enabled)?;
            } else {
                SystemParametersInfoW(
                    SPI_SETDRAGFULLWINDOWS,
                    u32::from(enabled),
                    None,
                    SPIF_SENDCHANGE,
                )?;
            }
        }
        Ok(())
    }
}

fn process_created(pid: u32) -> anyhow::Result<u64> {
    let process = super::native::Handle(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?
    });
    anyhow::ensure!(super::native::process_running(&process), "租约进程已退出");
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user)?;
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}
struct Locked(super::native::Handle);
impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0 .0);
        }
    }
}

struct Store {
    path: PathBuf,
    mutex_name: Vec<u16>,
}
impl Store {
    fn new() -> anyhow::Result<Self> {
        let (session, identity) = super::native::process_identity(std::process::id())?;
        let sid = identity
            .split(':')
            .next()
            .ok_or_else(|| anyhow::anyhow!("缺少用户身份"))?;
        anyhow::ensure!(sid != "S-1-5-18", "系统服务不得修改用户桌面效果");
        let key = format!("{sid}-{session}");
        let directory = PathBuf::from(
            std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow::anyhow!("缺少用户配置目录"))?,
        )
        .join("VntRemote")
        .join("desktop-effects");
        std::fs::create_dir_all(&directory)?;
        Ok(Self {
            path: directory.join(format!("{key}.json")),
            mutex_name: format!("Local\\VntRemote.DesktopEffects.{key}\0")
                .encode_utf16()
                .collect(),
        })
    }
    fn lock(&self) -> anyhow::Result<Locked> {
        let handle = super::native::Handle(unsafe {
            CreateMutexW(None, false, PCWSTR(self.mutex_name.as_ptr()))?
        });
        let result = unsafe { WaitForSingleObject(handle.0, 3000) };
        anyhow::ensure!(
            result == WAIT_OBJECT_0 || result == WAIT_ABANDONED,
            "桌面效果锁超时"
        );
        Ok(Locked(handle))
    }
    fn read(&self) -> anyhow::Result<Journal> {
        match std::fs::read(&self.path) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(error) => Err(error.into()),
        }
    }
    fn save(&self, journal: &Journal) -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        Ok(())
    }
    fn update(&self, backend: &impl Backend, change: Change) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let mut persisted = journal.clone();
        journal
            .leases
            .retain(|_, owner| match process_created(owner.pid) {
                Ok(created) => created == owner.created,
                // An elevated peer can be alive even if this process cannot query
                // it. Never restore over another controller on an access failure.
                Err(error) => error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|e| {
                        e.code() == windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
                    }),
            });
        // Recover a crashed generation before reading any new baseline.
        if journal.leases.is_empty() && !journal.restored() {
            journal.apply(backend);
            self.save(&journal)?;
            persisted = journal.clone();
        }
        match change {
            Change::Acquire(id, owner) => {
                if journal.leases.is_empty() {
                    anyhow::ensure!(journal.restored(), "上次桌面效果尚未还原");
                    journal.snapshot(backend);
                }
                journal.leases.insert(id, owner);
            }
            Change::Release(id) => {
                journal.leases.remove(&id);
            }
            Change::Retry => {}
        }
        // Write-ahead: cancellation, crashes or partial API failures cannot lose
        // the baseline. Never apply if durable storage fails.
        if journal != persisted {
            self.save(&journal)?;
            persisted = journal.clone();
        }
        journal.apply(backend);
        if journal != persisted {
            self.save(&journal)?;
        }
        anyhow::ensure!(
            !journal.leases.is_empty() || journal.restored(),
            "被控桌面效果还原失败，保留原始设置并重试"
        );
        Ok(())
    }
}
enum Change {
    Acquire(String, Owner),
    Release(String),
    Retry,
}
struct Request {
    change: Change,
    reply: mpsc::Sender<Result<(), String>>,
}
static WORKER: OnceLock<Result<mpsc::Sender<Request>, String>> = OnceLock::new();
fn worker() -> Result<&'static mpsc::Sender<Request>, String> {
    WORKER
        .get_or_init(|| {
            let store = Store::new().map_err(|e| e.to_string())?;
            let (tx, rx) = mpsc::channel::<Request>();
            std::thread::Builder::new()
                .name("desktop-effects".into())
                .spawn(move || {
                    let initialized =
                        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
                    let backend = WindowsBackend::new();
                    let mut pending_releases = Vec::new();
                    loop {
                        match rx.recv_timeout(Duration::from_secs(1)) {
                            Ok(request) => {
                                let release = match &request.change {
                                    Change::Release(id) => Some(id.clone()),
                                    _ => None,
                                };
                                let result = store
                                    .update(&backend, request.change)
                                    .map_err(|e| e.to_string());
                                if result.is_err() {
                                    if let Some(id) = release {
                                        pending_releases.push(id);
                                    }
                                }
                                let _ = request.reply.send(result);
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                pending_releases.retain(|id| {
                                    store.update(&backend, Change::Release(id.clone())).is_err()
                                });
                                if let Err(error) = store.update(&backend, Change::Retry) {
                                    tracing::debug!("重试桌面效果恢复失败：{error:#}");
                                }
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    drop(backend);
                    if initialized {
                        unsafe {
                            CoUninitialize();
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(tx)
        })
        .as_ref()
        .map_err(Clone::clone)
}
fn request(change: Change) -> Result<(), String> {
    let (reply, rx) = mpsc::channel();
    worker()?
        .send(Request { change, reply })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?
}
pub fn initialize() {
    if let Err(error) = worker() {
        tracing::debug!("桌面效果不可用：{error}");
    }
}
pub struct Lease(String);
impl Lease {
    pub fn acquire(settings: Settings) -> Option<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).ok()?;
        let id = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let owner = Owner {
            pid: std::process::id(),
            created: process_created(std::process::id()).ok()?,
            settings,
        };
        // Construct before requesting: a timed-out acknowledgement may have
        // applied successfully, so even an unsuccessful acquire needs release.
        let lease = Self(id.clone());
        match request(Change::Acquire(id, owner)) {
            Ok(()) => Some(lease),
            Err(error) => {
                tracing::debug!("忽略不支持的桌面效果配置：{error}");
                None
            }
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        for attempt in 0..3 {
            match request(Change::Release(self.0.clone())) {
                Ok(()) => return,
                Err(error) if attempt == 2 => tracing::warn!("释放桌面效果租约失败：{error}"),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Fake {
        wallpaper: Cell<bool>,
        drag: Cell<bool>,
        unsupported_wallpaper: bool,
        fail_drag: Cell<bool>,
    }
    impl Backend for Fake {
        fn get(&self, wallpaper: bool) -> anyhow::Result<bool> {
            anyhow::ensure!(!wallpaper || !self.unsupported_wallpaper, "unsupported");
            Ok(if wallpaper {
                self.wallpaper.get()
            } else {
                self.drag.get()
            })
        }
        fn set(&self, wallpaper: bool, value: bool) -> anyhow::Result<()> {
            anyhow::ensure!(wallpaper || !self.fail_drag.get(), "failed");
            if wallpaper {
                self.wallpaper.set(value);
            } else {
                self.drag.set(value);
            }
            Ok(())
        }
    }
    fn backend() -> Fake {
        Fake {
            wallpaper: Cell::new(true),
            drag: Cell::new(false),
            unsupported_wallpaper: false,
            fail_drag: Cell::new(false),
        }
    }
    fn owner(show_wallpaper: bool, drag_full_windows: bool) -> Owner {
        Owner {
            pid: 1,
            created: 1,
            settings: Settings {
                show_wallpaper,
                drag_full_windows,
            },
        }
    }
    #[test]
    fn aggregate_and_restore_in_both_departure_orders() {
        for first in ["a", "b"] {
            let backend = backend();
            let mut state = Journal::default();
            state.snapshot(&backend);
            state.leases.insert("a".into(), owner(true, true));
            state.apply(&backend);
            assert!(backend.drag.get());
            state.leases.insert("b".into(), owner(false, false));
            state.apply(&backend);
            assert!(!backend.wallpaper.get() && !backend.drag.get());
            state.leases.remove(first);
            state.apply(&backend);
            assert_eq!(backend.wallpaper.get(), first == "b");
            assert_eq!(backend.drag.get(), first == "b");
            state.leases.clear();
            state.apply(&backend);
            assert!(backend.wallpaper.get());
            assert!(!backend.drag.get());
            assert!(state.restored());
            state.apply(&backend); // idempotent cleanup
        }
    }
    #[test]
    fn unsupported_and_failed_items_do_not_block_other_item_or_lose_baseline() {
        let mut backend = backend();
        backend.unsupported_wallpaper = true;
        let mut state = Journal::default();
        state.snapshot(&backend);
        state.leases.insert("a".into(), owner(false, true));
        state.apply(&backend);
        assert!(backend.wallpaper.get() && backend.drag.get());
        backend.fail_drag.set(true);
        state.leases.clear();
        state.apply(&backend);
        assert_eq!(state.drag, Some(false));
        let serialized = serde_json::to_vec(&state).unwrap();
        let mut recovered: Journal = serde_json::from_slice(&serialized).unwrap();
        backend.fail_drag.set(false);
        recovered.apply(&backend);
        assert!(!backend.drag.get());
        assert!(recovered.restored());
    }

    fn store(directory: &std::path::Path) -> Store {
        Store {
            path: directory.join("effects.json"),
            mutex_name: format!(
                "Local\\VntRemote.DesktopEffects.Test.{}\0",
                super::super::native::random().unwrap()
            )
            .encode_utf16()
            .collect(),
        }
    }
    fn live_owner(show_wallpaper: bool, drag_full_windows: bool) -> Owner {
        Owner {
            pid: std::process::id(),
            created: process_created(std::process::id()).unwrap(),
            settings: Settings {
                show_wallpaper,
                drag_full_windows,
            },
        }
    }
    #[test]
    fn shared_store_reaggregates_and_releases_only_the_departing_connection() {
        let directory = tempfile::tempdir().unwrap();
        let first = store(directory.path());
        let second = Store {
            path: first.path.clone(),
            mutex_name: first.mutex_name.clone(),
        };
        let backend = backend();
        first
            .update(
                &backend,
                Change::Acquire("a".into(), live_owner(true, true)),
            )
            .unwrap();
        second
            .update(
                &backend,
                Change::Acquire("b".into(), live_owner(false, false)),
            )
            .unwrap();
        assert!(!backend.wallpaper.get() && !backend.drag.get());
        second
            .update(&backend, Change::Release("b".into()))
            .unwrap();
        assert!(backend.wallpaper.get() && backend.drag.get());
        first.update(&backend, Change::Release("a".into())).unwrap();
        first.update(&backend, Change::Release("a".into())).unwrap();
        assert!(backend.wallpaper.get() && !backend.drag.get());
        assert!(first.read().unwrap().restored());
    }
    #[test]
    fn crashed_owners_restore_before_new_baseline_and_failed_restores_block_acquisition() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let backend = backend();
        backend.wallpaper.set(false);
        backend.drag.set(true);
        let mut state = Journal {
            wallpaper: Some(true),
            drag: Some(false),
            ..Default::default()
        };
        state.leases.insert(
            "dead".into(),
            Owner {
                pid: 0,
                created: 0,
                settings: Settings {
                    show_wallpaper: false,
                    drag_full_windows: true,
                },
            },
        );
        store.save(&state).unwrap();
        backend.fail_drag.set(true);
        assert!(store
            .update(
                &backend,
                Change::Acquire("new".into(), live_owner(false, true))
            )
            .is_err());
        assert!(backend.wallpaper.get());
        assert_eq!(store.read().unwrap().drag, Some(false));
        assert!(store.read().unwrap().leases.is_empty());
        backend.fail_drag.set(false);
        store
            .update(
                &backend,
                Change::Acquire("new".into(), live_owner(false, true)),
            )
            .unwrap();
        assert!(!backend.wallpaper.get() && backend.drag.get());
        assert_eq!(store.read().unwrap().wallpaper, Some(true));
        assert_eq!(store.read().unwrap().drag, Some(false));
        store
            .update(&backend, Change::Release("new".into()))
            .unwrap();
        assert!(backend.wallpaper.get() && !backend.drag.get());
    }
    #[test]
    fn persistence_failure_never_changes_system_settings() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory.path().join("missing"));
        let backend = backend();
        assert!(store
            .update(
                &backend,
                Change::Acquire("a".into(), live_owner(false, true))
            )
            .is_err());
        assert!(backend.wallpaper.get() && !backend.drag.get());
    }
}
