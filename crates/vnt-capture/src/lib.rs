//! 屏幕捕获封装（基于 `screencap` crate：Windows Graphics Capture / DXGI / GDI）。
//!
//! `screencap` 的 `Monitor`/`ScreenGrabber` 均为 `!Send`，因此：
//! - 枚举接口在线程内完成，返回纯数据 [`MonitorInfo`]
//! - 捕获循环在专用线程运行，直接向独立订阅发布共享 BGRA 帧
//!   ——帧到达、取消信号都直接唤醒消费方，无固定轮询间隔
//!
//! 捕获后端优先级：Graphics(WGC, 含鼠标) → DXGI → GDI，取第一个可用。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod region;
pub use region::*;

#[cfg(windows)]
use screencap::{CaptureType, Monitor, PixelFormat, Region, ScreenGrabber};
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::{Monitor, ScreenGrabber};
#[cfg(target_os = "macos")]
use screencap::{PixelFormat, Region};

mod desktop;
pub use desktop::{DesktopCapture, DesktopSnapshot};
mod pacing;
mod publication;
use pacing::FramePacer;
mod shared;
pub use publication::CaptureCancelHandle;
use publication::{CapturePublisher, Publication};
pub use shared::{CaptureSubscription, SharedCapture};

/// Update the indicator for all captures in this process without recreating sources.
pub fn set_capture_border(enabled: bool) {
    #[cfg(windows)]
    screencap::set_capture_border(enabled);
    #[cfg(not(windows))]
    let _ = enabled;
}

/// 显示器描述（Send，可跨线程传给 UI/网络层）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MonitorInfo {
    #[serde(default)]
    pub orientation: u32,
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// HMONITOR 原始值，供 vnt-input 映射绝对坐标。
    pub hmonitor: usize,
    pub primary: bool,
}
#[doc(hidden)]
pub fn synthetic_monitor(index: u32, width: u32, height: u32) -> MonitorInfo {
    MonitorInfo {
        index,
        width,
        height,
        name: "Synthetic".into(),
        hmonitor: 0,
        primary: true,
        orientation: 0,
    }
}

/// 一帧 BGRA 捕获（BGRA 逐像素，行序自上而下）。
///
/// 刻意不实现 `Clone`：一帧是满屏缓冲（4K 下约 33MB），帧级克隆是一次
/// 无声的全帧 memcpy。需要多持有者时用 `Arc<CapturedFrame>` 共享
/// （见 `publication.rs` 的发布路径），让"复制像素"这件事在编译期就无处隐藏。
#[derive(Debug)]
pub struct CapturedFrame {
    /// Native source identity before cropping/scaling; absent for synthetic frames.
    pub source_monitor: Option<MonitorInfo>,
    /// Service desktop epoch; zero for other sources.
    pub desktop_generation: u64,
    pub captured_at: Instant,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// 本次 next_frame 耗时（毫秒）。
    pub capture_ms: u32,
    /// 捕获源相对时间戳（毫秒）；共享源的消费者按需换算为会话时间。
    pub pts_ms: u64,
}

/// Retain one published frame as the producer's reuse candidate. Its pixels may be overwritten
/// only after every subscriber/encoder has released its Arc. Slow consumers
/// get a fresh allocation; no frame pool or extra pixel copy is involved.
#[derive(Default)]
struct CaptureBuffer {
    previous: Option<Arc<CapturedFrame>>,
}
impl CaptureBuffer {
    fn pixels(&mut self, len: usize) -> Vec<u8> {
        let mut pixels = match self.previous.take().map(Arc::try_unwrap) {
            Some(Ok(frame)) => frame.bgra,
            Some(Err(frame)) => {
                // Keep this one candidate until its slow reader finishes.
                // Replacing it with each newer frame would prevent reuse
                // precisely when encoding takes longer than one capture tick.
                self.previous = Some(frame);
                Vec::new()
            }
            None => Vec::new(),
        };
        pixels.resize(len, 0);
        pixels
    }
    fn share(&mut self, frame: CapturedFrame) -> Arc<CapturedFrame> {
        let frame = Arc::new(frame);
        if self.previous.is_none() {
            self.previous = Some(frame.clone());
        }
        frame
    }
}

#[cfg(windows)]
fn monitor_handle(m: &Monitor) -> usize {
    m.h_monitor().0 as usize
}
#[cfg(target_os = "macos")]
fn monitor_handle(m: &Monitor) -> usize {
    m.index() as usize
}
#[cfg(target_os = "macos")]
fn open_grabber_for(monitor: &Monitor, _secure: bool) -> Result<ScreenGrabber, CaptureError> {
    ScreenGrabber::new(monitor).map_err(|e| CaptureError::Create(e.to_string()))
}

