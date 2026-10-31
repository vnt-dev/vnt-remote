//! Per-connection TCP/UDP application-wire bytes (including protocol framing).
//! Excludes local WebSocket forwarding and OS-level headers/retransmissions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct TrafficCounters(Arc<Counters>);

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    received: AtomicU64,
    video: Mutex<(u64, u64)>,
}

impl TrafficCounters {
    pub(crate) fn video_sent(&self, bytes: usize) {
        let mut video = self.0.video.lock().unwrap();
        video.0 += 1;
        video.1 += bytes as u64;
    }

    /// Completed TCP video writes, not application/session mailbox enqueues.
    pub fn video_snapshot(&self) -> (u64, u64) {
        *self.0.video.lock().unwrap()
    }
    pub(crate) fn sent(&self, bytes: usize) {
        self.0.sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn received(&self, bytes: usize) {
        self.0.received.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Cumulative (upload, download) bytes, shared by both TCP halves and UDP.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.0.sent.load(Ordering::Relaxed),
            self.0.received.load(Ordering::Relaxed),
        )
    }
}
