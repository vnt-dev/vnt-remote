//! In-process user desktop host. No SCM, agent processes or desktop attachment.
use super::*;

pub(super) struct Portable {
    #[cfg(test)]
    pub probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    pub path: std::path::PathBuf,
    pub region: Option<(vnt_capture::ScreenRegion, u32)>,
    base: Option<Arc<Portable>>,
    pub captures: std::sync::Mutex<HashMap<u32, SharedCapture>>,
    pub monitors: std::sync::Mutex<HashMap<u32, Arc<Portable>>>,
    pub desktop: Arc<AtomicBool>,
    pub capture_available: Arc<AtomicBool>,
    pub ready: tokio::sync::watch::Sender<bool>,
    pub generation: AtomicU32,
    pub invalid_before: std::sync::Mutex<Instant>,
}

impl Portable {
    pub fn new(path: std::path::PathBuf) -> Arc<Self> {
        let (ready, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            #[cfg(test)]
            probe: None,
            path,
            region: None,
            base: None,
            captures: Default::default(),
            monitors: Default::default(),
            desktop: Arc::new(AtomicBool::new(normal_desktop())),
            capture_available: Arc::new(AtomicBool::new(false)),
            ready,
            generation: AtomicU32::new(1),
            invalid_before: std::sync::Mutex::new(Instant::now()),
        })
    }

    pub fn for_region(base: Arc<Self>, region: vnt_capture::ScreenRegion, index: u32) -> Arc<Self> {
        let (ready, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            #[cfg(test)]
            probe: base.probe.clone(),
            path: base.path.clone(),
            region: Some((region, index)),
            desktop: base.desktop.clone(),
            capture_available: base.capture_available.clone(),
            base: Some(base),
            captures: Default::default(),
            monitors: Default::default(),
            ready,
            generation: AtomicU32::new(1),
            invalid_before: std::sync::Mutex::new(Instant::now()),
        })
    }
    pub fn region_valid(&self) -> bool {
        self.region.as_ref().is_none_or(|(r, index)| {
            #[cfg(test)]
            if self.probe.is_some() && r.monitor_name == "Synthetic" {
                return r.matches(&vnt_capture::synthetic_monitor(*index, 1280, 720));
            }
            vnt_capture::list_monitors()
                .ok()
                .is_some_and(|monitors| r.resolve(&monitors).is_ok_and(|m| m.index == *index))
        })
    }
    pub fn available(&self) -> bool {
        *self.ready.borrow()
            && self.desktop.load(Ordering::Acquire)
            && self.capture_available.load(Ordering::Acquire)
    }

    pub fn normal_desktop(&self) -> bool {
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            return probe();
        }
        normal_desktop()
    }

    pub fn state(&self, ready: bool, min_pts_ms: u32, reason: &str) -> ControlMessage {
        ControlMessage::DesktopState(pb::DesktopState {
            ready,
            desktop: "Default".into(),
            reason: reason.into(),
            generation: self.generation.load(Ordering::Acquire),
            min_pts_ms,
        })
    }

    pub async fn watch(
        self: Arc<Self>,
        clipboard: Option<Arc<crate::clipboard::ClipboardService>>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut timer = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = timer.tick() => {},
                _ = stop.changed() => break,
            }
            if *stop.borrow() {
                break;
            }
            let desktop = self.normal_desktop();
            // Desktop access is global; capture readiness is per monitor.
            if let Some(clipboard) = &clipboard {
                clipboard.set_enabled(desktop);
            }
            self.desktop.store(desktop, Ordering::Release);
            if !desktop {
                self.capture_available.store(false, Ordering::Release);
            }
            let ready = desktop
                && self.capture_available.load(Ordering::Acquire)
                && self.base.as_ref().is_none_or(|p| p.available())
                && self.region_valid();
            let previous = *self.ready.borrow();
            if ready != previous {
                *self.invalid_before.lock().unwrap() = Instant::now();
                self.generation.fetch_add(1, Ordering::AcqRel);
                self.ready.send_replace(ready);
            }
        }
    }
}

pub(super) fn normal_desktop() -> bool {
    #[cfg(windows)]
    return crate::windows::native::desktop_name()
        .is_ok_and(|name| name.eq_ignore_ascii_case("default"));
    #[cfg(target_os = "macos")]
    vnt_macos::query(9)
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

impl Drop for Portable {
    fn drop(&mut self) {
        for capture in self.captures.lock().unwrap().values() {
            capture.cancel();
        }
    }
}
