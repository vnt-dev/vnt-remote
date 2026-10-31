//! Control state for a persistent, single-producer desktop source.
use crate::{
    CaptureError, CaptureHandle, CapturePublisher, CapturedFrame, MonitorInfo, Publication,
};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct DesktopSnapshot {
    pub generation: u64,
    pub desktop: Option<String>,
    pub monitors: Vec<MonitorInfo>,
    pub monitor: Option<MonitorInfo>,
    pub capture_ready: bool,
    pub changed_at: Instant,
    pub error: String,
}
struct State {
    source_generation: u64,
    snapshot: DesktopSnapshot,
    source: Weak<Publication>,
    last_frame_at: Option<Instant>,
}
pub struct DesktopCapture {
    configured_monitor: u32,
    configured_name: Option<String>,
    region: Option<crate::ScreenRegion>,
    base: Option<Arc<DesktopCapture>>,
    state: Mutex<State>,
    restart: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicBool,
}
impl DesktopCapture {
    pub fn new(configured_monitor: u32, desktop: Option<String>) -> Arc<Self> {
        Self::with_region(configured_monitor, desktop, None)
    }
    pub fn named(
        configured_monitor: u32,
        name: Option<String>,
        desktop: Option<String>,
    ) -> Arc<Self> {
        let mut capture = Self::new(configured_monitor, desktop);
        Arc::get_mut(&mut capture).unwrap().configured_name = name;
        capture
    }
    pub fn with_region(
        configured_monitor: u32,
        desktop: Option<String>,
        region: Option<crate::ScreenRegion>,
    ) -> Arc<Self> {
        Arc::new(Self {
            configured_name: None,
            region,
            base: None,
            configured_monitor,
            restart: false.into(),
            active: false.into(),
            state: Mutex::new(State {
                source_generation: 0,
                source: Weak::new(),
                last_frame_at: None,
                snapshot: DesktopSnapshot {
                    generation: 1,
                    desktop,
                    monitors: Vec::new(),
                    monitor: None,
                    capture_ready: false,
                    changed_at: Instant::now(),
                    error: String::new(),
                },
            }),
        })
    }
    /// A policy view shares the producer and pixels, but owns its availability epoch.
    pub fn region_view(base: Arc<Self>, region: crate::ScreenRegion) -> Arc<Self> {
        let mut view = Self::with_region(
            base.configured_monitor,
            base.snapshot().desktop,
            Some(region),
        );
        Arc::get_mut(&mut view).unwrap().base = Some(base);
        view
    }
    fn refresh_region(&self) {
        let Some(base) = &self.base else {
            return;
        };
        let raw = base.snapshot();
        let region = self.region.as_ref().unwrap();
        let normal = raw
            .desktop
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case("default"));
        let valid = raw
            .monitor
            .as_ref()
            .and_then(|m| live_monitor(m).ok())
            .is_some_and(|m| region.matches(&m));
        let ready = raw.capture_ready && normal && valid;
        let mut state = self.state.lock().unwrap();
        if state.source_generation != raw.generation
            || state.snapshot.capture_ready != ready
            || state.snapshot.desktop != raw.desktop
        {
            state.snapshot.generation += 1;
            state.snapshot.changed_at = if ready {
                state.snapshot.changed_at.max(raw.changed_at)
            } else {
                Instant::now()
            };
        }
        state.source_generation = raw.generation;
        state.snapshot.desktop = raw.desktop;
        state.snapshot.monitors = raw.monitors;
        state.snapshot.monitor = raw.monitor;
        state.snapshot.capture_ready = ready;
        state.snapshot.error = if ready {
            String::new()
        } else {
            "屏幕区域暂不可用，已暂停画面与输入，恢复原桌面和显示模式后自动继续".into()
        };
    }
    pub fn frame_generation(&self, frame: &CapturedFrame) -> Option<u64> {
        let snapshot = self.snapshot();
        if let Some(base) = &self.base {
            (snapshot.capture_ready
                && base.accepts(frame.desktop_generation)
                && frame.captured_at >= snapshot.changed_at
                && frame
                    .source_monitor
                    .as_ref()
                    .is_some_and(|m| self.region.as_ref().unwrap().matches(m)))
            .then_some(snapshot.generation)
        } else {
            self.accepts(frame.desktop_generation)
                .then_some(frame.desktop_generation)
        }
    }
    pub fn snapshot(&self) -> DesktopSnapshot {
        self.refresh_region();
        self.state.lock().unwrap().snapshot.clone()
    }
    pub fn request_restart(&self) {
        if let Some(base) = &self.base {
            base.request_restart();
            return;
        }
        self.restart
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn restart_requested(&self) -> bool {
        self.restart.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn accepts(&self, generation: u64) -> bool {
        self.refresh_region();
        let state = self.state.lock().unwrap();
        state.snapshot.generation == generation && state.snapshot.capture_ready
    }
    /// A successful first frame does not hide a later native driver hang.
    pub fn recovery_overdue(&self, now: Instant) -> bool {
        let state = self.state.lock().unwrap();
        // Invalid regions and secure desktops intentionally wait for the original target.
        if self.region.is_some() && !state.snapshot.capture_ready {
            return false;
        }
        if !state.source.upgrade().is_some_and(|s| !s.is_stopped()) {
            return false;
        }
        let since = state.last_frame_at.unwrap_or(state.snapshot.changed_at);
        now.saturating_duration_since(since) >= Duration::from_secs(10)
    }

    pub fn has_subscribers(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .source
            .upgrade()
            .is_some_and(|s| !s.is_stopped())
    }
    pub fn set_desktop(&self, desktop: Option<String>) {
        let retired = {
            let mut state = self.state.lock().unwrap();
            if state.snapshot.desktop == desktop {
                return;
            }
            state.snapshot.generation += 1;
            state.snapshot.desktop = desktop;
            state.snapshot.capture_ready = false;
            state.last_frame_at = None;
            state.snapshot.monitor = None;
            state.snapshot.monitors.clear();
            state.snapshot.changed_at = Instant::now();
            state.snapshot.error.clear();
            state
                .source
                .upgrade()
                .map(|s| s.invalidate(state.snapshot.generation))
        };
        drop(retired);
    }
    fn attach(&self, source: &Arc<Publication>) {
        let retired = {
            let mut state = self.state.lock().unwrap();
            state.source = Arc::downgrade(source);
            state.snapshot.generation += 1;
            state.snapshot.capture_ready = false;
            state.last_frame_at = None;
            state.snapshot.changed_at = Instant::now();
            source.invalidate(state.snapshot.generation)
        };
        drop(retired);
    }
    fn failed(&self, generation: u64, error: &str) {
        let retired = {
            let mut state = self.state.lock().unwrap();
            if state.snapshot.generation != generation {
                return;
            }
            let retired = if state.snapshot.capture_ready {
                state.snapshot.generation += 1;
                state.snapshot.changed_at = Instant::now();
                state
                    .source
                    .upgrade()
                    .map(|s| s.invalidate(state.snapshot.generation))
            } else {
                None
            };
            state.snapshot.capture_ready = false;
            state.last_frame_at = None;
            state.snapshot.error = error.to_owned();
            retired
        };
        drop(retired);
    }
    fn submit(
        &self,
        target: &DesktopSnapshot,
        monitors: &[MonitorInfo],
        selected: &MonitorInfo,
        frame: Arc<CapturedFrame>,
        tx: &CapturePublisher,
    ) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.snapshot.generation != target.generation {
                return false;
            }
            if !state.snapshot.capture_ready {
                state.snapshot.monitors = monitors.to_vec();
                state.snapshot.monitor = Some(selected.clone());
                log::info!(
                    "desktop {} capture restored in {:?}",
                    target.generation,
                    target.changed_at.elapsed()
                );
            }
            state.snapshot.capture_ready = true;
            state.last_frame_at = Some(Instant::now());
            state.snapshot.error.clear();
        }
        // Publication checks the epoch again under its registry lock. No pixel destruction under state locks.
        tx.send_shared(frame)
    }
}

