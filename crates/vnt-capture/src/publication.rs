//! A single capture producer publishes into independent latest-frame subscriptions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{CaptureError, CapturedFrame};

#[derive(Default)]
struct MailboxState {
    frame: Option<Arc<CapturedFrame>>,
    dropped: u64,
    closed: bool,
    cancelled: bool,
}

#[derive(Default)]
struct Mailbox {
    state: Mutex<MailboxState>,
    ready: Condvar,
}

struct Subscriber {
    mailbox: Arc<Mailbox>,
    fps: u32,
    next_due_us: Option<u64>,
}

impl Subscriber {
    fn accepts(&mut self, pts_ms: u64) -> bool {
        let now = pts_ms.saturating_mul(1000);
        let next = self.next_due_us.get_or_insert(now);
        // Preserve the existing source-time cadence and millisecond tolerance.
        if now.saturating_add(1000) < *next {
            return false;
        }
        *next = next.saturating_add(1_000_000 / self.fps as u64).max(now);
        true
    }
}

pub(crate) struct Publication {
    subscribers: Mutex<HashMap<u64, Subscriber>>,
    generation: AtomicU64,
    changed: Condvar,
    stopped: AtomicBool,
    pub(crate) target_fps: Arc<AtomicU32>,
}

impl Publication {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            subscribers: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
            target_fps: Arc::new(AtomicU32::new(1)),
        })
    }

    pub(crate) fn subscribe(self: &Arc<Self>, id: u64, fps: u32) -> Option<CaptureCancelHandle> {
        let mut subscribers = self.subscribers.lock().unwrap();
        if self.is_stopped() {
            return None;
        }
        let mailbox = Arc::new(Mailbox::default());
        subscribers.insert(
            id,
            Subscriber {
                mailbox: mailbox.clone(),
                fps,
                next_due_us: None,
            },
        );
        self.update_rate(&subscribers);
        Some(CaptureCancelHandle {
            source: self.clone(),
            mailbox,
            id,
        })
    }

    fn update_rate(&self, subscribers: &HashMap<u64, Subscriber>) {
        let changed = if let Some(fps) = subscribers.values().map(|s| s.fps).max() {
            self.target_fps.swap(fps, Ordering::Relaxed) != fps
        } else {
            !self.stopped.swap(true, Ordering::AcqRel)
        };
        if changed {
            self.changed.notify_all();
        }
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Interrupt capture pacing on cancellation or a changed maximum frame rate.
    pub(crate) fn wait(&self, duration: Duration) {
        let subscribers = self.subscribers.lock().unwrap();
        if !self.is_stopped() {
            drop(self.changed.wait_timeout(subscribers, duration).unwrap());
        }
    }

    pub(crate) fn publish(&self, frame: Arc<CapturedFrame>) -> bool {
        let mut retired = Vec::new();
        {
            let mut subscribers = self.subscribers.lock().unwrap();
            if self.is_stopped()
                || frame.desktop_generation != self.generation.load(Ordering::Acquire)
            {
                return false;
            }
            for subscriber in subscribers.values_mut() {
                if !subscriber.accepts(frame.pts_ms) {
                    continue;
                }
                let mut state = subscriber.mailbox.state.lock().unwrap();
                let old = state.frame.replace(frame.clone());
                if let Some(old) = old {
                    state.dropped += 1;
                    retired.push(old);
                } else {
                    // Only an empty mailbox can have a receiver waiting for a frame.
                    subscriber.mailbox.ready.notify_one();
                }
            }
        }
        // Destruction of a last pixel reference must never extend a critical section.
        drop(retired);
        true
    }

    pub(crate) fn invalidate(&self, generation: u64) -> Vec<Arc<CapturedFrame>> {
        let mut retired = Vec::new();
        let mut subscribers = self.subscribers.lock().unwrap();
        self.generation.store(generation, Ordering::Release);
        for subscriber in subscribers.values_mut() {
            subscriber.next_due_us = None;
            if let Some(frame) = subscriber.mailbox.state.lock().unwrap().frame.take() {
                retired.push(frame);
            }
        }
        self.changed.notify_all();
        retired
    }

    /// Producer EOF (including panic) preserves the final pending frame.
    pub(crate) fn finish(&self) {
        let subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        self.stopped.store(true, Ordering::Release);
        for subscriber in subscribers.values() {
            let mut state = subscriber
                .mailbox
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            subscriber.mailbox.ready.notify_all();
        }
        self.changed.notify_all();
    }

    pub(crate) fn cancel(&self) {
        let mut retired = Vec::new();
        let subscribers = {
            let mut subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
            self.stopped.store(true, Ordering::Release);
            for subscriber in subscribers.values() {
                let mut state = subscriber
                    .mailbox
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                state.cancelled = true;
                retired.extend(state.frame.take());
                subscriber.mailbox.ready.notify_all();
            }
            std::mem::take(&mut *subscribers)
        };
        self.changed.notify_all();
        drop(subscribers);
        drop(retired);
    }
}

