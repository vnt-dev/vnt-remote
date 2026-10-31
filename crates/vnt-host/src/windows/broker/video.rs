//! Pace video independently of control traffic and recover safely after queue/socket pressure.
use std::{
    io,
    time::{Duration, Instant},
};

const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct VideoRecovery {
    waiting: bool,
    last_request: Option<Instant>,
}

impl VideoRecovery {
    pub(super) fn dropped(&mut self) {
        self.waiting = true;
    }
    pub(super) fn accepts(&self, keyframe: bool) -> bool {
        !self.waiting || keyframe
    }
    pub(super) fn delivered(&mut self, keyframe: bool) {
        if keyframe {
            self.waiting = false;
        }
    }
    pub(super) fn request_due(&mut self, now: Instant) -> bool {
        if !self.waiting
            || self
                .last_request
                .is_some_and(|last| now.duration_since(last) < KEYFRAME_RETRY)
        {
            return false;
        }
        self.last_request = Some(now);
        true
    }
}

struct QueuedVideo {
    frame: vnt_proto::pb::VideoFrame,
    target: std::net::SocketAddr,
    seq: u16,
    peak_kbps: u32,
}

/// Keep pacing independent of the reliable control/keyboard/audio loop. A small
/// queue bounds stale video; sequence gaps suppress deltas until the next IDR.
pub(super) struct VideoSender {
    tx: tokio::sync::mpsc::Sender<QueuedVideo>,
    pub(super) events: tokio::sync::mpsc::Receiver<io::Result<bool>>,
    sequence: std::sync::Arc<std::sync::Mutex<vnt_net::udp::VideoSequence>>,
    pub(super) sent: std::sync::Arc<std::sync::Mutex<(u64, u64)>>,
    task: tokio::task::JoinHandle<()>,
    fec: Option<vnt_net::fec::Sender>,
    repair: Option<vnt_net::repair::VideoRepair>,
}

impl Drop for VideoSender {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl VideoSender {
    #[cfg(test)]
    pub(super) fn new(
        socket: std::sync::Arc<vnt_net::datagram::Datagram>,
        session_id: u32,
    ) -> Self {
        Self::with_fec(socket, session_id, None)
    }

    #[cfg(test)]
    pub(super) fn with_fec(
        socket: std::sync::Arc<vnt_net::datagram::Datagram>,
        session_id: u32,
        fec: Option<vnt_net::fec::Sender>,
    ) -> Self {
        Self::with_sequence(
            socket,
            session_id,
            Default::default(),
            Default::default(),
            fec,
            None,
        )
    }

