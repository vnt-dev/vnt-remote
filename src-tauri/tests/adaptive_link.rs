//! Exercise real capture/encoding/UDP reassembly and the client's automatic
//! feedback through a controllable TCP/UDP network relay.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vnt_remote_lib::{client, server, EventSink, NoopEventSink};

#[derive(Default)]
struct Stats(Mutex<Vec<serde_json::Value>>);

impl EventSink for Stats {
    fn emit_json(&self, event: &str, payload: String) {
        if event == "client://stats" {
            self.0
                .lock()
                .unwrap()
                .push(serde_json::from_str(&payload).unwrap());
        }
    }
}

async fn wait_stats(
    stats: &Stats,
    predicate: impl Fn(&serde_json::Value) -> bool,
    timeout: Duration,
) -> serde_json::Value {
    tokio::time::timeout(timeout, async {
        loop {
            let sample = stats.0.lock().unwrap().last().cloned();
            if let Some(sample) = sample.filter(|sample| predicate(sample)) {
                return sample;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "quality did not converge: {:?}",
            stats.0.lock().unwrap().last()
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_udp_loss_reduces_bitrate_then_fps_and_recovers_without_reconnecting() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .try_init();
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "adaptive-test".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 4000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let backend: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    // Windows may exclude whole UDP ranges which still allow ephemeral TCP.
    // Do not repeatedly ask TCP port 0 for that same unavailable range.
    let mut relay = None;
    for _ in 0..128 {
        let mut random = [0u8; 2];
        getrandom::fill(&mut random).unwrap();
        let port = 20000 + u16::from_be_bytes(random) % 30000;
        let Ok(tcp) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
            continue;
        };
        let address = tcp.local_addr().unwrap();
        if let Ok(udp) = tokio::net::UdpSocket::bind(address).await {
            relay = Some((tcp, udp, address));
            break;
        }
    }
    let (tcp, udp, address) = relay.expect("relay needs a port supporting TCP and UDP");
    socket2::SockRef::from(&udp)
        .set_recv_buffer_size(16 * 1024 * 1024)
        .unwrap();
    let lossy = Arc::new(AtomicBool::new(false));
    let mode = lossy.clone();
    let udp_proxy = tokio::spawn(async move {
        let mut client = None;
        let mut bytes = vec![0; 65536];
        loop {
            let (len, from) = udp.recv_from(&mut bytes).await.unwrap();
            if from == backend {
                let Some(client) = client else { continue };
                if mode.load(Ordering::Relaxed) {
                    if let Some((header, _)) = vnt_proto::udp::FragHeader::decode(&bytes[..len]) {
                        // Lose every fourth whole frame, including all its fragments.
                        if header.frame_seq % 4 == 2 {
                            continue;
                        }
                        if header.frag_idx == 0 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
                udp.send_to(&bytes[..len], client).await.unwrap();
            } else {
                client = Some(from);
                udp.send_to(&bytes[..len], backend).await.unwrap();
            }
        }
    });
    let tcp_proxy = tokio::spawn(async move {
        // Managed sessions also bind auxiliary file/media TCP sockets. Forward
        // every connection, otherwise the unaccepted file bind expires at 30s
        // and turns a congestion recovery assertion into a session timeout.
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = tcp.accept() => {
                    let (mut upstream, _) = accepted.unwrap();
                    connections.spawn(async move {
                        let mut downstream = tokio::net::TcpStream::connect(backend).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut upstream, &mut downstream).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    let stats = Arc::new(Stats::default());
    let (client, info) = client::connect_split(
        stats.clone(),
        client::ConnectParams {
            clipboard_enabled: true,
            server_config_id: None,
            server_node_id: None,
            server_network_code: None,
            hardware_codecs: Vec::new(),
            color_depth: 24,
            show_wallpaper: true,
            drag_full_windows: false,
            fec_enabled: false,
            resolution_mode: "none".into(),
            target_width: 0,
            target_height: 0,
            audio_device_id: None,
            audio_enabled: false,
            transport: vnt_proto::Transport::Hybrid,
            host: "127.0.0.1".into(),
            port: address.port(),
            password: "adaptive-test".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 4000,
        },
        clipboard,
    )
    .await
    .unwrap();
    assert_eq!(info.fps, 30);
    wait_stats(
        &stats,
        |s| s["serverFps"].as_u64().is_some_and(|fps| fps > 0) && s["appliedFps"] == 30,
        Duration::from_secs(10),
    )
    .await;

    lossy.store(true, Ordering::Relaxed);
    let reduced = wait_stats(
        &stats,
        |s| s["appliedFps"].as_u64().is_some_and(|fps| fps < 30),
        Duration::from_secs(12),
    )
    .await;
    assert!(reduced["appliedBitrateKbps"].as_u64().unwrap() < 4000);
    assert!(stats.0.lock().unwrap().iter().any(|s| s["appliedFps"] == 30
        && s["appliedBitrateKbps"]
            .as_u64()
            .is_some_and(|kbps| kbps < 4000)));
    let during_loss = reduced["appliedBitrateKbps"].as_u64().unwrap();

    lossy.store(false, Ordering::Relaxed);
    // The current controller restores responsiveness to 20 FPS, then bitrate,
    // and finally the remaining FPS. Exercise that policy through screen routing.
    let recovered = wait_stats(
        &stats,
        |s| {
            s["appliedFps"].as_u64().is_some_and(|fps| fps >= 20)
                && s["lossPermille"] == 0
                && s["appliedBitrateKbps"]
                    .as_u64()
                    .is_some_and(|kbps| kbps > during_loss)
        },
        Duration::from_secs(25),
    )
    .await;
    let recovery_bitrate = recovered["appliedBitrateKbps"].as_u64().unwrap();
    assert!(
        recovery_bitrate <= 4000,
        "recovery must respect the requested bitrate limit"
    );
    wait_stats(
        &stats,
        |s| s["appliedFps"] == 30 && s["appliedBitrateKbps"] == 4000,
        Duration::from_secs(90),
    )
    .await;
    assert_eq!(server.active_sessions().len(), 1);
    client.disconnect();
    server.stop().await;
    udp_proxy.abort();
    tcp_proxy.abort();
}
