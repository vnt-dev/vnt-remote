//! Exercise the actual UDP/FEC path and reliable framed NACK control channel.
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, RwLock};
use vnt_net::datagram::Datagram as UdpSocket;
use vnt_net::{
    repair::VideoRepair,
    tcp::ControlChannel,
    udp::{UdpFrameSender, UdpVideoReceiver},
};
use vnt_proto::{control::ControlMessage as Msg, pb, recovery, udp::FragHeader};

struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn frame(id: u64, references: Vec<u64>) -> pb::VideoFrame {
    pb::VideoFrame {
        keyframe: id == 1,
        pts_ms: id as u32 * 16,
        data: vec![id as u8; 9000],
        dependencies: Some(pb::VideoDependencies {
            epoch: 1,
            frame_id: id,
            references,
            independent: id == 1,
            unknown: false,
        }),
    }
}

async fn recover_over_relay(fec: bool, whole: bool, routed: bool) {
    let server = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let mut receiver = UdpVideoReceiver::bind(relay_addr).await.unwrap();
    receiver.set_session(7);
    receiver.set_rtt(Some(2));
    receiver.set_fec(fec);
    let client_addr =
        std::net::SocketAddr::from(([127, 0, 0, 1], receiver.local_addr().unwrap().port()));
    let peer = Arc::new(RwLock::new(Some(relay_addr)));
    let (repair, _worker) = VideoRepair::start(server.clone(), peer);
    let (events, mut event_rx) = mpsc::channel(128);
    receiver.set_recovery_sender(events);
    let (a, b) = tokio::io::duplex(65536);
    let mut a = ControlChannel::new(a);
    let mut b = ControlChannel::new(b);
    let _control = Task(tokio::spawn(async move {
        while let Some(msg) = event_rx.recv().await {
            a.send_owned(msg).await.unwrap();
        }
    }));
    let idrs = Arc::new(AtomicUsize::new(0));
    let count = idrs.clone();
    let repairs = Arc::new(AtomicUsize::new(0));
    let received_repairs = repairs.clone();
    let repair_handler = repair.clone();
    let _requests = Task(tokio::spawn(async move {
        while let Some(msg) = b.recv().await.unwrap() {
            match msg {
                Msg::VideoNack(n) => {
                    received_repairs.fetch_add(1, Ordering::Relaxed);
                    repair_handler.request(n);
                }
                Msg::RequestKeyframe | Msg::ScreenPacket(_) => {
                    count.fetch_add(1, Ordering::Relaxed);
                }
                other => panic!("unexpected control: {other:?}"),
            }
        }
    }));
    let _relay = Task(tokio::spawn(async move {
        let mut buf = vec![0; 65536];
        loop {
            let (len, _) = relay.recv_from(&mut buf).await.unwrap();
            let packet = &buf[..len];
            let original = if packet[10] & vnt_net::fec::FLAG != 0 {
                // Deliberately lose parity as well: FEC alone cannot recover
                // this burst, but the independent repair envelope still can.
                if packet[33] >= packet[34] {
                    continue;
                }
                &packet[vnt_net::fec::HEADER + 2..]
            } else {
                packet
            };
            let (h, _) = FragHeader::decode(original).unwrap();
            let repair = h.flags & recovery::FLAG_REPAIR != 0;
            if h.frame_seq == 0 && !repair && (whole || (1..=4).contains(&h.frag_idx)) {
                continue;
            }
            relay.send_to(packet, client_addr).await.unwrap();
            // Duplicated repairs must never duplicate decoder delivery.
            if repair {
                relay.send_to(packet, client_addr).await.unwrap();
            }
        }
    }));
    let receive = tokio::spawn(async move {
        for id in 1..=3 {
            let (stream, generation, f) = receiver.recv_screen_frame().await.unwrap().unwrap();
            assert_eq!((stream, generation), if routed { (2, 3) } else { (0, 0) });
            assert_eq!(f.data, vec![id as u8; 9000]);
        }
        assert_eq!(
            receiver
                .recovery_stats(if routed { 2 } else { 0 })
                .lost_frames,
            0
        );
        assert_eq!(
            receiver
                .recovery_stats(if routed { 2 } else { 0 })
                .recovered_frames,
            1
        );
    });
    let mut sender = UdpFrameSender::default();
    sender.set_repair(Some(repair));
    sender.set_fec(vnt_net::fec::Sender::negotiated(u32::from(fec), true));
    for id in 1..=3u64 {
        let f = frame(id, if id == 1 { vec![] } else { vec![id - 1] });
        let sequence = (id as u16).wrapping_sub(2); // MAX -> 0 -> 1
        if routed {
            let Msg::ScreenPacket(packet) = vnt_proto::screen::wrap(2, 3, Msg::VideoFrame(f))
            else {
                unreachable!()
            };
            sender
                .send_screen_frame(&server, relay_addr, 7, sequence, &packet, 100_000)
                .await
                .unwrap();
        } else {
            sender
                .send_video_frame(&server, relay_addr, 7, sequence, &f, 100_000)
                .await
                .unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(2), receive)
        .await
        .expect("repair deadline")
        .unwrap();
    assert!(repairs.load(Ordering::Relaxed) >= 1);
    assert_eq!(idrs.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn fragments_and_whole_frames_recover_with_and_without_fec_on_screen_routes() {
    for fec in [false, true] {
        for whole in [false, true] {
            for routed in [false, true] {
                recover_over_relay(fec, whole, routed).await;
            }
        }
    }
}

#[tokio::test]
async fn permanent_loss_only_requests_idr_when_a_later_frame_uses_the_missing_reference() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut receiver = UdpVideoReceiver::bind(server.local_addr().unwrap())
        .await
        .unwrap();
    receiver.set_session(7);
    receiver.set_rtt(Some(2));
    let client =
        std::net::SocketAddr::from(([127, 0, 0, 1], receiver.local_addr().unwrap().port()));
    let (tx, mut control) = mpsc::channel(128);
    receiver.set_recovery_sender(tx);
    let (safe_tx, safe_rx) = tokio::sync::oneshot::channel();
    let receive = tokio::spawn(async move {
        assert_eq!(receiver.recv_frame().await.unwrap().unwrap().frame_seq, 1);
        assert_eq!(receiver.recv_frame().await.unwrap().unwrap().frame_seq, 3);
        safe_tx.send(()).unwrap();
        assert_eq!(receiver.recv_frame().await.unwrap().unwrap().frame_seq, 5);
        assert_eq!(receiver.recovery_stats(0).lost_frames, 1);
        assert_eq!(receiver.recovery_stats(0).skipped_frames, 1);
        assert_eq!(receiver.recovery_stats(0).idr_requests, 1);
    });
    let mut sender = UdpFrameSender::default();
    // Frame two is wholly absent. Frame three still uses the intact first frame.
    sender
        .send_video_frame(&server, client, 7, 1, &frame(1, vec![]), 100_000)
        .await
        .unwrap();
    sender
        .send_video_frame(&server, client, 7, 3, &frame(3, vec![1]), 100_000)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), safe_rx)
        .await
        .unwrap()
        .unwrap();
    while let Ok(message) = control.try_recv() {
        assert!(matches!(message, Msg::VideoNack(_)));
    }
    // This complete frame uses frame two and must never reach the decoder.
    sender
        .send_video_frame(&server, client, 7, 4, &frame(4, vec![2]), 100_000)
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(control.recv().await.unwrap(), Msg::RequestKeyframe) {
                break;
            }
        }
    })
    .await;
    assert!(request.is_ok());
    let mut reset = frame(5, vec![]);
    reset.keyframe = true;
    reset.dependencies.as_mut().unwrap().independent = true;
    sender
        .send_video_frame(&server, client, 7, 5, &reset, 100_000)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), receive)
        .await
        .unwrap()
        .unwrap();
}