    pub(super) fn with_repair(
        socket: std::sync::Arc<vnt_net::datagram::Datagram>,
        session_id: u32,
        fec: Option<vnt_net::fec::Sender>,
        repair: Option<vnt_net::repair::VideoRepair>,
    ) -> Self {
        Self::with_sequence(
            socket,
            session_id,
            Default::default(),
            Default::default(),
            fec,
            repair,
        )
    }
    fn with_sequence(
        socket: std::sync::Arc<vnt_net::datagram::Datagram>,
        session_id: u32,
        sequence: std::sync::Arc<std::sync::Mutex<vnt_net::udp::VideoSequence>>,
        sent: std::sync::Arc<std::sync::Mutex<(u64, u64)>>,
        fec: Option<vnt_net::fec::Sender>,
        repair: Option<vnt_net::repair::VideoRepair>,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<QueuedVideo>(2);
        let (events_tx, events) = tokio::sync::mpsc::channel(2);
        let wire_sequence = sequence.clone();
        let sent_stats = sent.clone();
        let sender_fec = fec.clone();
        let sender_repair = repair.clone();
        let task = tokio::spawn(async move {
            let mut sender = vnt_net::udp::UdpFrameSender::default();
            sender.set_fec(sender_fec);
            sender.set_repair(sender_repair);
            let mut recovery = VideoRecovery::default();
            let mut expected = None;
            while let Some(video) = rx.recv().await {
                if video.frame.dependencies.is_none()
                    && expected.is_some_and(|seq| seq != video.seq)
                {
                    recovery.dropped();
                }
                expected = Some(video.seq.wrapping_add(1));
                if video.frame.dependencies.is_none() && !recovery.accepts(video.frame.keyframe) {
                    if events_tx.send(Ok(false)).await.is_err() {
                        break;
                    }
                    continue;
                }
                let wire_seq = wire_sequence.lock().unwrap().take();
                let result = sender
                    .send_video_frame(
                        &socket,
                        video.target,
                        session_id,
                        wire_seq,
                        &video.frame,
                        video.peak_kbps,
                    )
                    .await;
                match result {
                    Ok(_) => {
                        {
                            let mut sent = sent_stats.lock().unwrap();
                            sent.0 += 1;
                            sent.1 += video.frame.data.len() as u64;
                        }
                        recovery.delivered(video.frame.keyframe);
                        if video.frame.keyframe && events_tx.send(Ok(true)).await.is_err() {
                            break;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        if video.frame.dependencies.is_some() {
                            continue;
                        }
                        recovery.dropped();
                        if events_tx.send(Ok(false)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = events_tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });
        Self {
            repair,
            fec,
            tx,
            events,
            task,
            sequence,
            sent,
        }
    }

    /// Flush old-resolution/desktop frames before publishing a stream reset.
    /// Await cancellation so no fragment can be sent after the reset is published.
    pub(super) async fn reset(
        &mut self,
        socket: std::sync::Arc<vnt_net::datagram::Datagram>,
        session_id: u32,
    ) {
        self.task.abort();
        let _ = (&mut self.task).await;
        *self = Self::with_sequence(
            socket,
            session_id,
            self.sequence.clone(),
            self.sent.clone(),
            self.fec.clone(),
            self.repair.clone(),
        );
    }

    /// Park only the media reader, leaving the Broker's input/control loop live.
    pub(super) fn capacity_waiter(
        &self,
    ) -> impl std::future::Future<Output = io::Result<()>> + use<> {
        let tx = self.tx.clone();
        async move {
            tx.reserve()
                .await
                .map(|_| ())
                .map_err(|_| io::Error::other("UDP video sender ended"))
        }
    }

    pub(super) fn try_send(
        &self,
        frame: vnt_proto::pb::VideoFrame,
        target: std::net::SocketAddr,
        seq: u16,
        peak_kbps: u32,
    ) -> bool {
        let sent = self
            .tx
            .try_send(QueuedVideo {
                frame,
                target,
                seq,
                peak_kbps,
            })
            .is_ok();
        if !sent {
            if let Some(fec) = &self.fec {
                fec.pressure();
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_sender_detects_queue_gaps_and_recovers_only_with_an_idr() {
        use vnt_net::datagram::Datagram as UdpSocket;
        use vnt_net::udp::UdpVideoReceiver;
        let socket = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut receiver = UdpVideoReceiver::bind(socket.local_addr().unwrap())
            .await
            .unwrap();
        receiver.set_session(7);
        let target =
            std::net::SocketAddr::from(([127, 0, 0, 1], receiver.local_addr().unwrap().port()));
        let mut sender = VideoSender::new(socket, 7);
        let frame = |keyframe| vnt_proto::pb::VideoFrame {
            dependencies: None,
            keyframe,
            pts_ms: 0,
            data: vec![42; 100],
        };
        // No yield: the worker has not run, so exactly two frames fit.
        assert!(sender.try_send(frame(true), target, 0, 4000));
        assert!(sender.try_send(frame(false), target, 1, 4000));
        assert!(!sender.try_send(frame(false), target, 2, 4000));
        assert!(sender.events.recv().await.unwrap().unwrap());
        for seq in [0, 1] {
            let received = tokio::time::timeout(Duration::from_secs(2), receiver.recv_frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(received.frame_seq, seq);
        }
        assert!(sender.try_send(frame(false), target, 3, 4000));
        assert!(
            !sender.events.recv().await.unwrap().unwrap(),
            "gap must reject dependent frames"
        );
        assert!(sender.try_send(frame(true), target, 4, 4000));
        assert!(sender.events.recv().await.unwrap().unwrap());
        let received = tokio::time::timeout(Duration::from_secs(2), receiver.recv_frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(received.frame_seq, 2);
        assert!(received.keyframe);
        assert_eq!(
            receiver.stats.lost_frames, 0,
            "local omissions are not network loss"
        );
        sender.task.abort();
        assert!(sender.events.recv().await.is_none());
    }

    #[tokio::test]
    async fn stream_reset_cancels_the_paced_frame_and_discards_queued_old_video() {
        use vnt_net::datagram::Datagram as UdpSocket;
        use vnt_net::udp::UdpVideoReceiver;
        let socket = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut receiver = UdpVideoReceiver::bind(socket.local_addr().unwrap())
            .await
            .unwrap();
        receiver.set_session(7);
        let target =
            std::net::SocketAddr::from(([127, 0, 0, 1], receiver.local_addr().unwrap().port()));
        let mut sender = VideoSender::new(socket.clone(), 7);
        assert!(sender.try_send(
            vnt_proto::pb::VideoFrame {
                dependencies: None,
                keyframe: true,
                pts_ms: 0,
                data: vec![1; 2 * 1024 * 1024],
            },
            target,
            0,
            200
        ));
        assert!(sender.try_send(
            vnt_proto::pb::VideoFrame {
                dependencies: None,
                keyframe: true,
                pts_ms: 1,
                data: vec![2; 100],
            },
            target,
            1,
            200
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;
        sender.reset(socket, 7).await;
        assert!(sender.try_send(
            vnt_proto::pb::VideoFrame {
                dependencies: None,
                keyframe: true,
                pts_ms: 2,
                data: vec![3; 100],
            },
            target,
            2,
            4000
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(2), sender.events.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), receiver.recv_frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.frame_seq, 1,
            "reset must preserve the next wire sequence"
        );
        assert_eq!(frame.data, vec![3; 100]);
    }

    #[test]
    fn only_a_successfully_delivered_keyframe_recovers_with_throttled_retries() {
        let mut recovery = VideoRecovery::default();
        let now = Instant::now();
        assert!(recovery.accepts(false));
        recovery.dropped();
        assert!(!recovery.accepts(false));
        assert!(recovery.accepts(true));
        assert!(recovery.request_due(now));
        recovery.dropped(); // A replacement IDR can also fail; do not start an IDR storm.
        assert!(!recovery.request_due(now + Duration::from_millis(499)));
        assert!(recovery.request_due(now + KEYFRAME_RETRY));
        recovery.delivered(false);
        assert!(!recovery.accepts(false));
        recovery.delivered(true);
        assert!(recovery.accepts(false));
        assert!(!recovery.request_due(now + KEYFRAME_RETRY * 2));
    }
}
