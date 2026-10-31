//! Lazily started capture sources with direct, independently paced subscriptions.

use std::sync::{Arc, Condvar, Mutex};

use crate::publication::{CapturePublisher, Publication};
use crate::{CaptureCancelHandle, CaptureError, CaptureHandle, CapturedFrame};

#[derive(Clone)]
enum SourceKind {
    Monitor(u32),
    Switching(Arc<crate::DesktopCapture>),
    SwitchingSynthetic(Arc<crate::DesktopCapture>, u32, u32),
    ResilientMonitor(
        u32,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicBool>,
    ),
    Synthetic(u32, u32),
}

/// Clone across sessions to share a capture thread. Capture starts on subscription.
#[derive(Clone)]
pub struct SharedCapture {
    manager: Arc<Lifecycle>,
}

struct Manager {
    kind: SourceKind,
    source: Option<Source>,
    next_id: u64,
    stopped: bool,
    retiring: Option<Arc<Publication>>,
}

struct Lifecycle {
    state: Mutex<Manager>,
    changed: Condvar,
}

impl Lifecycle {
    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Manager>> {
        self.state.lock()
    }

    fn retire(self: &Arc<Self>, source: Source) {
        // Native capture teardown and the bounded join run without any mutex.
        // New registrations wait for FinishRetirement to reopen admission.
        let _finished = FinishRetirement(self.clone());
        drop(source);
    }
}

struct FinishRetirement(Arc<Lifecycle>);

impl Drop for FinishRetirement {
    fn drop(&mut self) {
        let retired = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retiring
            .take();
        self.0.changed.notify_all();
        drop(retired);
    }
}

struct Source {
    _capture: CaptureHandle,
    publication: Arc<Publication>,
}

impl Source {
    fn start(kind: SourceKind, publication: Arc<Publication>) -> Result<Self, CaptureError> {
        let publisher = CapturePublisher(publication.clone());
        let capture = match kind {
            SourceKind::Switching(d) => crate::desktop::spawn(d, publisher, None)?,
            SourceKind::SwitchingSynthetic(d, w, h) => {
                crate::desktop::spawn(d, publisher, Some((w, h)))?
            }
            SourceKind::Monitor(index) => crate::spawn_capture(index, publisher)?,
            SourceKind::ResilientMonitor(index, desktop, available) => {
                crate::spawn_resilient_capture(index, publisher, desktop, available)?
            }
            SourceKind::Synthetic(w, h) => crate::spawn_capture_synthetic(w, h, publisher)?,
        };
        Ok(Self {
            _capture: capture,
            publication,
        })
    }
}