#[cfg(test)]
mod capture_buffer_tests {
    use super::*;
    fn frame(bgra: Vec<u8>) -> CapturedFrame {
        CapturedFrame {
            source_monitor: None,
            desktop_generation: 0,
            captured_at: Instant::now(),
            width: 2,
            height: 2,
            bgra,
            capture_ms: 0,
            pts_ms: 0,
        }
    }
    #[test]
    fn reuse_waits_for_readers_and_never_changes_their_pixels() {
        let mut buffer = CaptureBuffer::default();
        let first = buffer.share(frame(vec![7; 16]));
        let first_address = first.bgra.as_ptr();
        let mut fresh = buffer.pixels(16);
        fresh.fill(9);
        let second = buffer.share(frame(fresh));
        assert_eq!(first.bgra, vec![7; 16]);
        assert_ne!(first.bgra.as_ptr(), second.bgra.as_ptr());
        drop(first);
        let mut reused = buffer.pixels(16);
        assert_eq!(reused.as_ptr(), first_address);
        reused.fill(11);
        assert_eq!(second.bgra, vec![9; 16]);
        let shared = buffer.share(frame(reused));
        let weak = Arc::downgrade(&shared);
        drop(shared);
        drop(buffer);
        assert!(
            weak.upgrade().is_none(),
            "retirement must release the retained pixels"
        );
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("当前会话或桌面未找到可用显示器")]
    NoMonitor,
    #[error("显示器存在，但当前桌面暂不可采集: {0}")]
    MonitorUnavailable(String),
    #[error("创建抓屏器失败: {0}")]
    Create(String),
    #[error("抓帧失败: {0}")]
    Grab(String),
    #[error("捕获线程已停止")]
    Stopped,
}

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 枚举显示器。内部短线程完成（`Monitor` 非 Send）。
pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
    enumerate_monitors(false)
}

/// Refresh the input desktop's displays and exclude sources whose capture backend
/// cannot be opened or read. Run at admission, not in the metadata polling loop.
pub fn list_available_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
    enumerate_monitors(true)
}

fn enumerate_monitors(probe_capture: bool) -> Result<Vec<MonitorInfo>, CaptureError> {
    std::thread::Builder::new()
        .name("monitor-enum".into())
        .spawn(move || -> Result<Vec<MonitorInfo>, CaptureError> {
            #[cfg(windows)]
            let _desktop = bind_input_desktop()?;
            let all = Monitor::all().map_err(e2s).map_err(CaptureError::Create)?;
            let primary_index = Monitor::primary().ok().map(|m| m.index()).unwrap_or(0);
            let mut out = Vec::new();
            let mut failures = Vec::new();
            for m in &all {
                let descriptor = m
                    .size()
                    .and_then(|size| m.device_name().map(|name| (size, name)));
                let ((w, h), name) = match descriptor {
                    Ok(((w, h), name)) if w > 0 && h > 0 && !name.is_empty() => ((w, h), name),
                    other => {
                        log::debug!("忽略不可用显示器 {}: {other:?}", m.index());
                        failures.push(format!(
                            "屏幕 {} 的显示信息不可用: {other:?}",
                            m.index() + 1
                        ));
                        continue;
                    }
                };
                if probe_capture {
                    let probe = (|| -> Result<(), CaptureError> {
                        #[cfg(windows)]
                        let _grabber = open_grabber_for(
                            m,
                            SERVICE_DESKTOP.load(Ordering::Acquire)
                                && desktop::input_name()
                                    .is_some_and(|n| !n.eq_ignore_ascii_case("default")),
                        )?;
                        #[cfg(target_os = "macos")]
                        let mut grabber = open_grabber(m)?;
                        #[cfg(target_os = "macos")]
                        {
                            let mut pixels = vec![0; w as usize * h as usize * 4];
                            match grabber.next_frame_region_format(
                                &mut pixels,
                                None,
                                PixelFormat::BGRA,
                            ) {
                                Ok(_) => Ok(()),
                                // An idle WGC/DXGI source may not have a new frame yet.
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
                                Err(e) => Err(CaptureError::Grab(e.to_string())),
                            }
                        }
                        #[cfg(windows)]
                        Ok(())
                    })();
                    if let Err(error) = probe {
                        log::warn!("忽略无法采集的显示器 {name}: {error}");
                        failures.push(format!("{name}: {error}"));
                        continue;
                    }
                }
                out.push(MonitorInfo {
                    index: m.index(),
                    width: w,
                    height: h,
                    name: name.clone(),
                    hmonitor: monitor_handle(m),
                    orientation: monitor_orientation(&name),
                    primary: m.index() == primary_index,
                });
            }
            if out.is_empty() {
                if !failures.is_empty() {
                    return Err(CaptureError::MonitorUnavailable(failures.join("; ")));
                }
                return Err(CaptureError::NoMonitor);
            }
            Ok(out)
        })
        .map_err(|e| CaptureError::Create(format!("创建显示器枚举线程失败: {e}")))?
        .join()
        .map_err(|_| CaptureError::Create("显示器枚举线程异常退出".into()))?
}

