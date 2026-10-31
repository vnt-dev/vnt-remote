use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use vnt_proto::pb;

const FRESH: Duration = Duration::from_secs(3);
const BITRATE_INTERVAL: Duration = Duration::from_secs(1);
const FPS_INTERVAL: Duration = Duration::from_secs(3);
const HEALTHY_WINDOW: Duration = Duration::from_secs(5);
const INTERACTIVE_FPS: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub fps: u32,
    pub kbps: u32,
}

#[derive(Default)]
struct InboxState {
    sequence: u64,
    received_at: Option<Instant>,
    latest: Option<(pb::LinkFeedback, Instant)>,
}

/// Bounded, latest-wins telemetry: readers never queue work per video frame.
#[derive(Default)]
pub struct FeedbackInbox(Mutex<InboxState>);

impl FeedbackInbox {
    pub fn publish(&self, report: pb::LinkFeedback, now: Instant) -> bool {
        if !vnt_proto::validation::valid_link_feedback(&report) {
            return false;
        }
        let mut state = self.0.lock().unwrap();
        if report.sequence <= state.sequence
            || state
                .received_at
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(500))
        {
            return false;
        }
        state.sequence = report.sequence;
        state.received_at = Some(now);
        state.latest = Some((report, now));
        true
    }

    pub fn take(&self) -> Option<(pb::LinkFeedback, Instant)> {
        self.0.lock().unwrap().latest.take()
    }
}

/// Balanced, per-session adaptation. Successful local enqueues cannot override
/// receiver-reported congestion; stale feedback can never trigger recovery.
pub struct SendCongestion {
    background: bool,
    limits: Target,
    /// Foreground encoder quality budget, before reserving transport FEC.
    minimum_kbps: u32,
    fec_ratio: u32,
    target: Target,
    applied: Target,
    sequence: u64,
    last_feedback: Option<Instant>,
    loss: Option<u32>,
    rtt_history: VecDeque<(Instant, u32)>,
    bad_streak: u32,
    network_bad_since: Option<Instant>,
    local_full_since: Option<Instant>,
    last_local_pressure: Option<Instant>,
    healthy_since: Option<Instant>,
    last_bitrate_change: Option<Instant>,
    last_fps_change: Option<Instant>,
    last_recovery: Option<Instant>,
    last_attempt: Option<Instant>,
    pub reason: &'static str,
}

impl SendCongestion {
    pub fn new(kbps: u32, fps: u32) -> Self {
        let limits = Target { fps, kbps };
        Self {
            background: false,
            fec_ratio: 0,
            limits,
            minimum_kbps: 250.min(kbps),
            target: limits,
            applied: limits,
            sequence: 0,
            last_feedback: None,
            loss: None,
            rtt_history: VecDeque::new(),
            bad_streak: 0,
            network_bad_since: None,
            local_full_since: None,
            last_local_pressure: None,
            healthy_since: None,
            last_bitrate_change: None,
            last_fps_change: None,
            last_recovery: None,
            last_attempt: None,
            reason: "用户上限",
        }
    }

    pub fn target(&self) -> Target {
        let mut target = Target {
            fps: self.target.fps,
            kbps: ((u64::from(self.target.kbps.max(self.bitrate_floor())) * 1000
                / (1000 + u64::from(self.fec_ratio))) as u32)
                .max(100.min(self.limits.kbps)),
        };
        if self.background {
            let fps = self.limits.fps.min(5);
            let kbps =
                (u64::from(self.limits.kbps) * u64::from(fps) / u64::from(self.limits.fps)) as u32;
            target.fps = target.fps.min(fps);
            target.kbps = target.kbps.min(kbps.max(100).min(self.limits.kbps));
        }
        target
    }

    /// Use the encoded dimensions, after cropping/scaling. Start at 2 Mbps for
    /// 1080p, scale by pixel count and round up to 500 kbps (minimum 500 kbps).
    /// This is a target-rate guard, not a guarantee of perceptual quality.
    pub fn set_resolution(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let pixels = u64::from(width) * u64::from(height);
        self.minimum_kbps = pixels
            .saturating_mul(2000)
            .div_ceil(1920 * 1080)
            .div_ceil(500)
            .saturating_mul(500)
            .max(500)
            .min(u64::from(self.limits.kbps)) as u32;
        // A larger screen must not inherit the previous screen's low target.
        let floor = self.bitrate_floor();
        if self.target.kbps < floor {
            self.target.kbps = floor;
            self.last_attempt = None;
            self.reason = "分辨率画质保护";
        }
    }

