use crate::CapturePublisher;
use std::time::{Duration, Instant};

/// Keep the requested cadence across timer overshoot and short driver stalls.
/// Do not add each wakeup's scheduling delay to every subsequent frame.
#[derive(Default)]
pub(crate) struct FramePacer {
    fps: u32,
    deadline: Option<Instant>,
}
impl FramePacer {
    fn advance(&mut self, fps: u32, started: Instant, now: Instant) -> Instant {
        if fps == 0 {
            self.deadline = None;
            self.fps = 0;
            return now;
        }
        let interval = Duration::from_nanos(1_000_000_000 / fps as u64);
        let anchor = if self.fps == fps {
            self.deadline.unwrap_or(started)
        } else {
            started
        };
        let mut next = anchor + interval;
        // A long idle period or driver hang must not produce a catch-up burst.
        if now.saturating_duration_since(next) >= interval {
            next = now;
        }
        self.fps = fps;
        self.deadline = Some(next);
        next
    }
    pub(crate) fn wait(&mut self, tx: &CapturePublisher, fps: u32, started: Instant) {
        let next = self.advance(fps, started, Instant::now());
        let wait = next.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            tx.source().wait(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_short_slow_frame_does_not_permanently_reduce_the_capture_rate() {
        let start = Instant::now();
        let mut pacer = FramePacer::default();
        let at = |ms| start + Duration::from_millis(ms);
        assert_eq!(pacer.advance(50, at(0), at(5)), at(20));
        assert_eq!(pacer.advance(50, at(22), at(42)), at(40));
        assert_eq!(pacer.advance(50, at(42), at(47)), at(60));
    }
    #[test]
    fn idle_gaps_and_rate_changes_reset_the_schedule_without_bursts() {
        let start = Instant::now();
        let mut pacer = FramePacer::default();
        let at = |ms| start + Duration::from_millis(ms);
        pacer.advance(50, at(0), at(5));
        assert_eq!(pacer.advance(50, at(20), at(250)), at(250));
        assert_eq!(pacer.advance(50, at(250), at(255)), at(270));
        assert_eq!(pacer.advance(25, at(270), at(275)), at(310));
    }
}