static SERVICE_DESKTOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Only a SYSTEM session agent may enable secure desktop capture.
static SECURE_DESKTOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn enable_service_desktop(secure: bool) {
    SERVICE_DESKTOP.store(true, Ordering::Release);
    SECURE_DESKTOP.store(secure, Ordering::Release);
}
#[cfg(windows)]
struct BoundDesktop {
    original: windows::Win32::System::StationsAndDesktops::HDESK,
    bound: windows::Win32::System::StationsAndDesktops::HDESK,
}
#[cfg(windows)]
impl Drop for BoundDesktop {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let _ = SetThreadDesktop(self.original);
            if CloseDesktop(self.bound).is_err() {
                // WGC may keep dispatcher windows until this native thread exits.
                DEFERRED_DESKTOPS
                    .lock()
                    .unwrap()
                    .push(self.bound.0 as usize);
            }
        }
    }
}
#[cfg(windows)]
static DEFERRED_DESKTOPS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
#[cfg(windows)]
fn reap_desktops() {
    use windows::Win32::System::StationsAndDesktops::{CloseDesktop, HDESK};
    let retired = std::mem::take(&mut *DEFERRED_DESKTOPS.lock().unwrap());
    let mut pending = Vec::new();
    for handle in retired {
        if unsafe { CloseDesktop(HDESK(handle as *mut _)) }.is_err() {
            pending.push(handle);
        }
    }
    DEFERRED_DESKTOPS.lock().unwrap().extend(pending);
}
#[cfg(not(windows))]
fn reap_desktops() {}

#[cfg(windows)]
fn bind_input_desktop() -> Result<Option<BoundDesktop>, CaptureError> {
    use windows::Win32::System::StationsAndDesktops::*;
    if !SERVICE_DESKTOP.load(Ordering::Acquire) {
        return Ok(None);
    }
    unsafe {
        let h = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(
                DESKTOP_READOBJECTS.0
                    | DESKTOP_WRITEOBJECTS.0
                    | DESKTOP_SWITCHDESKTOP.0
                    | DESKTOP_CREATEWINDOW.0,
            ),
        )
        .map_err(|e| CaptureError::Create(e.to_string()))?;
        let original = GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId())
            .map_err(|e| CaptureError::Create(e.to_string()))?;
        let guard = BoundDesktop { original, bound: h };
        SetThreadDesktop(h).map_err(|e| CaptureError::Create(e.to_string()))?;
        Ok(Some(guard))
    }
}

fn open_grabber(monitor: &Monitor) -> Result<ScreenGrabber, CaptureError> {
    open_grabber_for(monitor, SECURE_DESKTOP.load(Ordering::Acquire))
}
#[cfg(windows)]
fn open_grabber_for(monitor: &Monitor, secure: bool) -> Result<ScreenGrabber, CaptureError> {
    // WGC cannot access a secure desktop. Both DXGI and GDI run on the input
    // desktop already bound by the SYSTEM agent; GDI also covers drivers that
    // cannot duplicate that desktop after an RDP disconnect.
    try_capture_backends(
        capture_backends(secure),
        |ty| ScreenGrabber::new(monitor, ty).map_err(|e| CaptureError::Create(e.to_string())),
        |grabber| {
            let (w, h) = monitor
                .size()
                .map_err(|e| CaptureError::Grab(e.to_string()))?;
            let mut pixels = vec![0; w as usize * h as usize * 4];
            // Some drivers create a source successfully and fail on its first read.
            // Admission and actual capture must apply the same fallback in that case.
            match grabber.next_frame_region_format(&mut pixels, None, PixelFormat::BGRA) {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
                Err(e) => Err(CaptureError::Grab(e.to_string())),
            }
        },
    )
}