    /// Reserve FEC on top of the encoder floor, but never exceed the user's
    /// total budget. If that budget is insufficient, FEC still takes its share.
    fn bitrate_floor(&self) -> u32 {
        (u64::from(self.minimum_kbps) * (1000 + u64::from(self.fec_ratio)))
            .div_ceil(1000)
            .min(u64::from(self.limits.kbps)) as u32
    }

    pub fn set_background(&mut self, background: bool) {
        if self.background != background {
            self.background = background;
            // Visibility changes apply on the next frame; retries still back off.
            self.last_attempt = None;
        }
    }
    pub fn applied(&self) -> Target {
        self.applied
    }
    pub fn loss_permille(&self, now: Instant) -> Option<u32> {
        self.last_feedback
            .filter(|last| now.duration_since(*last) <= FRESH)
            .and(self.loss)
    }
    pub fn reconfigure_due(&self, now: Instant) -> bool {
        self.applied != self.target() && elapsed(self.last_attempt, now, BITRATE_INTERVAL)
    }
    pub fn attempted(&mut self, now: Instant) {
        self.last_attempt = Some(now);
    }
    pub fn applied_successfully(&mut self, target: Target) {
        self.applied = target;
    }

    pub fn on_pressure(&mut self, now: Instant) {
        self.healthy_since = None;
        if self
            .last_local_pressure
            .is_some_and(|last| now.duration_since(last) > Duration::from_millis(500))
        {
            self.local_full_since = None;
        }
        self.last_local_pressure = Some(now);
        let since = *self.local_full_since.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_millis(300) {
            self.worsen(now, since, "发送队列持续拥塞");
        }
    }

    pub fn on_sent(&mut self) {
        self.local_full_since = None;
    }

    /// Raw mailbox replacement is harmless. Only sustained waiting for an
    /// actual output slot is local transport pressure, separate from UDP loss.
    pub fn on_send_wait(&mut self, now: Instant, waited: Duration) {
        if waited < Duration::from_millis(10) {
            self.on_sent();
            return;
        }
        if self
            .last_local_pressure
            .is_none_or(|last| now.duration_since(last) > waited + Duration::from_millis(500))
        {
            self.local_full_since = Some(now - waited);
        }
        self.local_full_since.get_or_insert(now - waited);
        self.healthy_since = None;
        self.last_local_pressure = Some(now);
        let since = self.local_full_since.unwrap();
        if now.duration_since(since) >= Duration::from_millis(300) {
            self.worsen(now, since, "发送等待持续过长");
        }
    }

