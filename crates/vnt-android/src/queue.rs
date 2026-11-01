//! Bounded decoder input. Dropping any reference frame starts a fresh keyframe chain.
use bytes::Bytes;
use std::{collections::VecDeque, time::Instant};
pub const MAX_FRAMES: usize = 3;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub struct Frame {
    pub keyframe: bool,
    pub pts_ms: u32,
    pub data: Bytes,
    pub received: Instant,
}
pub struct VideoQueue {
    frames: VecDeque<Frame>,
    bytes: usize,
    waiting: bool,
    pub resets: u64,
}
impl Default for VideoQueue {
    fn default() -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            waiting: true,
            resets: 0,
        }
    }
}
impl VideoQueue {
    pub fn reset(&mut self) {
        self.frames.clear();
        self.bytes = 0;
        self.waiting = true;
        self.resets += 1;
    }
    /// Returns true when the caller must request a new independently decodable frame.
    pub fn push(&mut self, frame: Frame) -> bool {
        let overflow = self.frames.len() >= MAX_FRAMES || self.bytes + frame.data.len() > MAX_BYTES;
        if overflow {
            self.reset();
        }
        if frame.data.len() > MAX_BYTES {
            return true;
        }
        if self.waiting && !frame.keyframe {
            return overflow;
        }
        self.waiting = false;
        self.bytes += frame.data.len();
        self.frames.push_back(frame);
        overflow
    }
    pub fn pop(&mut self) -> Option<Frame> {
        let f = self.frames.pop_front()?;
        self.bytes -= f.data.len();
        Some(f)
    }
    pub fn len(&self) -> usize {
        self.frames.len()
    }
}
#[derive(Default, Clone)]
pub struct PtsClock {
    last: Option<u32>,
    extended: u64,
}
impl PtsClock {
    pub fn micros(&mut self, pts: u32) -> i64 {
        if let Some(last) = self.last {
            self.extended += pts.wrapping_sub(last) as u64;
        }
        self.last = Some(pts);
        (self.extended * 1000).min(i64::MAX as u64) as i64
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn f(keyframe: bool, n: usize) -> Frame {
        Frame {
            keyframe,
            pts_ms: 0,
            data: vec![0; n].into(),
            received: Instant::now(),
        }
    }
    #[test]
    fn starts_only_at_keyframe_and_resets_overflow() {
        let mut q = VideoQueue::default();
        q.push(f(false, 1));
        assert_eq!(q.len(), 0);
        q.push(f(true, 1));
        q.push(f(false, 1));
        q.push(f(false, 1));
        assert!(q.push(f(false, 1)));
        assert_eq!(q.len(), 0);
        q.push(f(false, 1));
        assert_eq!(q.len(), 0);
        q.push(f(true, 1));
        assert_eq!(q.len(), 1);
    }
    #[test]
    fn bytes_are_bounded_even_for_large_keyframes() {
        let mut q = VideoQueue::default();
        q.push(f(true, MAX_BYTES));
        assert!(q.push(f(false, 1)));
        assert_eq!(q.len(), 0);
        assert!(q.push(f(true, MAX_BYTES + 1)));
        assert_eq!(q.len(), 0);
    }
    #[test]
    fn pts_unwraps_u32_and_uses_relative_monotonic_time() {
        let mut c = PtsClock::default();
        assert_eq!(c.micros(u32::MAX - 5), 0);
        assert_eq!(c.micros(4), 10_000);
    }
}
