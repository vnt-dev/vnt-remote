//! Audio packet A/B transport comparison. Uses 80-byte synthetic encoded payloads;
//! no recording, decoding, playback, or audio device is opened.
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Loss {
    Random(u32),
    Burst3,
}
impl Loss {
    fn name(self) -> String {
        match self {
            Self::Random(p) => format!("random_{}pct", p / 10),
            Self::Burst3 => "burst_3_of_100".into(),
        }
    }
    fn drop(self, state: &mut u64, ordinal: u64) -> bool {
        match self {
            Self::Random(p) => {
                *state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (*state >> 32) % 1000 < u64::from(p)
            }
            Self::Burst3 => ordinal % 100 < 3,
        }
    }
}
#[derive(Default)]
struct Stats {
    packets: u64,
    dropped: u64,
    bytes: u64,
    recovered: u64,
    ratios: Vec<u32>,
    original_drops: HashSet<u32>,
}
#[derive(Serialize)]
struct Outcome {
    scenario: String,
    fec: bool,
    source_packets: usize,
    received_packets: usize,
    arrival_pct: f64,
    udp_packets: u64,
    udp_dropped: u64,
    injected_loss_pct: f64,
    udp_send_kbps: f64,
    original_dropped_packets: usize,
    fec_repaired_measured_packets: usize,
    unrecovered_source_packets: usize,
    mean_arrival_ms: f64,
    p95_arrival_ms: f64,
    recovered_whole_run: u64,
    ratio_pct_mean: f64,
}
async fn measure(loss: Loss, enabled: bool) -> Outcome {
    use vnt_net::datagram::Datagram as UdpSocket;
    let seconds = |name: &str, default: u32| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let sample_start = 200 + seconds("FEC_AUDIO_COMPARE_SETTLE_SECONDS", 2) * 100;
    let sample_end = sample_start + seconds("FEC_AUDIO_COMPARE_SECONDS", 4) * 100;
    let _timer = vnt_net::timing::TimerResolution::acquire();
    let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let mut receiver = vnt_net::udp::UdpVideoReceiver::bind(relay_addr)
        .await
        .unwrap();
    receiver.set_session(7);
    receiver.set_fec(enabled);
    let target = SocketAddr::from(([127, 0, 0, 1], receiver.local_addr().unwrap().port()));
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(256);
    receiver.set_audio_sender(audio_tx);
    let fec = vnt_net::fec::Sender::negotiated(u32::from(enabled), true);
    let phase = Arc::new(AtomicU8::new(0));
    let stats = Arc::new(Mutex::new(Stats::default()));
    let proxy = {
        let phase = phase.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut bytes = vec![0; 65536];
            let mut rng = 0x8f31_427bu64;
            let mut ordinal = 0;
            loop {
                let (len, _) = relay.recv_from(&mut bytes).await.unwrap();
                let phase = phase.load(Ordering::Acquire);
                let drop = phase != 0 && loss.drop(&mut rng, ordinal);
                if phase != 0 {
                    ordinal += 1;
                }
                if phase == 2 {
                    let mut s = stats.lock().unwrap();
                    s.packets += 1;
                    s.dropped += u64::from(drop);
                    s.bytes += len as u64 + 28;
                    let packet = &bytes[..len];
                    let original =
                        if len > vnt_net::fec::HEADER + 2 && packet[10] == vnt_net::fec::FLAG {
                            if packet[33] < packet[34] {
                                let n = u16::from_be_bytes(
                                    packet[vnt_net::fec::HEADER..vnt_net::fec::HEADER + 2]
                                        .try_into()
                                        .unwrap(),
                                ) as usize;
                                packet.get(vnt_net::fec::HEADER + 2..vnt_net::fec::HEADER + 2 + n)
                            } else {
                                None
                            }
                        } else {
                            Some(packet)
                        };
                    if drop {
                        if let Some(frame) =
                            original.and_then(|p| vnt_proto::audio::decode_datagram(p, 7))
                        {
                            if (sample_start..sample_end).contains(&frame.sequence) {
                                s.original_drops.insert(frame.sequence);
                            }
                        }
                    }
                }
                if !drop {
                    relay.send_to(&bytes[..len], target).await.unwrap();
                }
            }
        })
    };
    let reader = {
        let fec = fec.clone();
        let stats = stats.clone();
        let phase = phase.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut sequence = 0;
            loop {
                tokio::select! {
                    _=tick.tick()=>{
                        sequence+=1;let reports=receiver.fec_reports(None);
                        if let Some(fec)=&fec{fec.feedback(&vnt_proto::pb::LinkFeedback{sequence,interval_ms:1000,rtt_ms:Some(1),fec:reports.clone(),..Default::default()});}
                        let mut s=stats.lock().unwrap();s.recovered+=reports.iter().map(|f|f.recovered).sum::<u64>();
                        if phase.load(Ordering::Acquire)==2{s.ratios.extend(reports.iter().map(|f|f.ratio_permille));}
                    }
                    result=receiver.recv_frame()=>{result.unwrap();}
                }
            }
        })
    };
    let sent = Arc::new(Mutex::new(HashMap::<u32, Instant>::new()));
    let received = Arc::new(Mutex::new(HashSet::<u32>::new()));
    let delays = Arc::new(Mutex::new(Vec::new()));
    let collector = {
        let sent = sent.clone();
        let received = received.clone();
        let delays = delays.clone();
        tokio::spawn(async move {
            while let Some(frame) = audio_rx.recv().await {
                assert_eq!(
                    frame.data,
                    vec![frame.sequence as u8; 80],
                    "FEC must preserve the entire encoded audio packet"
                );
                if (sample_start..sample_end).contains(&frame.sequence)
                    && received.lock().unwrap().insert(frame.sequence)
                {
                    let at = sent.lock().unwrap()[&frame.sequence];
                    delays
                        .lock()
                        .unwrap()
                        .push(at.elapsed().as_secs_f64() * 1000.0);
                }
            }
        })
    };
    let mut clock = tokio::time::interval(Duration::from_millis(10));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut start = None;
    for sequence in 0..sample_end {
        clock.tick().await;
        phase.store(
            if sequence < 200 {
                0
            } else if sequence < sample_start {
                1
            } else {
                2
            },
            Ordering::Release,
        );
        if sequence == sample_start {
            start = Some(Instant::now());
        }
        sent.lock().unwrap().insert(sequence, Instant::now());
        let frame = vnt_proto::pb::AudioFrame {
            generation: 1,
            sequence,
            timestamp_samples: u64::from(sequence) * 480,
            data: vec![sequence as u8; 80],
        };
        vnt_net::udp::send_audio(&tx, relay_addr, 7, &frame, fec.as_ref())
            .await
            .unwrap();
    }
    // Drain the last packet before ending the measured wire interval.
    tokio::time::sleep(Duration::from_millis(25)).await;
    phase.store(3, Ordering::Release);
    let seconds = start.unwrap().elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let stats = stats.lock().unwrap();
    let received_set = received.lock().unwrap();
    let repaired = stats.original_drops.intersection(&received_set).count();
    let received = received_set.len();
    let mut delays = delays.lock().unwrap().clone();
    delays.sort_by(f64::total_cmp);
    let outcome = Outcome {
        scenario: loss.name(),
        fec: enabled,
        source_packets: (sample_end - sample_start) as usize,
        received_packets: received,
        arrival_pct: received as f64 / f64::from(sample_end - sample_start) * 100.0,
        original_dropped_packets: stats.original_drops.len(),
        fec_repaired_measured_packets: repaired,
        unrecovered_source_packets: (sample_end - sample_start) as usize - received,
        udp_packets: stats.packets,
        udp_dropped: stats.dropped,
        injected_loss_pct: stats.dropped as f64 / stats.packets.max(1) as f64 * 100.0,
        udp_send_kbps: stats.bytes as f64 * 8.0 / seconds / 1000.0,
        mean_arrival_ms: delays.iter().sum::<f64>() / delays.len().max(1) as f64,
        p95_arrival_ms: delays.get(delays.len() * 95 / 100).copied().unwrap_or(0.0),
        recovered_whole_run: stats.recovered,
        ratio_pct_mean: stats.ratios.iter().map(|r| *r as f64).sum::<f64>()
            / stats.ratios.len().max(1) as f64
            / 10.0,
    };
    drop(stats);
    proxy.abort();
    reader.abort();
    collector.abort();
    outcome
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit audio UDP FEC A/B comparison; about 75 seconds"]
async fn audio_udp_loss_fec_comparison() {
    let path = std::env::var_os("FEC_AUDIO_COMPARE_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../target/fec-audio-comparison.json")
        });
    let mut outcomes = Vec::new();
    let cases = std::env::var("FEC_AUDIO_COMPARE_CASES").ok();
    for loss in [
        Loss::Random(0),
        Loss::Random(30),
        Loss::Random(100),
        Loss::Burst3,
    ] {
        if cases
            .as_ref()
            .is_some_and(|cases| !cases.split(',').any(|name| name.trim() == loss.name()))
        {
            continue;
        }
        for enabled in [false, true] {
            let outcome = measure(loss, enabled).await;
            eprintln!("FEC_AUDIO_AB {}", serde_json::to_string(&outcome).unwrap());
            outcomes.push(outcome);
            std::fs::write(&path, serde_json::to_string_pretty(&outcomes).unwrap()).unwrap();
        }
    }
    assert!(outcomes
        .iter()
        .filter(|o| o.fec)
        .any(|o| o.recovered_whole_run > 0));
    for outcome in &outcomes {
        assert_eq!(
            outcome.unrecovered_source_packets,
            outcome.original_dropped_packets - outcome.fec_repaired_measured_packets,
            "every missing or repaired source packet must be accounted for"
        );
        if outcome.scenario == "random_0pct" {
            assert_eq!(outcome.received_packets, outcome.source_packets);
        }
    }
}