/// Unique producer guard. Dropping it closes subscriptions even during unwinding.
/// It deliberately has no Clone implementation.
pub(crate) struct CapturePublisher(pub(crate) Arc<Publication>);

impl CapturePublisher {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.is_stopped()
    }

    pub(crate) fn send(&self, frame: CapturedFrame) -> bool {
        self.0.publish(Arc::new(frame))
    }

    pub(crate) fn send_shared(&self, frame: Arc<CapturedFrame>) -> bool {
        self.0.publish(frame)
    }

    pub(crate) fn source(&self) -> Arc<Publication> {
        self.0.clone()
    }
}

impl Drop for CapturePublisher {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// A cloneable cancellation handle; it never joins a capture or encoder thread.
#[derive(Clone)]
pub struct CaptureCancelHandle {
    source: Arc<Publication>,
    mailbox: Arc<Mailbox>,
    id: u64,
}

impl CaptureCancelHandle {
    pub(crate) fn source(&self) -> Arc<Publication> {
        self.source.clone()
    }

    /// Cancel only this subscription, clear pending pixels and wake its receiver.
    pub fn cancel(&self) {
        let (subscriber, retired) = {
            // All operations needing both locks take registry -> mailbox.
            let mut subscribers = self
                .source
                .subscribers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let subscriber = subscribers.remove(&self.id);
            let mut state = self.mailbox.state.lock().unwrap_or_else(|e| e.into_inner());
            state.cancelled = true;
            let retired = state.frame.take();
            self.source.update_rate(&subscribers);
            (subscriber, retired)
        };
        self.mailbox.ready.notify_all();
        drop(subscriber);
        drop(retired);
    }

    pub fn is_cancelled(&self) -> bool {
        self.mailbox.state.lock().unwrap().cancelled
    }

