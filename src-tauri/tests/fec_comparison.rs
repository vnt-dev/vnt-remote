//! Explicit, content-independent loss injection for an end-to-end FEC A/B run.
//! cargo test -p vnt-remote --test fec_comparison -- --ignored --nocapture --test-threads=1
use futures_util::StreamExt;
use serde::Serialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::Message;
use vnt_remote_lib::{client, server, EventSink, NoopEventSink};

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
        // Neither rule looks at flags, frame boundaries, shard indices or FEC.
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
struct Events(Mutex<Vec<(Instant, String, serde_json::Value)>>);
impl EventSink for Events {
    fn emit_json(&self, event: &str, payload: String) {
        if matches!(event, "client://fec" | "client://stats" | "client://closed") {
            self.0.lock().unwrap().push((
                Instant::now(),
                event.into(),
                serde_json::from_str(&payload).unwrap(),
            ));
        }
    }
}
#[derive(Clone)]
struct Frame {
    pts: u32,
    keyframe: bool,
    measured: bool,
}
#[derive(Default)]
struct RelayStats {
    packets: u64,
    dropped: u64,
    bytes: u64,
    original_packets: u64,
    original_dropped: u64,
    parity_packets: u64,
    parity_bytes: u64,
    seen: HashSet<u32>,
    frames: Vec<Frame>,
}
#[derive(Serialize)]
struct Outcome {
    scenario: String,
    fec: bool,
    seed: u64,
    codec: String,
    hardware: bool,
    width: u32,
    height: u32,
    measured_seconds: f64,
    udp_packets: u64,
    udp_dropped: u64,
    injected_loss_pct: f64,
    original_packets: u64,
    original_dropped: u64,
    parity_packets: u64,
    parity_bytes: u64,
    udp_send_mbps: f64,
    sent_frames: usize,
    complete_frames: usize,
    complete_pct: f64,
    complete_fps: f64,
    reference_chain_fps_estimate: f64,
    applied_fps_mean: f64,
    applied_kbps_mean: f64,
    fec_ratio_pct_mean: f64,
    recovered_fragments_whole_run: u64,
    disconnect_reason: Option<String>,
}

fn original(packet: &[u8]) -> Option<&[u8]> {
    if *packet.get(10)? != vnt_net::fec::FLAG {
        return Some(packet);
    }
    if packet.len() < vnt_net::fec::HEADER + 2 || packet[33] >= packet[34] {
        return None;
    }
    let len = u16::from_be_bytes(
        packet[vnt_net::fec::HEADER..vnt_net::fec::HEADER + 2]
            .try_into()
            .ok()?,
    ) as usize;
    packet.get(vnt_net::fec::HEADER + 2..vnt_net::fec::HEADER + 2 + len)
}

async fn relay_sockets() -> (tokio::net::TcpListener, tokio::net::UdpSocket, u16) {
    for _ in 0..128 {
        let mut random = [0; 2];
        getrandom::fill(&mut random).unwrap();
        let port = 20000 + u16::from_be_bytes(random) % 30000;
        let Ok(tcp) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
            continue;
        };
        if let Ok(udp) = tokio::net::UdpSocket::bind(("127.0.0.1", port)).await {
            socket2::SockRef::from(&udp)
                .set_recv_buffer_size(16 * 1024 * 1024)
                .unwrap();
            return (tcp, udp, port);
        }
    }
    panic!("no free TCP/UDP relay port");
}

