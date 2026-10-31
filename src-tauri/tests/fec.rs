//! Full synthetic capture -> encoder -> lossy UDP relay -> FEC -> controller.
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use vnt_remote_lib::{client, server, EventSink, NoopEventSink};

#[derive(Default)]
struct Events(Mutex<Vec<serde_json::Value>>);
impl EventSink for Events {
    fn emit_json(&self, event: &str, payload: String) {
        if event == "client://fec" || event == "client://stats" {
            let mut value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            value["event"] = event.into();
            self.0.lock().unwrap().push(value);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn negotiated_fec_repairs_udp_capture_and_tcp_forces_it_off() {
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "fec-test".into(),
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
    let mut bound = None;
    for _ in 0..128 {
        let mut random = [0; 2];
        getrandom::fill(&mut random).unwrap();
        let port = 20000 + u16::from_be_bytes(random) % 30000;
        let Ok(tcp) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
            continue;
        };
        if let Ok(udp) = tokio::net::UdpSocket::bind(tcp.local_addr().unwrap()).await {
            bound = Some((tcp, udp, port));
            break;
        }
    }
    let (tcp, udp, port) = bound.unwrap();
    socket2::SockRef::from(&udp)
        .set_recv_buffer_size(16 * 1024 * 1024)
        .unwrap();
    let udp_proxy = tokio::spawn(async move {
        let mut controller = None;
        let mut buf = vec![0; 65536];
        loop {
            let (len, from) = udp.recv_from(&mut buf).await.unwrap();
            if from == backend {
                let Some(target) = controller else {
                    continue;
                };
                // Drop one original from every protected block. Recovery must
                // preserve its raw loss signal even though no video frame is lost.
                if len > vnt_net::fec::HEADER
                    && buf[10] == vnt_net::fec::FLAG
                    && buf[35] > 0
                    && buf[33] == 0
                {
                    continue;
                }
                udp.send_to(&buf[..len], target).await.unwrap();
            } else {
                controller = Some(from);
                udp.send_to(&buf[..len], backend).await.unwrap();
            }
        }
    });
    let tcp_proxy = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                next=tcp.accept()=>{let (mut a,_)=next.unwrap();children.spawn(async move{let mut b=tokio::net::TcpStream::connect(backend).await.unwrap();let _=tokio::io::copy_bidirectional(&mut a,&mut b).await;});}
                Some(_)=children.join_next(),if !children.is_empty()=>{}
            }
        }
    });
    for transport in [vnt_proto::Transport::Hybrid, vnt_proto::Transport::Tcp] {
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
                fec_enabled: true,
                resolution_mode: "none".into(),
                target_width: 0,
                target_height: 0,
                audio_device_id: None,
                audio_enabled: false,
                transport,
                host: "127.0.0.1".into(),
                port,
                password: "fec-test".into(),
                codecs: vec!["h264".into()],
                fps: 30,
                bitrate_kbps: 4000,
            },
            clipboard.clone(),
        )
        .await
        .unwrap();
        assert_eq!(info.fec_enabled, transport == vnt_proto::Transport::Hybrid);
        if info.fec_enabled {
            tokio::time::timeout(Duration::from_secs(12), async {
                loop {
                    let good = events.0.lock().unwrap().iter().any(|s| {
                        s["event"] == "client://fec"
                            && s["recovered"].as_u64().is_some_and(|n| n > 0)
                            && s["rawLossPermille"].as_u64().is_some_and(|n| n > 0)
                    });
                    if good {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("no FEC repair observed: {:?}", events.0.lock().unwrap()));
        } else {
            assert_eq!(info.fec_reason, "适用于混合模式、QUIC 和 VNT");
        }
        connection.disconnect();
    }
    udp_proxy.abort();
    tcp_proxy.abort();
    server.stop().await;
}
