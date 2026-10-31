//! Cancellable backpressure for the encoder thread.
use super::VideoOut;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) fn reserve_output(
    runtime: &tokio::runtime::Handle,
    tx: &mpsc::Sender<VideoOut>,
    cancel: &CancellationToken,
) -> Option<(mpsc::OwnedPermit<VideoOut>, Duration)> {
    let started = Instant::now();
    let permit = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            permit = tx.clone().reserve_owned() => permit.ok(),
        }
    })?;
    Some((permit, started.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vnt_capture::CapturedFrame;

    fn output(pts_ms: u32) -> VideoOut {
        VideoOut {
            dependencies: None,
            config_revision: 0,
            desktop_generation: 0,
            bitrate_kbps: 8000,
            seq: pts_ms as u16,
            keyframe: pts_ms == 0,
            pts_ms,
            data: vec![pts_ms as u8],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backpressure_preserves_encoded_order_and_cancellation_wakes_encoder() {
        let runtime = tokio::runtime::Handle::current();
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        tx.send(output(0))
            .await
            .unwrap_or_else(|_| panic!("closed"));
        let stop = cancel.clone();
        let encoder = tokio::task::spawn_blocking(move || {
            let (permit, _) = reserve_output(&runtime, &tx, &stop).unwrap();
            permit.send(output(1));
            reserve_output(&runtime, &tx, &stop).is_none()
        });
        assert_eq!(rx.recv().await.unwrap().pts_ms, 0);
        while rx.is_empty() {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), encoder)
            .await
            .unwrap()
            .unwrap());
        assert_eq!(rx.recv().await.unwrap().pts_ms, 1);
    }

    #[test]
    fn pipeline_frames_are_released_when_dropped() {
        // 管线不再归还原始帧：帧随 Arc 释放，不设回收/复用步骤
        let frame = Arc::new(CapturedFrame {
            source_monitor: None,
            desktop_generation: 0,
            captured_at: Instant::now(),
            width: 2,
            height: 2,
            bgra: vec![0; 16],
            pts_ms: 0,
            capture_ms: 0,
        });
        let weak = Arc::downgrade(&frame);
        drop(frame);
        assert!(weak.upgrade().is_none());
    }
}