async fn measure(loss: Loss, fec: bool) -> Outcome {
    let seed = std::env::var("FEC_COMPARE_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x8f31_427bu64);
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
    let host = server::start_with_capture(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            password: "fec-compare".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: clipboard.clone(),
        },
        vnt_capture::SharedCapture::synthetic(1280, 720),
    )
    .await
    .unwrap();
    let backend = SocketAddr::from(([127, 0, 0, 1], host.port));
    let (tcp, udp, port) = relay_sockets().await;
    let phase = Arc::new(AtomicU8::new(0));
    let relay_stats = Arc::new(Mutex::new(RelayStats::default()));
    let udp_task = {
        let phase = phase.clone();
        let stats = relay_stats.clone();
        tokio::spawn(async move {
            let mut controller = None;
            let mut buffer = vec![0; 65536];
            let mut rng = seed;
            let mut ordinal = 0;
            loop {
                let (len, from) = udp.recv_from(&mut buffer).await.unwrap();
                let packet = &buffer[..len];
                if from != backend {
                    controller = Some(from);
                    udp.send_to(packet, backend).await.unwrap();
                    continue;
                }
                let Some(target) = controller else {
                    continue;
                };
                let phase = phase.load(Ordering::Acquire);
                let drop = phase != 0 && loss.drop(&mut rng, ordinal);
                if phase != 0 {
                    ordinal += 1;
                }
                let original = original(packet);
                {
                    let mut stats = stats.lock().unwrap();
                    if let Some((header, _)) = original.and_then(vnt_proto::udp::FragHeader::decode)
                    {
                        if header.flags & vnt_proto::audio::FLAG_AUDIO == 0
                            && stats.seen.insert(header.pts_ms)
                        {
                            stats.frames.push(Frame {
                                pts: header.pts_ms,
                                keyframe: header.is_keyframe(),
                                measured: phase == 2,
                            });
                        }
                    }
                    if phase == 2 {
                        stats.packets += 1;
                        stats.bytes += len as u64 + 28; // IPv4 + UDP headers
                        stats.dropped += u64::from(drop);
                        if original.is_some() {
                            stats.original_packets += 1;
                            stats.original_dropped += u64::from(drop);
                        } else {
                            stats.parity_packets += 1;
                            stats.parity_bytes += len as u64;
                        }
                    }
                }
                if !drop {
                    udp.send_to(packet, target).await.unwrap();
                }
            }
        })
    };
    let tcp_task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted=tcp.accept()=>{let (mut a,_)=accepted.unwrap();connections.spawn(async move {let mut b=tokio::net::TcpStream::connect(backend).await.unwrap();let _=tokio::io::copy_bidirectional(&mut a,&mut b).await;});}
                Some(_)=connections.join_next(),if !connections.is_empty()=>{}
            }
        }
    });
    let events = Arc::new(Events::default());
    let (connection, info) = client::connect_split(
        events.clone(),
        client::ConnectParams {
            clipboard_enabled: true,
            server_config_id: None,
            server_node_id: None,
            server_network_code: None,
            hardware_codecs: Vec::new(),
            color_depth: 24,
            show_wallpaper: true,
            drag_full_windows: false,
            fec_enabled: fec,
            resolution_mode: "none".into(),
            target_width: 0,
            target_height: 0,
            audio_device_id: None,
            audio_enabled: false,
            transport: vnt_proto::Transport::Hybrid,
            host: "127.0.0.1".into(),
            port,
            password: "fec-compare".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        clipboard,
    )
    .await
    .unwrap();
    assert_eq!(info.fec_enabled, fec);
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/?t={}",
        info.ws_port, info.token
    ))
    .await
    .unwrap();
    let seconds = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    let settle = if matches!(loss, Loss::Random(0)) {
        Duration::ZERO
    } else {
        Duration::from_secs(seconds("FEC_COMPARE_SETTLE_SECONDS", 6))
    };
    let warmup = Duration::from_secs(2);
    let sample = Duration::from_secs(seconds("FEC_COMPARE_SECONDS", 6));
    let drain = Duration::from_secs(1);
    let start = Instant::now();
    let mut measured_start = None;
    let mut measured_end = None;
    let mut received = HashSet::new();
    while start.elapsed() < warmup + settle + sample + drain {
        let elapsed = start.elapsed();
        let next = if elapsed < warmup {
            0
        } else if elapsed < warmup + settle {
            1
        } else if elapsed < warmup + settle + sample {
            2
        } else {
            3
        };
        let previous = phase.swap(next, Ordering::AcqRel);
        if next == 2 && previous != 2 {
            measured_start = Some(Instant::now());
        }
        if next == 3 && previous == 2 {
            measured_end = Some(Instant::now());
        }
        match tokio::time::timeout(Duration::from_millis(50), ws.next()).await {
            Ok(Some(Ok(Message::Binary(frame)))) => {
                let header = 8;
                if frame.len() >= header + 9 {
                    let size = u32::from_be_bytes(frame[header + 5..header + 9].try_into().unwrap())
                        as usize;
                    assert_eq!(frame.len(), header + 9 + size);
                    received.insert(u32::from_be_bytes(
                        frame[header + 1..header + 5].try_into().unwrap(),
                    ));
                }
            }
            Ok(None) | Ok(Some(Err(_))) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            _ => {}
        }
    }
    phase.store(3, Ordering::Release);
    let measured_start = measured_start.unwrap();
    let measured_end = measured_end.unwrap();
    let seconds = measured_end.duration_since(measured_start).as_secs_f64();
    let outcome = {
        let stats = relay_stats.lock().unwrap();
        let sent = stats.frames.iter().filter(|f| f.measured).count();
        let complete = stats
            .frames
            .iter()
            .filter(|f| f.measured && received.contains(&f.pts))
            .count();
        // Conservative reference-chain estimate, not a real WebCodecs decoder:
        // any missing reference frame invalidates deltas until the next complete IDR.
        let mut reference = false;
        let mut chain = 0;
        for frame in &stats.frames {
            if !received.contains(&frame.pts) {
                reference = false;
            } else {
                if frame.keyframe {
                    reference = true;
                }
                if reference && frame.measured {
                    chain += 1;
                }
            }
        }
        let events = events.0.lock().unwrap();
        let mean = |event: &str, field: &str| {
            let values: Vec<_> = events
                .iter()
                .filter(|(at, kind, _)| {
                    kind == event && *at >= measured_start && *at < measured_end
                })
                .filter_map(|(_, _, v)| v[field].as_f64())
                .collect();
            if values.is_empty() {
                0.0
            } else {
                values.iter().sum::<f64>() / values.len() as f64
            }
        };
        let outcome = Outcome {
            scenario: loss.name(),
            fec,
            seed,
            codec: info.codec,
            hardware: info.hardware,
            width: 1280,
            height: 720,
            measured_seconds: seconds,
            udp_packets: stats.packets,
            udp_dropped: stats.dropped,
            injected_loss_pct: 100.0 * stats.dropped as f64 / stats.packets.max(1) as f64,
            original_packets: stats.original_packets,
            original_dropped: stats.original_dropped,
            parity_packets: stats.parity_packets,
            parity_bytes: stats.parity_bytes,
            udp_send_mbps: stats.bytes as f64 * 8.0 / seconds / 1e6,
            sent_frames: sent,
            complete_frames: complete,
            complete_pct: 100.0 * complete as f64 / sent.max(1) as f64,
            complete_fps: complete as f64 / seconds,
            reference_chain_fps_estimate: chain as f64 / seconds,
            applied_fps_mean: mean("client://stats", "appliedFps"),
            applied_kbps_mean: mean("client://stats", "appliedBitrateKbps"),
            fec_ratio_pct_mean: mean("client://fec", "ratioPermille") / 10.0,
            recovered_fragments_whole_run: events
                .iter()
                .filter(|(_, kind, _)| kind == "client://fec")
                .filter_map(|(_, _, v)| v["recovered"].as_u64())
                .max()
                .unwrap_or(0),
            disconnect_reason: events
                .iter()
                .find(|(_, kind, _)| kind == "client://closed")
                .and_then(|(_, _, v)| v["reason"].as_str().map(str::to_owned)),
        };
        outcome
    };
    connection.disconnect();
    drop(ws);
    host.stop().await;
    udp_task.abort();
    tcp_task.abort();
    outcome
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit lossy UDP FEC A/B comparison; about three minutes"]
async fn udp_loss_fec_comparison() {
    let path = std::env::var_os("FEC_COMPARE_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/fec-comparison.json")
        });
    let cases = std::env::var("FEC_COMPARE_CASES").ok();
    let mut outcomes = Vec::new();
    for loss in [
        Loss::Random(0),
        Loss::Random(10),
        Loss::Random(30),
        Loss::Random(50),
        Loss::Random(100),
        Loss::Burst3,
    ] {
        if cases
            .as_ref()
            .is_some_and(|cases| !cases.split(',').any(|name| name.trim() == loss.name()))
        {
            continue;
        }
        for fec in [false, true] {
            let outcome = measure(loss, fec).await;
            eprintln!("FEC_AB {}", serde_json::to_string(&outcome).unwrap());
            outcomes.push(outcome);
            std::fs::write(&path, serde_json::to_string_pretty(&outcomes).unwrap()).unwrap();
        }
    }
    assert!(outcomes
        .iter()
        .filter(|o| o.fec)
        .any(|o| o.recovered_fragments_whole_run > 0));
    assert!(outcomes.iter().all(|o| o.sent_frames > 0));
    for outcome in outcomes.iter().filter(|o| o.scenario == "random_0pct") {
        assert_eq!(
            outcome.complete_frames, outcome.sent_frames,
            "lossless control run must deliver every measured frame"
        );
    }
    assert!(
        outcomes.iter().all(|o| o.disconnect_reason.is_none()),
        "the comparison must not silently include a disconnected session"
    );
    eprintln!("FEC_AB_RESULTS {}", path.display());
}