impl SharedCapture {
    /// Whether these handles publish from the same native capture producer.
    pub fn shares_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.manager, &other.manager)
    }

    pub fn switching(desktop: Arc<crate::DesktopCapture>) -> Self {
        Self::new(SourceKind::Switching(desktop))
    }
    #[doc(hidden)]
    pub fn switching_synthetic(d: Arc<crate::DesktopCapture>, w: u32, h: u32) -> Self {
        Self::new(SourceKind::SwitchingSynthetic(d, w, h))
    }
    pub fn monitor(index: u32) -> Self {
        Self::new(SourceKind::Monitor(index))
    }

    /// Keep subscriptions alive while the portable host's desktop is inaccessible.
    pub fn resilient_monitor(
        index: u32,
        desktop: Arc<std::sync::atomic::AtomicBool>,
        available: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self::new(SourceKind::ResilientMonitor(index, desktop, available))
    }

    #[doc(hidden)]
    pub fn synthetic(width: u32, height: u32) -> Self {
        Self::new(SourceKind::Synthetic(width, height))
    }

    fn new(kind: SourceKind) -> Self {
        Self {
            manager: Arc::new(Lifecycle {
                state: Mutex::new(Manager {
                    kind,
                    source: None,
                    next_id: 0,
                    stopped: false,
                    retiring: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// Stop publication and wake every subscriber without joining capture.
    /// This permanently prevents new subscriptions on this handle.
    pub fn cancel(&self) {
        let publication = {
            let mut manager = self.manager.lock().unwrap();
            manager.stopped = true;
            manager
                .source
                .as_ref()
                .map(|s| s.publication.clone())
                .or_else(|| manager.retiring.clone())
        };
        self.manager.changed.notify_all();
        if let Some(publication) = publication {
            publication.cancel();
        }
    }

    pub fn subscribe(&self, fps: u32) -> Result<CaptureSubscription, CaptureError> {
        if !(1..=120).contains(&fps) {
            return Err(CaptureError::Create("共享抓屏帧率须为 1–120".into()));
        }
        loop {
            let mut manager = self.manager.lock().unwrap();
            if manager.stopped {
                return Err(CaptureError::Stopped);
            }
            if manager.retiring.is_some() {
                drop(self.manager.changed.wait(manager).unwrap());
                continue;
            }
            let id = manager.next_id;
            manager.next_id += 1;
            if let Some(source) = &manager.source {
                if let Some(cancel) = source.publication.subscribe(id, fps) {
                    return Ok(CaptureSubscription {
                        manager: self.manager.clone(),
                        cancel,
                    });
                }
            }
            if let Some(source) = manager.source.take() {
                manager.retiring = Some(source.publication.clone());
                drop(manager);
                self.manager.retire(source);
                continue;
            }
            let publication = Publication::new();
            let cancel = publication.subscribe(id, fps).unwrap();
            manager.source = Some(Source::start(manager.kind.clone(), publication)?);
            return Ok(CaptureSubscription {
                manager: self.manager.clone(),
                cancel,
            });
        }
    }
}

/// Unique receiver and lease for one independently paced latest-frame slot.
/// Dropping it unregisters only this subscriber.
pub struct CaptureSubscription {
    manager: Arc<Lifecycle>,
    cancel: CaptureCancelHandle,
}

impl CaptureSubscription {
    /// Wait for a frame, cancellation or source EOF. No polling is involved.
    pub fn recv(&mut self) -> Option<Arc<CapturedFrame>> {
        self.cancel.recv()
    }

    pub fn set_fps(&self, fps: u32) -> Result<(), CaptureError> {
        self.cancel.set_fps(fps)
    }

    /// Count overwritten unread frames, excluding deliberate frame-rate skips.
    pub fn dropped_frames(&self) -> u64 {
        self.cancel.dropped_frames()
    }

    pub fn cancel_handle(&self) -> CaptureCancelHandle {
        self.cancel.clone()
    }
}

impl Drop for CaptureSubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
        let source = {
            let mut manager = self.manager.lock().unwrap();
            if manager.source.as_ref().is_some_and(|source| {
                Arc::ptr_eq(&source.publication, &self.cancel.source())
                    && source.publication.is_stopped()
            }) {
                let source = manager.source.take().unwrap();
                manager.retiring = Some(source.publication.clone());
                Some(source)
            } else {
                None
            }
        };
        if let Some(source) = source {
            self.manager.retire(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::Ordering, mpsc, Barrier};
    use std::time::{Duration, Instant};

    fn next_frame(
        mut subscription: CaptureSubscription,
    ) -> (CaptureSubscription, Option<Arc<CapturedFrame>>) {
        let cancel = subscription.cancel_handle();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let frame = subscription.recv();
            sent.send((subscription, frame)).ok();
        });
        let result = received.recv_timeout(Duration::from_secs(2));
        if result.is_err() {
            cancel.cancel();
        }
        worker.join().unwrap();
        result.expect("capture failed to wake subscriber")
    }

    fn thread_id(capture: &SharedCapture) -> std::thread::ThreadId {
        capture
            .manager
            .lock()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
            ._capture
            .join
            .as_ref()
            .unwrap()
            .thread()
            .id()
    }

    #[test]
    fn concurrent_subscriptions_start_only_one_capture() {
        let capture = SharedCapture::synthetic(16, 16);
        assert!(capture.manager.lock().unwrap().source.is_none());
        let barrier = Arc::new(Barrier::new(32));
        let threads: Vec<_> = (0..32)
            .map(|_| {
                let capture = capture.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let subscription = capture.subscribe(30).unwrap();
                    let id = thread_id(&capture);
                    (subscription, id)
                })
            })
            .collect();
        let subscriptions: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(subscriptions.iter().all(|s| s.1 == subscriptions[0].1));
        drop(subscriptions);
        assert!(capture.manager.lock().unwrap().source.is_none());
    }

    #[test]
    fn rate_changes_and_leaving_peers_preserve_the_capture_thread() {
        let capture = SharedCapture::synthetic(16, 16);
        let slow = capture.subscribe(30).unwrap();
        let fast = capture.subscribe(60).unwrap();
        let original = thread_id(&capture);
        for (rate, max) in [(20, 30), (90, 90), (10, 30)] {
            fast.set_fps(rate).unwrap();
            let manager = capture.manager.lock().unwrap();
            let source = manager.source.as_ref().unwrap();
            assert_eq!(source.publication.target_fps.load(Ordering::Relaxed), max);
        }
        assert_eq!(thread_id(&capture), original);
        let (slow, first) = next_frame(slow);
        drop(fast);
        assert_eq!(thread_id(&capture), original);
        let (slow, second) = next_frame(slow);
        assert!(second.unwrap().pts_ms > first.unwrap().pts_ms);
        drop(slow);
        assert!(capture.manager.lock().unwrap().source.is_none());
        let fresh = capture.subscribe(60).unwrap();
        assert_ne!(thread_id(&capture), original);
        drop(fresh);
    }

    #[test]
    fn cancellation_stops_capture_with_a_live_lease_and_old_handles_cannot_cancel_restarted_source()
    {
        let capture = SharedCapture::synthetic(16, 16);
        let old = capture.subscribe(60).unwrap();
        let cancel = old.cancel_handle();
        cancel.cancel();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if capture
                .manager
                .lock()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                ._capture
                .join
                .as_ref()
                .unwrap()
                .is_finished()
            {
                break;
            }
            assert!(Instant::now() < deadline, "live lease kept capture running");
            std::thread::sleep(Duration::from_millis(1));
        }
        let fresh = capture.subscribe(30).unwrap();
        cancel.cancel();
        drop(old);
        let (fresh, frame) = next_frame(fresh);
        assert!(frame.is_some());
        drop(fresh);
    }

    #[test]
    fn concurrent_last_cancel_and_registration_keep_the_new_subscription_alive() {
        let capture = SharedCapture::synthetic(16, 16);
        for _ in 0..32 {
            let old = capture.subscribe(60).unwrap();
            let cancel = old.cancel_handle();
            let barrier = Arc::new(Barrier::new(2));
            let start = barrier.clone();
            let cancel_worker = std::thread::spawn(move || {
                start.wait();
                cancel.cancel();
            });
            barrier.wait();
            let fresh = capture.subscribe(30).unwrap();
            cancel_worker.join().unwrap();
            drop(old);
            let (fresh, frame) = next_frame(fresh);
            assert!(frame.is_some());
            drop(fresh);
        }
    }

    #[test]
    fn eof_source_replacement_preserves_its_final_pending_frame() {
        let capture = SharedCapture::synthetic(16, 16);
        let publication = Publication::new();
        let cancel = publication.subscribe(100, 60).unwrap();
        publication.publish(Arc::new(CapturedFrame {
            source_monitor: None,
            desktop_generation: 0,
            captured_at: Instant::now(),
            width: 2,
            height: 2,
            bgra: vec![0; 16],
            capture_ms: 0,
            pts_ms: 999,
        }));
        publication.finish();
        // Inject a finished source to test replacement without racing a producer.
        capture.manager.lock().unwrap().source = Some(Source {
            _capture: CaptureHandle {
                publication: publication.clone(),
                join: None,
            },
            publication,
        });
        let mut old = CaptureSubscription {
            manager: capture.manager.clone(),
            cancel,
        };
        let fresh = capture.subscribe(30).unwrap();
        assert_eq!(old.recv().unwrap().pts_ms, 999);
        assert!(old.recv().is_none());
        assert!(old.set_fps(30).is_err());
        drop(old);
        let (fresh, frame) = next_frame(fresh);
        assert!(frame.is_some());
        drop(fresh);
    }

    #[test]
    fn service_cancel_prevents_new_subscribers_and_ends_live_subscriptions() {
        let capture = SharedCapture::synthetic(16, 16);
        let subscription = capture.subscribe(30).unwrap();
        capture.cancel();
        let (_, frame) = next_frame(subscription);
        assert!(frame.is_none());
        assert!(capture.subscribe(30).is_err());
        for fps in [0, 121] {
            assert!(SharedCapture::synthetic(16, 16).subscribe(fps).is_err());
        }
    }

    #[test]
    fn service_cancel_does_not_wait_for_native_teardown_and_wakes_pending_registrations() {
        let capture = SharedCapture::synthetic(16, 16);
        let publication = Publication::new();
        let cancel = publication.subscribe(100, 60).unwrap();
        let (release, released) = mpsc::channel();
        let join = std::thread::spawn(move || {
            released.recv().ok();
        });
        capture.manager.lock().unwrap().source = Some(Source {
            _capture: CaptureHandle {
                publication: publication.clone(),
                join: Some(join),
            },
            publication,
        });
        let subscription = CaptureSubscription {
            manager: capture.manager.clone(),
            cancel,
        };
        let cleanup = std::thread::spawn(move || drop(subscription));
        let deadline = Instant::now() + Duration::from_secs(1);
        while capture.manager.lock().unwrap().retiring.is_none() {
            assert!(Instant::now() < deadline, "capture did not begin teardown");
            std::thread::sleep(Duration::from_millis(1));
        }
        let pending = capture.clone();
        let (done, registered) = mpsc::channel();
        let registration = std::thread::spawn(move || {
            done.send(pending.subscribe(30).is_err()).unwrap();
        });
        assert!(registered.recv_timeout(Duration::from_millis(30)).is_err());
        let started = Instant::now();
        capture.cancel();
        let cancelled = registered.recv_timeout(Duration::from_secs(1));
        release.send(()).ok();
        cleanup.join().unwrap();
        registration.join().unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(cancelled.unwrap());
        assert!(capture.manager.lock().unwrap().retiring.is_none());
    }
}