    pub fn on_feedback(&mut self, report: pb::LinkFeedback, now: Instant) {
        if report.sequence <= self.sequence || !vnt_proto::validation::valid_link_feedback(&report)
        {
            return;
        }
        if self
            .last_feedback
            .is_some_and(|last| now.duration_since(last) > Duration::from_millis(1500))
        {
            self.healthy_since = None;
            self.network_bad_since = None;
            self.bad_streak = 0;
        }
        self.sequence = report.sequence;
        self.last_feedback = Some(now);
        let total = u64::from(report.received_frames) + u64::from(report.lost_frames);
        if let Some(ratio) = report
            .fec
            .iter()
            .filter(|f| !f.audio)
            .map(|f| f.ratio_permille)
            .max()
        {
            self.fec_ratio = ratio.min(300);
        }
        self.loss = (total > 0).then(|| (u64::from(report.lost_frames) * 1000 / total) as u32);
        while self
            .rtt_history
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(60))
        {
            self.rtt_history.pop_front();
        }
        if let Some(rtt) = report.rtt_ms {
            self.rtt_history.push_back((now, rtt));
        }
        let baseline = self.rtt_history.iter().map(|(_, rtt)| *rtt).min();
        let rtt_delta = report
            .rtt_ms
            .zip(baseline)
            .map(|(rtt, base)| rtt.saturating_sub(base));
        let enough = total >= 10;
        let severe = self.loss.is_some_and(|loss| loss >= 100)
            || report.frame_gap_ms.is_some_and(|gap| gap >= 1000);
        let bad = severe
            || (enough && self.loss.is_some_and(|loss| loss >= 30))
            || rtt_delta
                .zip(baseline)
                .is_some_and(|(delta, base)| delta > 50.max(base / 2));
        if bad {
            self.healthy_since = None;
            self.bad_streak = self.bad_streak.saturating_add(1);
            let since = *self.network_bad_since.get_or_insert(now);
            if severe || self.bad_streak >= 2 {
                self.worsen(
                    now,
                    since,
                    if severe {
                        "接收端严重丢帧或断流"
                    } else {
                        "接收端丢帧或 RTT 上升"
                    },
                );
            }
        } else {
            self.bad_streak = 0;
            self.network_bad_since = None;
            // Recovery must remain possible after rate control has reduced
            // actual output below ten frames/sec. Require real progress in
            // every healthy window, rather than the pre-congestion frame rate.
            let healthy = report.received_frames > 0
                && self.loss.is_some_and(|loss| loss <= 10)
                && rtt_delta.is_some_and(|delta| delta < 20)
                && self
                    .last_local_pressure
                    .is_none_or(|last| now.duration_since(last) > BITRATE_INTERVAL);
            if healthy {
                self.healthy_since.get_or_insert(now);
            } else {
                self.healthy_since = None;
            }
        }
    }

    fn worsen(&mut self, now: Instant, since: Instant, reason: &'static str) {
        if elapsed(self.last_bitrate_change, now, BITRATE_INTERVAL) {
            let kbps = (self.target.kbps * 7 / 10).max(self.bitrate_floor());
            if kbps != self.target.kbps {
                self.target.kbps = kbps;
                self.last_bitrate_change = Some(now);
            }
        }
        if now.duration_since(since) >= FPS_INTERVAL
            && elapsed(self.last_fps_change, now, FPS_INTERVAL)
        {
            let fps = (self.target.fps * 8 / 10).max(10.min(self.limits.fps));
            if fps != self.target.fps {
                self.target.fps = fps;
                self.last_fps_change = Some(now);
            }
        }
        self.reason = reason;
        self.last_recovery = None;
    }

    pub fn tick(&mut self, now: Instant) {
        if self
            .last_feedback
            .is_none_or(|last| now.duration_since(last) > FRESH)
        {
            self.healthy_since = None;
            self.network_bad_since = None;
            self.bad_streak = 0;
            return;
        }
        if self
            .healthy_since
            .is_none_or(|since| now.duration_since(since) < HEALTHY_WINDOW)
            || !elapsed(self.last_recovery, now, FPS_INTERVAL)
        {
            return;
        }
        // Recover basic responsiveness first, without overshooting 20 FPS or
        // the user's limit. Then restore image quality before the remaining FPS.
        let interactive_fps = INTERACTIVE_FPS.min(self.limits.fps);
        if self.target.fps < interactive_fps {
            if !elapsed(self.last_fps_change, now, FPS_INTERVAL) {
                return;
            }
            self.target.fps = (self.target.fps + 5).min(interactive_fps);
            self.last_fps_change = Some(now);
        } else if self.target.kbps < self.limits.kbps
            && elapsed(self.last_bitrate_change, now, BITRATE_INTERVAL)
        {
            self.target.kbps =
                (self.target.kbps + (self.target.kbps / 10).max(100)).min(self.limits.kbps);
            self.last_bitrate_change = Some(now);
        } else if self.target.kbps == self.limits.kbps
            && self.target.fps < self.limits.fps
            && elapsed(self.last_fps_change, now, FPS_INTERVAL)
        {
            self.target.fps = (self.target.fps + 5).min(self.limits.fps);
            self.last_fps_change = Some(now);
        } else {
            return;
        }
        self.last_recovery = Some(now);
        self.reason = "链路持续稳定，逐步恢复";
    }
}