    pub(crate) fn recv(&self) -> Option<Arc<CapturedFrame>> {
        let mut state = self.mailbox.state.lock().unwrap();
        loop {
            if state.cancelled {
                return None;
            }
            if let Some(frame) = state.frame.take() {
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.mailbox.ready.wait(state).unwrap();
        }
    }

    pub(crate) fn set_fps(&self, fps: u32) -> Result<(), CaptureError> {
        if !(1..=120).contains(&fps) {
            return Err(CaptureError::Create("共享抓屏帧率须为 1–120".into()));
        }
        let mut subscribers = self.source.subscribers.lock().unwrap();
        if self.source.is_stopped() {
            return Err(CaptureError::Stopped);
        }
        let subscriber = subscribers.get_mut(&self.id).ok_or(CaptureError::Stopped)?;
        if subscriber.fps != fps {
            subscriber.fps = fps;
            subscriber.next_due_us = None;
        }
        self.source.update_rate(&subscribers);
        Ok(())
    }

    pub(crate) fn dropped_frames(&self) -> u64 {
        self.mailbox.state.lock().unwrap().dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    fn frame(pts_ms: u64) -> Arc<CapturedFrame> {
        Arc::new(CapturedFrame {
            source_monitor: None,
            desktop_generation: 0,
            captured_at: Instant::now(),
            width: 2,
            height: 2,
            bgra: vec![pts_ms as u8; 16],
            capture_ms: 0,
            pts_ms,
        })
    }

    #[test]
    fn fanout_shares_pixels_and_slow_subscribers_only_drop_their_own_frames() {
        let source = Publication::new();
        let fast = source.subscribe(0, 60).unwrap();
        let slow = source.subscribe(1, 60).unwrap();
        let first = frame(0);
        source.publish(first.clone());
        let held = fast.recv().unwrap();
        assert!(Arc::ptr_eq(&first, &held));
        for pts in [17, 34] {
            let latest = frame(pts);
            source.publish(latest.clone());
            assert!(Arc::ptr_eq(&latest, &fast.recv().unwrap()));
        }
        assert_eq!(slow.recv().unwrap().pts_ms, 34);
        assert_eq!(slow.dropped_frames(), 2);
        assert_eq!(fast.dropped_frames(), 0);
        assert_eq!(held.bgra, [0; 16]);
        source.publish(frame(50));
        assert!(Arc::ptr_eq(&fast.recv().unwrap(), &slow.recv().unwrap()));
    }

    #[test]
    fn each_subscription_keeps_its_requested_cadence_without_counting_rate_skips() {
        let source = Publication::new();
        let subscribers: Vec<_> = [30, 60, 120]
            .into_iter()
            .enumerate()
            .map(|(id, fps)| (source.subscribe(id as u64, fps).unwrap(), fps))
            .collect();
        for tick in 0..120 {
            source.publish(frame(tick * 1000 / 120));
        }
        for (subscriber, fps) in subscribers {
            assert_eq!(subscriber.dropped_frames() + 1, fps as u64);
        }
    }

    #[test]
    fn changing_rate_resets_one_cadence_and_cancellation_lowers_the_source_rate() {
        let source = Publication::new();
        let fast = source.subscribe(0, 60).unwrap();
        let slow = source.subscribe(1, 30).unwrap();
        for (rate, maximum) in [(20, 30), (90, 90), (10, 30)] {
            fast.set_fps(rate).unwrap();
            assert_eq!(source.target_fps.load(Ordering::Relaxed), maximum);
        }
        source.publish(frame(0));
        fast.recv().unwrap();
        fast.set_fps(60).unwrap();
        source.publish(frame(1));
        assert_eq!(fast.recv().unwrap().pts_ms, 1);
        assert_eq!(slow.dropped_frames(), 0);
        fast.cancel();
        assert_eq!(source.target_fps.load(Ordering::Relaxed), 30);
        assert!(!source.is_stopped());
        assert!(fast.set_fps(30).is_err());
        assert!(slow.set_fps(0).is_err());
        assert!(slow.set_fps(121).is_err());
        slow.cancel();
        assert!(source.is_stopped());
        assert!(!source.publish(frame(2)));
        assert!(source.subscribe(2, 30).is_none());
    }

    #[test]
    fn cancelled_and_evicted_pixels_are_released_with_live_handles() {
        let source = Publication::new();
        let subscriber = source.subscribe(0, 60).unwrap();
        let pixels = frame(0);
        let first = Arc::downgrade(&pixels);
        source.publish(pixels);
        let pixels = frame(17);
        let last = Arc::downgrade(&pixels);
        source.publish(pixels);
        assert!(first.upgrade().is_none());
        assert!(last.upgrade().is_some());
        subscriber.cancel();
        assert!(last.upgrade().is_none());
        assert!(subscriber.recv().is_none());
        assert!(source.subscribers.lock().unwrap().is_empty());
    }

    #[test]
    fn consumed_pixels_are_not_retained_by_the_publisher() {
        let source = Publication::new();
        let subscriber = source.subscribe(0, 60).unwrap();
        let pixels = frame(0);
        let weak = Arc::downgrade(&pixels);
        source.publish(pixels);
        drop(subscriber.recv().unwrap());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn producer_eof_delivers_pending_pixels_then_ends() {
        let source = Publication::new();
        let subscriber = source.subscribe(0, 60).unwrap();
        source.publish(frame(0));
        drop(CapturePublisher(source.clone()));
        assert_eq!(subscriber.recv().unwrap().pts_ms, 0);
        assert!(subscriber.recv().is_none());
        assert!(subscriber.set_fps(30).is_err());
    }

    #[test]
    fn explicit_source_cancel_discards_pixels_even_after_eof() {
        let source = Publication::new();
        let a = source.subscribe(0, 60).unwrap();
        let b = source.subscribe(1, 30).unwrap();
        let pixels = frame(0);
        let weak = Arc::downgrade(&pixels);
        source.publish(pixels);
        source.finish();
        source.cancel();
        assert!(weak.upgrade().is_none());
        assert!(a.recv().is_none());
        assert!(b.recv().is_none());
        assert!(a.is_cancelled() && b.is_cancelled());
    }

    fn check_wakeup(action: impl FnOnce(&Arc<Publication>, &CaptureCancelHandle)) {
        let source = Publication::new();
        let subscriber = source.subscribe(0, 60).unwrap();
        let receiver = subscriber.clone();
        let (done, result) = mpsc::channel();
        let (started, entered) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            done.send(receiver.recv()).unwrap();
        });
        entered.recv().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        action(&source, &subscriber);
        let received = result.recv_timeout(Duration::from_secs(1));
        if received.is_err() {
            source.cancel();
        }
        worker.join().unwrap();
        assert!(received.is_ok(), "subscriber did not wake");
    }

    #[test]
    fn arrival_cancel_and_eof_wake_receivers_without_polling() {
        check_wakeup(|source, _| {
            source.publish(frame(0));
        });
        check_wakeup(|_, subscriber| subscriber.cancel());
        check_wakeup(|source, _| source.finish());
        check_wakeup(|source, _| source.cancel());
    }

    #[test]
    fn producer_panic_wakes_all_blocked_subscribers() {
        let source = Publication::new();
        let a = source.subscribe(0, 60).unwrap();
        let b = source.subscribe(1, 30).unwrap();
        let producer = CapturePublisher(source.clone());
        let thread = std::thread::spawn(move || {
            let _guard = producer;
            panic!("simulated capture failure");
        });
        assert!(thread.join().is_err());
        assert!(a.recv().is_none());
        assert!(b.recv().is_none());
    }

    #[test]
    fn stalled_subscribers_never_apply_capacity_backpressure() {
        let source = Publication::new();
        let subscriber = source.subscribe(0, 120).unwrap();
        let producer = source.clone();
        let (done, result) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            for tick in 0..2000 {
                assert!(producer.publish(frame(tick * 10)));
            }
            done.send(()).unwrap();
        });
        let completed = result.recv_timeout(Duration::from_secs(2));
        if completed.is_err() {
            source.cancel();
        }
        thread.join().unwrap();
        assert!(completed.is_ok());
        assert_eq!(subscriber.recv().unwrap().pts_ms, 19990);
        assert_eq!(subscriber.dropped_frames(), 1999);
    }

    #[test]
    #[ignore = "explicit direct-publication microbenchmark"]
    fn publication_cost() {
        for count in [1, 8, 32] {
            let source = Publication::new();
            let subscribers: Vec<_> = (0..count)
                .map(|id| source.subscribe(id, 120).unwrap())
                .collect();
            let started = Instant::now();
            for tick in 0..100_000 {
                source.publish(frame(tick * 10));
            }
            eprintln!(
                "PUBLICATION {count} stalled subscribers: {:.3} us/frame",
                started.elapsed().as_secs_f64() * 10.0
            );
            drop(subscribers);
        }
    }
}
