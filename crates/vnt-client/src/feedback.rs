use std::time::Duration;
use tokio::time::Instant;
use vnt_proto::pb;

/// Connection details aggregate fresh stream windows, while recovery is cumulative.
#[derive(Default)]
pub(super) struct FecTelemetry {
    recovered: u64,
    latest: std::collections::HashMap<(u32, bool), (pb::FecFeedback, Instant)>,
}

impl FecTelemetry {
    pub fn update(
        &mut self,
        session: u32,
        reports: &[pb::FecFeedback],
    ) -> Option<serde_json::Value> {
        if reports.is_empty() {
            return None;
        }
        let now = Instant::now();
        self.recovered += reports.iter().map(|f| f.recovered).sum::<u64>();
        for f in reports {
            self.latest.insert((f.stream_id, f.audio), (*f, now));
        }
        self.latest
            .retain(|_, (_, at)| now.duration_since(*at) <= Duration::from_secs(3));
        let reports: Vec<_> = self.latest.values().map(|(f, _)| f).collect();
        let expected = reports.iter().map(|f| f.expected).sum::<u64>();
        let received = reports.iter().map(|f| f.received).sum::<u64>();
        Some(serde_json::json!({
            "sessionId": session,
            "recovered": self.recovered,
            "rawLossPermille": (expected > 0).then(|| (expected-received)*1000/expected),
            "ratioPermille": reports.iter().map(|f|f.ratio_permille).max().unwrap_or(0),
            "streams": reports.iter().map(|f|serde_json::json!({
                "streamId":f.stream_id, "audio":f.audio, "ratioPermille":f.ratio_permille,
                "expected":f.expected, "received":f.received, "recovered":f.recovered,
                "residual":f.residual, "parityBytes":f.parity_bytes,
            })).collect::<Vec<_>>()
        }))
    }
}

/// A receive window advances even while UDP is completely silent. Lost-frame
/// counters wrap at u32; subtract them with the same wrapping arithmetic.
pub(super) struct ReceiveWindow {
    started: Instant,
    sequence: u64,
    frames: u32,
    bytes: u64,
    last_lost: u32,
    last_frame: Option<Instant>,
}

impl ReceiveWindow {
    pub fn new(now: Instant, lost: u32) -> Self {
        Self {
            started: now,
            sequence: 0,
            frames: 0,
            bytes: 0,
            last_lost: lost,
            last_frame: None,
        }
    }

    pub fn received(&mut self, now: Instant, bytes: usize) {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.last_frame = Some(now);
    }

    /// A new video generation resets counters, while the live worker still
    /// requires monotonically increasing feedback sequence numbers.
    pub fn reset_generation(&mut self, now: Instant, lost: u32) {
        let sequence = self.sequence;
        *self = Self::new(now, lost);
        self.sequence = sequence;
    }

    pub fn report(
        &mut self,
        now: Instant,
        lost: u32,
        rtt_ms: Option<u32>,
    ) -> Option<pb::LinkFeedback> {
        let elapsed = now.duration_since(self.started);
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        self.sequence += 1;
        let report = pb::LinkFeedback {
            fec: Vec::new(),
            sequence: self.sequence,
            interval_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
            received_frames: self.frames,
            lost_frames: lost.wrapping_sub(self.last_lost),
            received_bytes: self.bytes,
            frame_gap_ms: self
                .last_frame
                .map(|last| now.duration_since(last).as_millis().min(60_000) as u32),
            rtt_ms,
        };
        self.started = now;
        self.frames = 0;
        self.bytes = 0;
        self.last_lost = lost;
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn generation_changes_preserve_feedback_order_without_carrying_old_loss() {
        let start = Instant::now();
        let mut window = ReceiveWindow::new(start, 0);
        window.received(start, 100);
        let first = window
            .report(start + Duration::from_secs(1), 2, Some(10))
            .unwrap();
        window.reset_generation(start + Duration::from_secs(1), 0);
        window.received(start + Duration::from_secs(1), 200);
        let next = window
            .report(start + Duration::from_secs(2), 0, Some(10))
            .unwrap();
        assert_eq!(next.sequence, first.sequence + 1);
        assert_eq!(next.lost_frames, 0);
        assert_eq!(next.received_frames, 1);
        assert_eq!(next.received_bytes, 200);
    }

    #[tokio::test(start_paused = true)]
    async fn fec_details_aggregate_fresh_streams_without_double_counting_recovery() {
        let mut t = FecTelemetry::default();
        let a = pb::FecFeedback {
            stream_id: 1,
            expected: 100,
            received: 90,
            recovered: 10,
            ratio_permille: 100,
            ..Default::default()
        };
        let b = pb::FecFeedback {
            stream_id: 2,
            expected: 300,
            received: 300,
            ratio_permille: 50,
            ..Default::default()
        };
        assert_eq!(t.update(7, &[a]).unwrap()["recovered"], 10);
        let value = t.update(7, std::slice::from_ref(&b)).unwrap();
        assert_eq!(value["rawLossPermille"], 25);
        assert_eq!(value["recovered"], 10);
        assert_eq!(value["ratioPermille"], 100);
        tokio::time::advance(Duration::from_secs(4)).await;
        let value = t.update(7, &[b]).unwrap();
        assert_eq!(value["rawLossPermille"], 0);
        assert_eq!(value["streams"].as_array().unwrap().len(), 1);
        assert_eq!(value["recovered"], 10);
    }

    #[tokio::test(start_paused = true)]
    async fn windows_count_complete_frames_and_report_during_total_udp_silence() {
        let start = Instant::now();
        let mut window = ReceiveWindow::new(start, u32::MAX);
        assert!(window.report(start, 0, None).is_none());
        window.received(start, 200);
        window.received(start + Duration::from_millis(30), 300);
        let first = window
            .report(start + Duration::from_secs(1), 1, Some(20))
            .unwrap();
        assert_eq!(
            (
                first.sequence,
                first.received_frames,
                first.received_bytes,
                first.lost_frames
            ),
            (1, 2, 500, 2)
        );
        assert_eq!(first.frame_gap_ms, Some(970));
        let second = window
            .report(start + Duration::from_secs(2), 1, None)
            .unwrap();
        assert_eq!(
            (
                second.sequence,
                second.received_frames,
                second.received_bytes,
                second.lost_frames
            ),
            (2, 0, 0, 0)
        );
        assert_eq!(second.frame_gap_ms, Some(1970));
        assert_eq!(second.rtt_ms, None);
        let mut unbound = ReceiveWindow::new(start, 0);
        assert_eq!(
            unbound
                .report(start + Duration::from_secs(1), 0, None)
                .unwrap()
                .frame_gap_ms,
            None
        );
    }
}