fn elapsed(last: Option<Instant>, now: Instant, duration: Duration) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(sequence: u64, lost: u32, rtt: Option<u32>) -> pb::LinkFeedback {
        pb::LinkFeedback {
            fec: Vec::new(),
            sequence,
            interval_ms: 1000,
            received_frames: 30 - lost,
            lost_frames: lost,
            received_bytes: 100_000,
            frame_gap_ms: Some(10),
            rtt_ms: rtt,
        }
    }

    #[test]
    fn brief_startup_waits_do_not_reduce_quality_but_sustained_backpressure_does() {
        let t = Instant::now();
        let mut congestion = SendCongestion::new(8000, 30);
        congestion.on_send_wait(t + Duration::from_millis(100), Duration::from_millis(100));
        assert_eq!(congestion.target().kbps, 8000);
        congestion.on_send_wait(t + Duration::from_millis(150), Duration::ZERO);
        assert_eq!(congestion.loss_permille(t), None);
        congestion.on_send_wait(t + Duration::from_millis(1100), Duration::from_millis(900));
        assert_eq!(congestion.target().kbps, 5600);
        assert_eq!(congestion.target().fps, 30);
        assert_eq!(congestion.loss_permille(t), None);
    }

    #[test]
    fn single_bad_window_does_not_lower_quality_but_two_windows_reduce_bitrate_first() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 60);
        c.on_feedback(report(1, 1, Some(20)), t);
        assert_eq!(
            c.target(),
            Target {
                fps: 60,
                kbps: 8000
            }
        );
        c.on_feedback(report(2, 1, Some(20)), t + Duration::from_secs(1));
        assert_eq!(
            c.target(),
            Target {
                fps: 60,
                kbps: 5600
            }
        );
        // A successful local send cannot undo loss reported by the receiver.
        c.on_sent();
        assert_eq!(c.target().kbps, 5600);
    }

    #[test]
    fn sustained_severe_loss_lowers_fps_after_three_seconds_and_respects_cooldowns_and_floors() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 60);
        for second in 0..100 {
            let now = t + Duration::from_secs(second);
            c.on_feedback(report(second + 1, 3, Some(20)), now);
            c.tick(now);
            if second == 2 {
                assert_eq!(c.target().fps, 60);
            }
            if second == 3 || second == 5 {
                assert_eq!(c.target().fps, 48);
            }
            if second == 6 {
                assert_eq!(c.target().fps, 38);
            }
        }
        assert_eq!(c.target(), Target { fps: 10, kbps: 250 });
        let mut small = SendCongestion::new(100, 1);
        for second in 0..10 {
            small.on_feedback(
                report(second + 1, 3, Some(20)),
                t + Duration::from_secs(second),
            );
        }
        assert_eq!(small.target(), Target { fps: 1, kbps: 100 });
    }

    #[test]
    fn recovery_requires_five_healthy_seconds_restores_bitrate_before_fps_and_stays_bounded() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        for second in 0..4 {
            c.on_feedback(
                report(second + 1, 3, Some(20)),
                t + Duration::from_secs(second),
            );
        }
        assert_eq!(c.target().fps, 24);
        let reduced_kbps = c.target().kbps;
        for second in 4..=8 {
            let now = t + Duration::from_secs(second);
            c.on_feedback(report(second + 1, 0, Some(20)), now);
            c.tick(now);
            assert_eq!(c.target().fps, 24);
        }
        for second in 9..=90 {
            let now = t + Duration::from_secs(second);
            c.on_feedback(report(second + 1, 0, Some(20)), now);
            c.tick(now);
            if c.target().kbps < 8000 {
                assert_eq!(c.target().fps, 24, "quality must recover before frame rate");
            }
            if second == 9 {
                assert_eq!(
                    c.target(),
                    Target {
                        fps: 24,
                        kbps: reduced_kbps + reduced_kbps / 10
                    }
                );
            }
            if second == 12 {
                assert_eq!(c.target().fps, 24);
                assert!(c.target().kbps > reduced_kbps + reduced_kbps / 10);
            }
        }
        assert_eq!(
            c.target(),
            Target {
                fps: 30,
                kbps: 8000
            }
        );
    }

    #[test]
    fn low_fps_recovers_to_twenty_before_bitrate_then_waits_for_quality() {
        let start = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        c.set_resolution(1920, 1080);
        for second in 0..30 {
            c.on_feedback(
                report(second + 1, 3, Some(30)),
                start + Duration::from_secs(second),
            );
        }
        assert_eq!(
            c.target(),
            Target {
                fps: 10,
                kbps: 2000
            }
        );
        for second in 30..=41 {
            let now = start + Duration::from_secs(second);
            c.on_feedback(report(second + 1, 0, Some(30)), now);
            c.tick(now);
            let expected = match second {
                30..=34 => Target {
                    fps: 10,
                    kbps: 2000,
                },
                35..=37 => Target {
                    fps: 15,
                    kbps: 2000,
                },
                38..=40 => Target {
                    fps: 20,
                    kbps: 2000,
                },
                _ => Target {
                    fps: 20,
                    kbps: 2200,
                },
            };
            assert_eq!(c.target(), expected);
        }
        for second in 42..=110 {
            let now = start + Duration::from_secs(second);
            c.on_feedback(report(second + 1, 0, Some(30)), now);
            c.tick(now);
            if c.target().kbps < 8000 {
                assert_eq!(c.target().fps, 20);
            }
        }
        assert_eq!(
            c.target(),
            Target {
                fps: 30,
                kbps: 8000
            }
        );
    }

    #[test]
    fn fps_first_recovery_stops_at_twenty_or_the_lower_user_limit() {
        let start = Instant::now();
        for (limit, reduced, restored) in [(30, 19, 20), (15, 10, 15)] {
            let mut c = SendCongestion::new(8000, limit);
            c.set_resolution(1920, 1080);
            for second in 0..7 {
                c.on_feedback(
                    report(second + 1, 3, Some(30)),
                    start + Duration::from_secs(second),
                );
            }
            assert_eq!(c.target().fps, reduced);
            let reduced_kbps = c.target().kbps;
            for second in 7..=12 {
                let now = start + Duration::from_secs(second);
                c.on_feedback(report(second + 1, 0, Some(30)), now);
                c.tick(now);
                assert_eq!(c.target().kbps, reduced_kbps);
            }
            assert_eq!(c.target().fps, restored);
        }
    }

    #[test]
    fn stale_or_empty_feedback_cannot_raise_targets() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        c.on_feedback(report(1, 3, Some(20)), t);
        for second in 1..=4 {
            c.on_feedback(
                report(second + 1, 0, Some(20)),
                t + Duration::from_secs(second),
            );
        }
        c.tick(t + Duration::from_secs(8));
        assert_eq!(c.target().kbps, 5600);
        assert_eq!(c.loss_permille(t + Duration::from_secs(8)), None);
        for second in 9..=20 {
            let mut sparse = report(second + 1, 0, Some(20));
            sparse.received_frames = 0;
            sparse.received_bytes = 0;
            c.on_feedback(sparse, t + Duration::from_secs(second));
            c.tick(t + Duration::from_secs(second));
        }
        assert_eq!(c.target().kbps, 5600);
    }

    #[test]
    fn low_rate_but_healthy_video_can_recover_instead_of_remaining_at_the_floor() {
        let t = Instant::now();
        let mut congestion = SendCongestion::new(8000, 30);
        congestion.on_feedback(report(1, 3, Some(20)), t);
        for second in 1..=10 {
            let mut progress = report(second + 1, 0, Some(20));
            progress.received_frames = 2;
            progress.received_bytes = 6000;
            congestion.on_feedback(progress, t + Duration::from_secs(second));
            congestion.tick(t + Duration::from_secs(second));
        }
        assert!(congestion.target().kbps > 5600);
    }

    #[test]
    fn stable_high_rtt_is_healthy_but_a_sustained_rtt_increase_is_pressure() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        for second in 0..5 {
            c.on_feedback(
                report(second + 1, 0, Some(400)),
                t + Duration::from_secs(second),
            );
            c.tick(t + Duration::from_secs(second));
        }
        assert_eq!(c.target().kbps, 8000);
        c.on_feedback(report(6, 0, Some(650)), t + Duration::from_secs(5));
        assert_eq!(c.target().kbps, 8000);
        c.on_feedback(report(7, 0, Some(650)), t + Duration::from_secs(6));
        assert_eq!(c.target().kbps, 5600);
    }

    #[test]
    fn total_udp_loss_is_detected_without_sequence_gaps_and_new_pressure_cancels_recovery() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        let mut silence = report(1, 0, Some(20));
        silence.received_frames = 0;
        silence.received_bytes = 0;
        silence.frame_gap_ms = Some(1000);
        c.on_feedback(silence, t);
        assert_eq!(c.target().kbps, 5600);
        for second in 1..=4 {
            c.on_feedback(
                report(second + 1, 0, Some(20)),
                t + Duration::from_secs(second),
            );
        }
        c.on_pressure(t + Duration::from_millis(4900));
        c.tick(t + Duration::from_secs(6));
        assert_eq!(c.target().kbps, 5600);
    }

    #[test]
    fn local_pressure_lowers_bitrate_and_fps_without_feedback() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 60);
        for i in 0..=40 {
            c.on_pressure(t + Duration::from_millis(i * 100));
        }
        assert!(c.target().kbps < 8000);
        assert_eq!(c.target().fps, 48);
    }

    #[test]
    fn failed_reconfiguration_keeps_applied_values_and_waits_before_retrying() {
        let t = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        c.on_feedback(report(1, 3, Some(20)), t);
        assert!(c.reconfigure_due(t));
        c.attempted(t);
        assert_eq!(
            c.applied(),
            Target {
                fps: 30,
                kbps: 8000
            }
        );
        assert!(!c.reconfigure_due(t + Duration::from_millis(999)));
        assert!(c.reconfigure_due(t + Duration::from_secs(1)));
        c.applied_successfully(c.target());
        assert!(!c.reconfigure_due(t + Duration::from_secs(1)));
    }

    #[test]
    fn inbox_validates_deduplicates_rate_limits_and_delivers_each_report_once() {
        let t = Instant::now();
        let inbox = FeedbackInbox::default();
        assert!(inbox.publish(report(1, 0, None), t));
        assert!(!inbox.publish(report(1, 0, None), t + Duration::from_secs(1)));
        assert!(!inbox.publish(report(2, 0, None), t + Duration::from_millis(100)));
        let mut invalid = report(2, 0, None);
        invalid.interval_ms = 0;
        assert!(!inbox.publish(invalid, t + Duration::from_secs(1)));
        assert_eq!(inbox.take().unwrap().0.sequence, 1);
        assert!(inbox.take().is_none());
        assert!(inbox.publish(report(2, 0, None), t + Duration::from_secs(1)));
        assert!(inbox.publish(report(3, 0, None), t + Duration::from_secs(2)));
        assert_eq!(inbox.take().unwrap().0.sequence, 3);
    }

    #[test]
    fn fec_reserves_bitrate_without_compounding_and_residual_loss_still_reduces_quality() {
        let now = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        for sequence in 1..=3 {
            let mut f = report(sequence, 0, Some(10));
            f.fec = vec![pb::FecFeedback {
                expected: 100,
                received: 90,
                recovered: 10,
                ratio_permille: 250,
                ..Default::default()
            }];
            c.on_feedback(f, now + Duration::from_secs(sequence));
            assert_eq!(
                c.target(),
                Target {
                    fps: 30,
                    kbps: 6400
                }
            );
        }
        c.on_feedback(report(4, 0, Some(10)), now + Duration::from_secs(4));
        assert_eq!(
            c.target().kbps,
            6400,
            "empty telemetry cannot release an active FEC reservation"
        );
        c.on_feedback(report(5, 5, Some(100)), now + Duration::from_secs(5));
        assert!(c.target().kbps < 6400);
    }

    #[test]
    fn background_caps_encoding_without_replacing_network_targets() {
        let now = Instant::now();
        let mut c = SendCongestion::new(30_000, 30);
        c.applied_successfully(c.target());
        c.attempted(now);
        c.set_background(true);
        assert_eq!(
            c.target(),
            Target {
                fps: 5,
                kbps: 5_000
            }
        );
        assert!(c.reconfigure_due(now));
        c.applied_successfully(c.target());
        c.attempted(now);
        // A real network limit must survive visibility restoration.
        c.target = Target {
            fps: 12,
            kbps: 2_000,
        };
        assert_eq!(
            c.target(),
            Target {
                fps: 5,
                kbps: 2_000
            }
        );
        c.set_background(false);
        assert_eq!(
            c.target(),
            Target {
                fps: 12,
                kbps: 2_000
            }
        );
        assert!(c.reconfigure_due(now));
    }

    #[test]
    fn background_never_raises_low_requested_limits_and_failed_reconfigure_backs_off() {
        let now = Instant::now();
        for fps in [1, 3, 5, 30, 120] {
            for kbps in [100, 200, 30_000, 200_000] {
                let mut c = SendCongestion::new(kbps, fps);
                c.set_background(true);
                let target = c.target();
                assert_eq!(target.fps, fps.min(5));
                assert_eq!(target.kbps, (kbps * fps.min(5) / fps).max(100).min(kbps));
                assert!(target.kbps <= kbps);
                c.attempted(now);
                // Identical replay must not bypass retry backoff.
                c.set_background(true);
                assert!(!c.reconfigure_due(now + Duration::from_millis(999)));
            }
        }
    }

    #[test]
    fn sustained_congestion_respects_encoded_pixel_count_and_user_ceiling() {
        let now = Instant::now();
        for (width, height, limit, floor) in [
            (320, 240, 24_000, 500),
            (1280, 720, 24_000, 1000),
            (1920, 1080, 24_000, 2000),
            (1080, 1920, 24_000, 2000),
            (2560, 1440, 24_000, 4000),
            (3840, 2160, 24_000, 8000),
            (7680, 4320, 24_000, 24_000),
            (3840, 2160, 1000, 1000),
            (1920, 1080, 100, 100),
        ] {
            let mut c = SendCongestion::new(limit, 30);
            c.set_resolution(width, height);
            for second in 0..60 {
                c.on_feedback(
                    report(second + 1, 3, Some(30)),
                    now + Duration::from_secs(second),
                );
                assert!((floor..=limit).contains(&c.target().kbps));
            }
            assert_eq!(
                c.target(),
                Target {
                    fps: 10,
                    kbps: floor
                }
            );
        }
    }

    #[test]
    fn larger_resolution_restores_quality_immediately_and_smaller_output_can_adapt_again() {
        let now = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        c.set_resolution(1280, 720);
        for second in 0..30 {
            c.on_feedback(
                report(second + 1, 3, Some(30)),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(c.target().kbps, 1000);
        c.applied_successfully(c.target());
        c.attempted(now + Duration::from_secs(30));
        c.set_resolution(3840, 2160);
        assert_eq!(c.target().kbps, 8000);
        assert_eq!(c.applied().kbps, 1000);
        assert!(c.reconfigure_due(now + Duration::from_secs(30)));
        // Cropping/scaling back down changes the floor without forcing a drop.
        c.set_resolution(320, 240);
        assert_eq!(c.target().kbps, 8000);
        for second in 30..60 {
            c.on_feedback(
                report(second + 1, 3, Some(30)),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(c.target().kbps, 500);
    }

    #[test]
    fn fec_is_reserved_above_the_quality_floor_without_exceeding_the_user_budget() {
        let now = Instant::now();
        for (limit, expected) in [(8000, 2000), (1000, 800)] {
            let mut c = SendCongestion::new(limit, 30);
            c.set_resolution(1920, 1080);
            for second in 0..60 {
                let mut feedback = report(second + 1, 3, Some(30));
                feedback.fec = vec![pb::FecFeedback {
                    ratio_permille: 250,
                    ..Default::default()
                }];
                c.on_feedback(feedback, now + Duration::from_secs(second));
                assert!(c.target().kbps >= expected);
                assert!(u64::from(c.target().kbps) * 1250 <= u64::from(limit) * 1000);
            }
            assert_eq!(c.target().kbps, expected);
        }
    }

    #[test]
    fn background_throttle_can_go_below_the_floor_and_foreground_restores_it() {
        let now = Instant::now();
        let mut c = SendCongestion::new(8000, 30);
        c.set_resolution(1920, 1080);
        c.set_background(true);
        for second in 0..30 {
            c.on_feedback(
                report(second + 1, 3, Some(30)),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(c.target(), Target { fps: 5, kbps: 1333 });
        c.set_background(false);
        assert_eq!(
            c.target(),
            Target {
                fps: 10,
                kbps: 2000
            }
        );
    }
}