pub(crate) fn spawn(
    desktop: Arc<DesktopCapture>,
    tx: CapturePublisher,
    synthetic: Option<(u32, u32)>,
) -> Result<CaptureHandle, CaptureError> {
    if desktop.restart_requested()
        || desktop
            .active
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        desktop.request_restart();
        return Err(CaptureError::Stopped);
    }
    let active = ActiveCapture(desktop.clone());
    let publication = tx.source();
    desktop.attach(&publication);
    let source = publication.clone();
    let join = crate::spawn_capture_thread("service-capture".into(), move || {
        let _active = active;
        let mut producer = Some(tx);
        let start = Instant::now();
        let mut generation = 0;
        let mut failures = 0usize;
        let mut pacer = crate::FramePacer::default();
        while !source.is_stopped() {
            let target = desktop.snapshot();
            if generation != target.generation {
                generation = target.generation;
                failures = 0;
            }
            if target.desktop.is_some() {
                if let Some((width, height)) = synthetic {
                    let monitor = MonitorInfo {
                        index: desktop.configured_monitor,
                        width,
                        height,
                        name: "Synthetic".into(),
                        primary: true,
                        hmonitor: 0,
                        orientation: 0,
                    };
                    let frame = CapturedFrame {
                        source_monitor: Some(crate::synthetic_monitor(
                            desktop.configured_monitor,
                            width,
                            height,
                        )),
                        desktop_generation: generation,
                        captured_at: Instant::now(),
                        width,
                        height,
                        bgra: vec![generation as u8; width as usize * height as usize * 4],
                        pts_ms: start.elapsed().as_millis() as u64,
                        capture_ms: 0,
                    };
                    desktop.submit(
                        &target,
                        std::slice::from_ref(&monitor),
                        &monitor,
                        Arc::new(frame),
                        producer.as_ref().unwrap(),
                    );
                    pace(producer.as_ref().unwrap(), &mut pacer, Instant::now());
                    continue;
                }
                // Transfer the unique publisher to one native worker, then reclaim it
                // only after that worker has exited. No intermediate frame queue.
                // WGC can retain dispatcher windows; a fresh windowless thread is
                // the bounded fallback for SetThreadDesktop / ERROR_BUSY.
                let owned = producer.take().unwrap();
                let worker_desktop = desktop.clone();
                let worker = std::thread::Builder::new()
                    .name(format!("desktop-capture-{generation}"))
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            capture(&worker_desktop, &target, &owned, start)
                        }));
                        (owned, result)
                    });
                let outcome = match worker {
                    Ok(worker) => worker.join(),
                    Err(error) => {
                        desktop.failed(generation, &error.to_string());
                        desktop.request_restart();
                        return;
                    }
                };
                crate::reap_desktops();
                match outcome {
                    Ok((owned, Ok(result))) => {
                        producer = Some(owned);
                        if let Err(error) = result {
                            desktop.failed(generation, &error.to_string());
                        }
                    }
                    _ => {
                        desktop.request_restart();
                        return;
                    }
                }
            }
            source.wait(Duration::from_millis([50, 100, 250, 500][failures.min(3)]));
            failures = failures.saturating_add(1);
        }
    })?;
    Ok(CaptureHandle {
        publication,
        join: Some(join),
    })
}
struct ActiveCapture(Arc<DesktopCapture>);
impl Drop for ActiveCapture {
    fn drop(&mut self) {
        self.0
            .active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

fn pace(tx: &CapturePublisher, pacer: &mut crate::FramePacer, started: Instant) {
    let fps = tx
        .source()
        .target_fps
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1);
    pacer.wait(tx, fps, started);
}
fn capture(
    desktop: &DesktopCapture,
    target: &DesktopSnapshot,
    tx: &CapturePublisher,
    start: Instant,
) -> Result<(), CaptureError> {
    #[cfg(windows)]
    let _binding = crate::bind_input_desktop()?;
    if input_name().as_ref() != target.desktop.as_ref() {
        return Ok(());
    }
    let monitors = crate::list_monitors()?;
    let secure = !target
        .desktop
        .as_deref()
        .is_some_and(|n| n.eq_ignore_ascii_case("default"));
    if desktop.region.is_some() && secure {
        return Err(CaptureError::Create("区域共享在锁屏或安全桌面暂停".into()));
    }
    let selected = if let Some(region) = &desktop.region {
        crate::require_region(region, &monitors)?.clone()
    } else if let Some(name) = desktop.configured_name.as_ref().filter(|_| !secure) {
        monitors
            .iter()
            .find(|m| &m.name == name)
            .ok_or(CaptureError::NoMonitor)?
            .clone()
    } else {
        monitors
            .iter()
            .find(|m| !secure && m.index == desktop.configured_monitor)
            .or_else(|| monitors.iter().find(|m| m.primary))
            .or(monitors.first())
            .ok_or(CaptureError::NoMonitor)?
            .clone()
    };
    let monitor = crate::Monitor::from_index(selected.index)
        .map_err(|e| CaptureError::Create(e.to_string()))?;
    let mut grabber = crate::open_grabber_for(&monitor, secure)?;
    let (w, h) = monitor
        .size()
        .map_err(|e| CaptureError::Grab(e.to_string()))?;
    let mut buffer = crate::CaptureBuffer::default();
    let mut pacer = crate::FramePacer::default();
    loop {
        if tx.is_cancelled()
            || desktop.snapshot().generation != target.generation
            || input_name().as_ref() != target.desktop.as_ref()
        {
            return Ok(());
        }
        if let Some(region) = &desktop.region {
            if !region.matches(&live_monitor(&selected)?) {
                return Err(CaptureError::Create(
                    "区域显示模式已变化，已暂停共享".into(),
                ));
            }
        }
        let captured_at = Instant::now();
        let mut bgra = buffer.pixels(w as usize * h as usize * 4);
        let (len, width, height) =
            match grabber.next_frame_region_format(&mut bgra, None, screencap::PixelFormat::BGRA) {
                Ok(value) => value,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(CaptureError::Grab(e.to_string())),
            };
        if input_name().as_ref() != target.desktop.as_ref() {
            return Ok(());
        }
        bgra.truncate(len);
        let frame = buffer.share(CapturedFrame {
            source_monitor: Some(live_monitor(&selected)?),
            desktop_generation: target.generation,
            captured_at,
            width,
            height,
            bgra,
            pts_ms: start.elapsed().as_millis() as u64,
            capture_ms: captured_at.elapsed().as_millis() as u32,
        });
        if !desktop.submit(target, &monitors, &selected, frame, tx) {
            return Ok(());
        }
        pace(tx, &mut pacer, captured_at);
    }
}
#[cfg(windows)]
pub(super) fn input_name() -> Option<String> {
    use windows::Win32::{Foundation::HANDLE, System::StationsAndDesktops::*};
    unsafe {
        let h = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
        let mut name = [0u16; 256];
        let result = GetUserObjectInformationW(
            HANDLE(h.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            512,
            None,
        );
        let _ = CloseDesktop(h);
        result.ok()?;
        Some(String::from_utf16_lossy(
            &name[..name.iter().position(|&v| v == 0).unwrap_or(name.len())],
        ))
    }
}
#[cfg(not(windows))]
fn input_name() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn live_monitor(selected: &MonitorInfo) -> Result<MonitorInfo, CaptureError> {
    #[cfg(test)]
    if selected.name == "Synthetic" {
        return Ok(selected.clone());
    }
    crate::list_monitors()?
        .into_iter()
        .find(|m| m.name == selected.name)
        .ok_or(CaptureError::NoMonitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedCapture;
    #[test]
    fn switching_preserves_receivers_and_discards_pixels_without_charging_drops() {
        let d = DesktopCapture::new(0, Some("Default".into()));
        let source = SharedCapture::switching_synthetic(d.clone(), 16, 16);
        let mut fast = source.subscribe(120).unwrap();
        let mut slow = source.subscribe(1).unwrap();
        let before = fast.recv().unwrap().desktop_generation;
        d.set_desktop(None);
        let dropped = slow.dropped_frames();
        assert!(!d.accepts(before));
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || tx.send(slow.recv()).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        d.set_desktop(Some("Winlogon".into()));
        let restored = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert!(restored.desktop_generation > before);
        assert!(d.accepts(restored.desktop_generation));
        assert_eq!(dropped, 0);
        waiter.join().unwrap();
        source.cancel();
        assert!(fast.recv().is_none());
    }
    #[test]
    fn rapid_changes_and_old_cancel_handles_cannot_resurrect_old_epochs() {
        let d = DesktopCapture::new(0, Some("Default".into()));
        let source = SharedCapture::switching_synthetic(d.clone(), 16, 16);
        let mut sub = source.subscribe(120).unwrap();
        let old = sub.recv().unwrap().desktop_generation;
        for name in ["Winlogon", "Default", "Winlogon", "Default"] {
            d.set_desktop(Some(name.into()));
        }
        d.failed(old, "late failure");
        let expected = d.snapshot().generation;
        assert_eq!(sub.recv().unwrap().desktop_generation, expected);
        assert!(d.snapshot().error.is_empty());
        let cancel = sub.cancel_handle();
        cancel.cancel();
        let mut new = source.subscribe(120).unwrap();
        cancel.cancel();
        assert!(new.recv().unwrap().desktop_generation > expected);
        source.cancel();
    }
    #[test]
    fn failure_invalidates_epoch_without_extending_retry_deadline() {
        let d = DesktopCapture::new(0, Some("Default".into()));
        let p = Publication::new();
        let _sub = p.subscribe(1, 30).unwrap();
        d.attach(&p);
        let old = d.snapshot().generation;
        d.state.lock().unwrap().snapshot.capture_ready = true;
        d.failed(old, "access lost");
        let failed = d.snapshot();
        assert!(failed.generation > old);
        assert!(!failed.capture_ready);
        d.failed(failed.generation, "retry");
        assert_eq!(d.snapshot().changed_at, failed.changed_at);
        assert!(!p.publish(Arc::new(CapturedFrame {
            source_monitor: None,
            desktop_generation: old,
            captured_at: Instant::now(),
            width: 1,
            height: 1,
            bgra: vec![0; 4],
            pts_ms: 1,
            capture_ms: 0
        })));
    }
    #[cfg(windows)]
    #[test]
    fn native_backend_reopens_without_ending_subscription() {
        crate::enable_service_desktop(false);
        let name = input_name().expect("interactive desktop");
        let monitors = crate::list_monitors().unwrap();
        let index = monitors
            .iter()
            .find(|m| m.primary)
            .unwrap_or(&monitors[0])
            .index;
        let d = DesktopCapture::new(index, Some(name.clone()));
        let source = SharedCapture::switching(d.clone());
        let mut sub = source.subscribe(30).unwrap();
        let cancel = sub.cancel_handle();
        let timeout = std::thread::spawn(move || {
            for _ in 0..150 {
                if cancel.is_cancelled() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            cancel.cancel();
        });
        assert!(sub.recv().is_some());
        for _ in 0..8 {
            d.set_desktop(None);
            d.set_desktop(Some(name.clone()));
            let expected = d.snapshot().generation;
            assert_eq!(
                sub.recv().expect("restored frame").desktop_generation,
                expected
            );
        }
        source.cancel();
        drop(sub);
        timeout.join().unwrap();
        crate::reap_desktops();
        assert!(
            crate::DEFERRED_DESKTOPS.lock().unwrap().is_empty(),
            "desktop handles must be reclaimed after native thread exit"
        );
    }
    #[test]
    fn a_hung_backend_after_first_frame_requests_recovery_but_idle_sources_do_not() {
        let d = DesktopCapture::new(0, Some("Default".into()));
        let source = Publication::new();
        let sub = source.subscribe(1, 30).unwrap();
        d.attach(&source);
        let now = Instant::now();
        {
            let mut state = d.state.lock().unwrap();
            state.snapshot.capture_ready = true;
            state.last_frame_at = Some(now - Duration::from_secs(11));
        }
        assert!(d.recovery_overdue(now));
        sub.cancel();
        assert!(!d.recovery_overdue(now));
    }
}

#[cfg(windows)]
fn live_monitor(selected: &MonitorInfo) -> Result<MonitorInfo, CaptureError> {
    #[cfg(test)]
    if selected.hmonitor == 0 && selected.name == "Synthetic" {
        return Ok(selected.clone());
    }
    use windows::Win32::Graphics::Gdi::*;
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if !unsafe { GetMonitorInfoW(HMONITOR(selected.hmonitor as *mut _), &mut info.monitorInfo) }
        .as_bool()
    {
        return Err(CaptureError::NoMonitor);
    }
    let mut current = selected.clone();
    let rect = info.monitorInfo.rcMonitor;
    current.width = (rect.right - rect.left) as u32;
    current.height = (rect.bottom - rect.top) as u32;
    current.name = String::from_utf16_lossy(
        &info.szDevice[..info
            .szDevice
            .iter()
            .position(|&v| v == 0)
            .unwrap_or(info.szDevice.len())],
    );
    // Display modes are physical pixels even for a service without a DPI manifest.
    let (width, height, orientation) =
        crate::region::monitor_mode(&current.name).ok_or(CaptureError::NoMonitor)?;
    current.width = width;
    current.height = height;
    current.orientation = orientation;
    Ok(current)
}

#[cfg(test)]
mod region_view_tests {
    use super::*;
    #[test]
    fn region_epochs_pause_independently_and_reject_old_or_foreign_pixels() {
        let base = DesktopCapture::new(0, Some("Default".into()));
        let monitor = crate::synthetic_monitor(0, 1280, 720);
        {
            let mut s = base.state.lock().unwrap();
            s.snapshot.monitor = Some(monitor.clone());
            s.snapshot.capture_ready = true;
        }
        let region = crate::ScreenRegion {
            id: "r".into(),
            name: "region".into(),
            monitor_name: "Synthetic".into(),
            screen_width: 1280,
            screen_height: 720,
            orientation: 0,
            x: 100,
            y: 100,
            width: 320,
            height: 240,
        };
        let view = DesktopCapture::region_view(base.clone(), region);
        let mut frame = CapturedFrame {
            source_monitor: Some(monitor),
            desktop_generation: base.snapshot().generation,
            captured_at: Instant::now(),
            width: 1280,
            height: 720,
            bgra: Vec::new(),
            pts_ms: 0,
            capture_ms: 0,
        };
        let first = view.frame_generation(&frame).unwrap();
        {
            let mut s = base.state.lock().unwrap();
            s.snapshot.monitor.as_mut().unwrap().orientation = 2;
        }
        assert!(!view.accepts(first));
        assert!(base.snapshot().capture_ready);
        assert!(view.frame_generation(&frame).is_none());
        {
            let mut s = base.state.lock().unwrap();
            s.snapshot.monitor.as_mut().unwrap().orientation = 0;
        }
        assert!(
            view.frame_generation(&frame).is_none(),
            "pixels from before pause must be discarded"
        );
        frame.captured_at = Instant::now();
        let restored = view.frame_generation(&frame).unwrap();
        assert!(restored > first);
        base.set_desktop(Some("Winlogon".into()));
        assert!(!view.accepts(restored));
        base.set_desktop(Some("Default".into()));
        {
            let mut s = base.state.lock().unwrap();
            s.snapshot.capture_ready = true;
            s.snapshot.monitor = frame.source_monitor.clone();
        }
        assert!(
            view.frame_generation(&frame).is_none(),
            "old raw-source epochs cannot return after unlock"
        );
    }
}