#[cfg(windows)]
fn capture_backends(secure: bool) -> &'static [CaptureType] {
    if secure {
        &[CaptureType::Dxgi, CaptureType::Gdi]
    } else {
        &[CaptureType::Graphics, CaptureType::Dxgi, CaptureType::Gdi]
    }
}

#[cfg(windows)]
fn try_capture_backends<T>(
    types: &[CaptureType],
    mut open: impl FnMut(CaptureType) -> Result<T, CaptureError>,
    mut validate: impl FnMut(&mut T) -> Result<(), CaptureError>,
) -> Result<T, CaptureError> {
    let mut failures = Vec::new();
    for &ty in types {
        match open(ty).and_then(|mut grabber| {
            validate(&mut grabber)?;
            Ok(grabber)
        }) {
            Ok(g) => {
                log::info!("抓屏后端: {ty:?}");
                return Ok(g);
            }
            Err(e) => failures.push(format!("{ty:?}: {e}")),
        }
    }
    Err(CaptureError::Create(format!(
        "所有抓屏后端均失败: {}",
        failures.join("; ")
    )))
}

#[cfg(all(test, windows))]
mod backend_tests {
    use super::*;

    #[test]
    fn secure_desktop_uses_gdi_when_dxgi_cannot_read_a_frame() {
        let mut attempted = Vec::new();
        let backend = try_capture_backends(
            capture_backends(true),
            |ty| {
                attempted.push(ty);
                Ok(ty)
            },
            |ty| {
                if *ty == CaptureType::Dxgi {
                    Err(CaptureError::Grab("DXGI_ERROR_ACCESS_LOST".into()))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(backend, CaptureType::Gdi);
        assert_eq!(attempted, [CaptureType::Dxgi, CaptureType::Gdi]);
    }

    #[test]
    fn failed_backends_preserve_creation_and_first_frame_errors() {
        let error = try_capture_backends(
            capture_backends(false),
            |ty| {
                if ty == CaptureType::Graphics {
                    Err(CaptureError::Create("WGC access denied".into()))
                } else {
                    Ok(ty)
                }
            },
            |ty| Err(CaptureError::Grab(format!("{ty:?} first frame failed"))),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Graphics: 创建抓屏器失败: WGC access denied"),
            "{error}"
        );
        assert!(error.contains("Dxgi first frame failed"), "{error}");
        assert!(error.contains("Gdi first frame failed"), "{error}");
    }
}

/// 捕获线程句柄。
///
/// 停止语义：取消发布（让采集端后续投放失败、唤醒等待中的管线）后
/// **有界**等待线程退出。WGC 无新画面时最多等待 250ms 后复用缓存，
/// 捕获循环随后检查取消。若驱动在其他调用中阻塞，超时后 detach，
/// 线程会在下一次可中断点自行退出，绝不永久 join。
struct CaptureHandle {
    publication: Arc<Publication>,
    join: Option<JoinHandle<()>>,
}

/// 停止等待上限。
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

impl CaptureHandle {
    fn join_bounded(&mut self, timeout: Duration) {
        if let Some(j) = self.join.take() {
            let deadline = Instant::now() + timeout;
            while !j.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if j.is_finished() {
                let _ = j.join();
                reap_desktops();
            } else {
                log::debug!("捕获线程未在 {:?} 内退出，detach 由其自行退出", timeout);
                drop(j); // detach：线程退出时自行释放资源
            }
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // EOF already closed receivers and must keep their final pending frame.
        if !self.publication.is_stopped() {
            self.publication.cancel();
        }
        self.join_bounded(STOP_JOIN_TIMEOUT);
    }
}

/// Start a capture thread; JoinHandle::is_finished also detects panic exits.
fn spawn_capture_thread(
    name: String,
    body: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle<()>, CaptureError> {
    std::thread::Builder::new()
        .name(name)
        .spawn(body)
        .map_err(|e| CaptureError::Create(format!("创建线程失败: {e}")))
}

/// 启动唯一捕获生产者，直接向订阅者发布最新 BGRA 帧，直到源停止。
///
/// 分辨率变化通过帧的 `width/height` 字段体现（缓冲区自动扩容）。
fn spawn_capture(monitor_index: u32, tx: CapturePublisher) -> Result<CaptureHandle, CaptureError> {
    // Validate before spawning: failure must not leave an unowned capture thread.
    if !list_monitors()?.iter().any(|m| m.index == monitor_index) {
        return Err(CaptureError::NoMonitor);
    }
    let publication = tx.source();
    let target_fps = publication.target_fps.clone();
    let thread_fps = target_fps.clone();
    let join = spawn_capture_thread(format!("capture-{monitor_index}"), move || {
        if let Err(e) = capture_loop(monitor_index, thread_fps, &tx, None, None, Instant::now()) {
            log::error!("捕获线程退出: {e}");
        }
    })?;
    Ok(CaptureHandle {
        publication,
        join: Some(join),
    })
}

fn spawn_resilient_capture(
    monitor: u32,
    tx: CapturePublisher,
    desktop: Arc<std::sync::atomic::AtomicBool>,
    available: Arc<std::sync::atomic::AtomicBool>,
) -> Result<CaptureHandle, CaptureError> {
    let publication = tx.source();
    let target_fps = publication.target_fps.clone();
    let rate = target_fps.clone();
    let join = spawn_capture_thread("portable-capture".into(), move || {
        let start = Instant::now();
        while !tx.is_cancelled() {
            available.store(false, Ordering::Release);
            if desktop.load(Ordering::Acquire) {
                if let Err(error) = capture_loop(
                    monitor,
                    rate.clone(),
                    &tx,
                    Some(&desktop),
                    Some(&available),
                    start,
                ) {
                    log::debug!("普通桌面采集暂停，稍后重试：{error}");
                }
            }
            available.store(false, Ordering::Release);
            for _ in 0..5 {
                if tx.is_cancelled() {
                    break;
                }
                tx.source().wait(Duration::from_millis(50));
            }
        }
    })?;
    Ok(CaptureHandle {
        publication,
        join: Some(join),
    })
}

fn capture_loop(
    monitor_index: u32,
    target_fps: Arc<AtomicU32>,
    tx: &CapturePublisher,
    desktop: Option<&std::sync::atomic::AtomicBool>,
    available: Option<&std::sync::atomic::AtomicBool>,
    start: Instant,
) -> Result<(), CaptureError> {
    #[cfg(windows)]
    let _desktop = bind_input_desktop()?;
    let monitor = Monitor::from_index(monitor_index)
        .map_err(e2s)
        .map_err(CaptureError::Create)?;
    let mut grabber = open_grabber(&monitor)?;
    // 只保留一个可复用候选帧；消费者仍持有时绝不改写其像素。
    let mut published = false;
    // 显示器尺寸只在开流时查一次：循环内每帧查询是每帧一次系统调用。
    // 分辨率变化不主动轮询，靠下方抓帧失败时重新取尺寸来感知并更新。
    let (mut mw, mut mh) = monitor.size().map_err(e2s).map_err(CaptureError::Grab)?;
    let source_name = monitor
        .device_name()
        .map_err(e2s)
        .map_err(CaptureError::Grab)?;
    let mut buffer = CaptureBuffer::default();
    let mut pacer = FramePacer::default();

    loop {
        if tx.is_cancelled() {
            return Ok(());
        }
        if desktop.is_some_and(|d| !d.load(Ordering::Acquire))
            || (published && available.is_some_and(|a| !a.load(Ordering::Acquire)))
        {
            return Err(CaptureError::Grab("普通桌面不可访问".into()));
        }
        let t_frame_start = Instant::now();
        #[cfg(target_os = "macos")]
        if monitor.size().map_err(e2s).map_err(CaptureError::Grab)? != (mw, mh) {
            return Err(CaptureError::Grab("显示模式已变化，重新创建采集流".into()));
        }

        // 缓冲区按显示器当前尺寸分配（分辨率变化后自动扩容重试）
        let need = mw as usize * mh as usize * 4;
        let mut buf = buffer.pixels(need.max(1));

        let grabbed =
            grabber.next_frame_region_format(&mut buf, Option::<Region>::None, PixelFormat::BGRA);
        let (len, w, h) = match grabbed {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // WGC has not produced its first frame yet. Its bounded wait
                // lets cancellation work even before a desktop frame arrives.
                continue;
            }
            Err(e) => {
                // 尺寸不足是常见失败原因：重新取尺寸，与当前不同就更新后重试一次
                let msg = e2s(&e);
                let (nw, nh) = monitor.size().map_err(e2s).map_err(CaptureError::Grab)?;
                if (nw, nh) != (mw, mh) {
                    mw = nw;
                    mh = nh;
                    continue;
                }
                return Err(CaptureError::Grab(msg));
            }
        };
        let capture_ms = t_frame_start.elapsed().as_millis() as u32;
        if desktop.is_some_and(|d| !d.load(Ordering::Acquire))
            || (published && available.is_some_and(|a| !a.load(Ordering::Acquire)))
        {
            return Err(CaptureError::Grab("普通桌面不可访问".into()));
        }
        if let Some(available) = available {
            available.store(true, Ordering::Release);
        }
        published = true;

        // 移交抓取缓冲，所有订阅只共享 Arc，不复制像素。
        buf.truncate(len);
        let frame = buffer.share(CapturedFrame {
            source_monitor: Some(MonitorInfo {
                index: monitor_index,
                width: w,
                height: h,
                name: source_name.clone(),
                hmonitor: monitor_handle(&monitor),
                primary: false,
                orientation: monitor_orientation(&source_name),
            }),
            desktop_generation: 0,
            captured_at: t_frame_start,
            width: w,
            height: h,
            bgra: buf,
            capture_ms,
            pts_ms: start.elapsed().as_millis() as u64,
        });
        // 直接限帧、覆盖各订阅的旧帧并唤醒；不等待编码或网络。
        if !tx.send_shared(frame) {
            return Ok(());
        }

        let fps = target_fps.load(Ordering::Relaxed);
        pacer.wait(tx, fps, t_frame_start);
    }
}

/// 测试用合成帧源：滚动彩条图案，不依赖真实屏幕状态。
/// E2E 在无人值守/静止桌面上运行时（WGC 无新帧），用它驱动完整编码推流链路。
#[doc(hidden)]
fn spawn_capture_synthetic(
    width: u32,
    height: u32,
    tx: CapturePublisher,
) -> Result<CaptureHandle, CaptureError> {
    let publication = tx.source();
    let target_fps = publication.target_fps.clone();
    let thread_fps = target_fps.clone();
    let join = spawn_capture_thread("capture-synthetic".into(), move || {
        let start = Instant::now();
        let mut tick: u32 = 0;
        loop {
            if tx.is_cancelled() {
                return;
            }
            let t_frame_start = Instant::now();
            tick = tick.wrapping_add(3);
            let w = width as usize;
            let h = height as usize;
            // 合成测试源按帧分配，不保留采集缓冲池。
            let mut bgra = vec![0u8; w * h * 4];
            for row in 0..h {
                for col in 0..w {
                    let p = (row * w + col) * 4;
                    bgra[p] = ((col + tick as usize) & 0xFF) as u8; // B
                    bgra[p + 1] = ((row + tick as usize / 2) & 0xFF) as u8; // G
                    bgra[p + 2] = ((col ^ row) & 0xFF) as u8; // R
                    bgra[p + 3] = 255;
                }
            }
            let frame = CapturedFrame {
                source_monitor: Some(synthetic_monitor(0, width, height)),
                desktop_generation: 0,
                captured_at: Instant::now(),
                width,
                height,
                bgra,
                capture_ms: 1,
                pts_ms: start.elapsed().as_millis() as u64,
            };
            // 与真实采集共用直接发布路径。
            if !tx.send(frame) {
                return;
            }
            let frame_interval = Duration::from_nanos(
                1_000_000_000 / thread_fps.load(Ordering::Relaxed).max(1) as u64,
            );
            if let Some(remain) = frame_interval.checked_sub(t_frame_start.elapsed()) {
                tx.source().wait(remain);
            }
        }
    })?;
    Ok(CaptureHandle {
        publication,
        join: Some(join),
    })
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn capture_regions_reject_coordinate_overflow_and_out_of_bounds_edges() {
        for region in [
            Region::new(u32::MAX, 0, 2, 2),
            Region::new(0, u32::MAX, 2, 2),
            Region::new(1919, 0, 2, 2),
            Region::new(0, 1079, 2, 2),
            Region::new(0, 0, 0, 2),
        ] {
            assert!(region.check(1920, 1080).is_err());
        }
        assert!(Region::new(1918, 1078, 2, 2).check(1920, 1080).is_ok());
    }

    #[test]
    fn panicked_capture_thread_does_not_wait_for_the_stop_timeout() {
        let join = spawn_capture_thread("capture-panic-test".into(), || {
            panic!("simulated capture backend failure");
        })
        .unwrap();
        while !join.is_finished() {
            std::thread::yield_now();
        }
        let handle = CaptureHandle {
            publication: Publication::new(),
            join: Some(join),
        };
        let started = Instant::now();
        drop(handle);
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
mod cursor;
pub use cursor::{CursorObserver, CursorSample};
