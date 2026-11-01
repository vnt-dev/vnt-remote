//! 端到端测试：本机同时运行被控端与控制端。
//!
//! 验证链路：TCP 握手/认证/协商 → 抓屏 → MF 硬编 → UDP 分片 → 重组 →
//! 回环 WS 桥 → （伪前端）收流。不注入键鼠输入（避免影响真实桌面）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use vnt_net::tcp::ControlChannel;
use vnt_proto::control::ControlMessage;
use vnt_proto::pb;
use vnt_remote_lib::client::{self, ConnectParams};
use vnt_remote_lib::{server, NoopEventSink};

/// 测试用剪贴板服务：不启动监听线程，也不写本机剪贴板。
fn test_clipboard() -> Arc<vnt_remote_lib::clipboard::ClipboardService> {
    vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}))
}

async fn wait_for_session_count(server: &server::ServerHandle, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.active_sessions().len() != count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server must remove only the closed session promptly");
}

// Like useDecoder, recover a bridge closed after broadcast lag while keeping
// the same authenticated remote session. A dead session/listener still fails.
async fn next_video_with_bridge_recovery(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    info: &client::ClientSessionInfo,
    client: &client::ClientHandle,
) {
    let mut reconnects = 0;
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(_))) => return,
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                reconnects += 1;
                assert!(reconnects <= 5, "bridge never recovered");
                let (connected, _) = tokio_tungstenite::connect_async(format!(
                    "ws://127.0.0.1:{}/?t={}",
                    info.ws_port, info.token,
                ))
                .await
                .expect("live remote session must retain its video bridge");
                *ws = connected;
                client
                    .control_tx
                    .send(ControlMessage::RequestKeyframe)
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_limit_releases_capacity_after_a_peer_disconnects() {
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "capacity-test".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 1,
            bitrate_kbps: 8000,
            hardware_encode: false,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let addr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let hello = ControlMessage::Hello(pb::Hello {
        system: vnt_proto::system::local() as i32,
        hardware_codecs: Vec::new(),
        color_depth: 24,
        desktop_effects: Some(Default::default()),
        capabilities: vnt_proto::platform_capabilities(),
        fec_version: 0,
        resolution_mode: 0,
        target_width: 0,
        target_height: 0,
        audio_enabled: false,
        transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
        proto_ver: vnt_proto::PROTO_VER,
        codecs: vec![pb::Codec::H264.into()],
        fps: 1,
        bitrate_kbps: 8000,
        client: None,
    });
    let mut peers = Vec::new();
    for _ in 0..server::MAX_CONNECTIONS {
        let mut peer = ControlChannel::connect(addr).await.unwrap();
        peer.send(&hello).await.unwrap();
        vnt_net::auth::respond(&mut peer, "capacity-test")
            .await
            .unwrap();
        assert!(matches!(
            recv_channel(&mut peer).await.unwrap(),
            Some(ControlMessage::Welcome(_))
        ));
        peers.push(peer);
    }
    wait_for_session_count(&server, server::MAX_CONNECTIONS).await;
    let mut overflow = ControlChannel::connect(addr).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(2), recv_channel(&mut overflow))
        .await
        .unwrap();
    assert!(
        matches!(reply, Ok(None) | Err(_)),
        "over-capacity connections must be closed"
    );
    let mut closing = peers.pop().unwrap();
    closing
        .send(&ControlMessage::SessionEnd(pb::SessionEnd {
            reason: "controller disconnected".into(),
        }))
        .await
        .unwrap();
    // Keep the socket alive until the host consumes SessionEnd. Closing with
    // unread state packets can reset TCP and discard the final outbound packet.
    // Session registration is removed before the writer's bounded shutdown;
    // wait until the connection permit itself has been returned.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut replacement = ControlChannel::connect(addr).await.unwrap();
            if replacement.send(&hello).await.is_ok()
                && vnt_net::auth::respond(&mut replacement, "capacity-test")
                    .await
                    .is_ok()
                && matches!(
                    recv_channel(&mut replacement).await,
                    Ok(Some(ControlMessage::Welcome(_)))
                )
            {
                peers.push(replacement);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("disconnected peers must return admission capacity");
    drop(closing);
    drop(peers);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_controller_windows_allows_reconnect_without_restarting_shared_server() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init();
    use vnt_remote_lib::{config::AppConfig, connections::Connections, AppState};
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "shared-peer".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let app_data = tempfile::tempdir().unwrap();
    let state = AppState {
        link: Arc::new(vnt_host::link::ServerLinks::default()),
        store: vnt_remote_lib::config::ConfigStore::new(app_data.path()),
        host: tokio::sync::Mutex::new(None),
        host_clipboard: test_clipboard(),
        clipboard: test_clipboard(),
        config: std::sync::Mutex::new(AppConfig::default()),
        config_update: Default::default(),
        next_window: std::sync::atomic::AtomicU64::new(1),
        resolution_notices: Default::default(),
        window_params: Default::default(),
        client: Default::default(),
        child_windows: Default::default(),
    };
    let connect = |fps| {
        client::connect_split(
            Arc::new(NoopEventSink),
            ConnectParams {
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
                port: server.port,
                password: "shared-peer".into(),
                codecs: vec!["h264".into()],
                fps,
                bitrate_kbps: if fps == 30 { 8000 } else { 16000 },
            },
            clipboard.clone(),
        )
    };
    let (survivor, info) = connect(30).await.unwrap();
    let (mut survivor_ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/?t={}",
        info.ws_port, info.token
    ))
    .await
    .unwrap();
    let mut ids = std::collections::HashSet::from([info.session_id]);
    for _ in 0..3 {
        let (handle, other_info) = connect(60).await.unwrap();
        assert_eq!(other_info.fps, 60);
        assert!(ids.insert(other_info.session_id));
        let (mut other_ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/?t={}",
            other_info.ws_port, other_info.token
        ))
        .await
        .unwrap();
        wait_for_session_count(&server, 2).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            next_video_with_bridge_recovery(&mut survivor_ws, &info, &survivor).await;
            next_video_with_bridge_recovery(&mut other_ws, &other_info, &handle).await;
        })
        .await
        .expect("each controller must receive its own video stream");
        let mut connection = Connections::default();
        let (generation, _) = connection.begin();
        connection.commit(generation, handle).unwrap();
        state
            .client
            .lock()
            .unwrap()
            .insert("remote-test".into(), connection);
        // Same backend path as the native X button; no disconnect IPC or Vue unmount.
        state.close_control_window("remote-test");
        state.close_control_window("remote-test"); // Destroyed after CloseRequested is idempotent.
        wait_for_session_count(&server, 1).await;
        assert_eq!(server.active_sessions()[0].session_id, info.session_id);
        survivor
            .control_tx
            .send(ControlMessage::Ping(pb::Ping { timestamp_ms: 123 }))
            .await
            .unwrap();
    }
    // The original peer must still receive fresh frames after all three closes.
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..8 {
            next_video_with_bridge_recovery(&mut survivor_ws, &info, &survivor).await;
        }
    })
    .await
    .unwrap();
    drop(survivor);
    wait_for_session_count(&server, 0).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abrupt_tcp_close_across_codecs_does_not_leave_sessions_behind() {
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let addr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    // AV1/H264 有软编兜底必然可协商；HEVC 只有硬件路径，按探测结果动态判定
    let caps = vnt_codec::probe_caps();
    let hevc_available = caps.iter().any(|c| c.codec == vnt_proto::Codec::Hevc);
    for (codecs, expect_welcome) in [
        (vec![pb::Codec::Av1.into()], true),
        (vec![pb::Codec::H264.into()], true),
        (vec![pb::Codec::Hevc.into()], hevc_available),
    ] {
        let mut channel = ControlChannel::connect(addr).await.unwrap();
        channel
            .send(&ControlMessage::Hello(pb::Hello {
                system: vnt_proto::system::local() as i32,
                hardware_codecs: Vec::new(),
                color_depth: 24,
                desktop_effects: Some(Default::default()),
                capabilities: vnt_proto::platform_capabilities(),
                fec_version: 0,
                resolution_mode: 0,
                target_width: 0,
                target_height: 0,
                audio_enabled: false,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                proto_ver: vnt_proto::PROTO_VER,
                codecs,
                client: None,
                fps: 30,
                bitrate_kbps: 8000,
            }))
            .await
            .unwrap();
        vnt_net::auth::respond(&mut channel, "pw").await.unwrap();
        let reply = recv_channel(&mut channel).await.unwrap();
        if expect_welcome {
            assert!(
                matches!(reply, Some(ControlMessage::Welcome(_))),
                "有软编兜底的编解码应协商成功"
            );
            wait_for_session_count(&server, 1).await;
        } else {
            assert!(matches!(reply, Some(ControlMessage::Denied(_))));
        }
        drop(channel); // No SessionEnd packet, simulating process/window termination.
                       // Abrupt loss keeps the logical session available for transport recovery.
                       // It must expire after the 30-second recovery grace, without a restart.
        tokio::time::timeout(Duration::from_secs(40), async {
            while !server.active_sessions().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("unrecoverable sessions must expire after the recovery grace");
    }
    server.stop().await;
}

/// Each controller window owns a separate handle. Ending one must leave the
/// other video bridge alive, including when peers use different stream settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_connections_preserve_requested_stream_settings() {
    let mut servers = Vec::new();
    let mut clients = Vec::new();
    let mut sockets = Vec::new();
    for fps in [30, 60] {
        let clipboard = test_clipboard();
        let server = server::start(
            Arc::new(NoopEventSink),
            server::ServerConfig {
                audio: false,
                password: "multi-window-test".into(),
                port: 0,
                monitor_index: 0,
                region: None,
                fps: 15,
                bitrate_kbps: 4000,
                hardware_encode: true,
                clipboard: clipboard.clone(),
                synthetic: true,
                view_only: false,
            },
        )
        .await
        .unwrap();
        let (client, info) = client::connect_split(
            Arc::new(NoopEventSink),
            ConnectParams {
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
                port: server.port,
                password: "multi-window-test".into(),
                codecs: vec!["h264".into()],
                fps,
                bitrate_kbps: 8000,
            },
            clipboard.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            info.fps, fps,
            "server must use the controller's requested fps"
        );
        let (socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/?t={}",
            info.ws_port, info.token
        ))
        .await
        .unwrap();
        servers.push(server);
        clients.push(client);
        sockets.push(socket);
    }
    // Verify both independently established streams before ending either.
    for socket in &mut sockets {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(socket.next().await.unwrap().unwrap(), Message::Binary(_)) {
                    break;
                }
            }
        })
        .await
        .expect("both windows must receive video");
    }
    clients.remove(0).disconnect();
    servers.remove(0).stop().await;
    // Drain old buffered messages, then require fresh video from the survivor.
    let socket = &mut sockets[1];
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut frames = 0;
        while frames < 16 {
            if matches!(socket.next().await.unwrap().unwrap(), Message::Binary(_)) {
                frames += 1;
            }
        }
    })
    .await
    .expect("closing one window must not stop the other stream");
    clients.pop().unwrap().disconnect();
    servers.pop().unwrap().stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentication_rejects_wrong_password_and_cross_connection_replay() {
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let hello = pb::Hello {
        system: vnt_proto::system::local() as i32,
        color_depth: 24,
        desktop_effects: Some(Default::default()),
        capabilities: vnt_proto::platform_capabilities(),
        proto_ver: vnt_proto::PROTO_VER,
        transport: vnt_proto::Transport::Tcp.to_proto() as i32,
        codecs: vec![pb::Codec::H264 as i32],
        fps: 30,
        bitrate_kbps: 8000,
        ..Default::default()
    };
    let mut replayed = None;
    let mut previous_nonce = None;
    for attempt in 0..3 {
        let mut channel = ControlChannel::connect(addr).await.unwrap();
        channel
            .send(&ControlMessage::Hello(hello.clone()))
            .await
            .unwrap();
        let Some(ControlMessage::AuthChallenge(challenge)) =
            recv_channel(&mut channel).await.unwrap()
        else {
            panic!("expected challenge before authentication")
        };
        let nonce: [u8; 32] = challenge.nonce.try_into().unwrap();
        if let Some(previous) = previous_nonce {
            assert_ne!(nonce, previous);
        }
        previous_nonce = Some(nonce);
        let proof = match attempt {
            0 => {
                replayed = Some(vnt_proto::auth::response("pw", &nonce));
                vnt_proto::auth::response("wrong", &nonce)
            }
            1 => replayed.take().unwrap(),
            _ => vnt_proto::auth::response("pw", &nonce),
        };
        assert!(server.active_sessions().is_empty());
        channel
            .send(&ControlMessage::AuthResponse(pb::AuthResponse { proof }))
            .await
            .unwrap();
        let result = recv_channel(&mut channel).await.unwrap();
        if attempt < 2 {
            assert!(matches!(result, Some(ControlMessage::Denied(_))));
            assert!(server.active_sessions().is_empty());
        } else {
            assert!(matches!(result, Some(ControlMessage::Welcome(_))));
        }
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_stream_preferences_are_denied_before_claiming_session() {
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    for (fps, bitrate, depth, accepted) in [
        (0, 8000, 24, false),
        (60, 200001, 24, false),
        (60, 16000, 8, false),
        (60, 16000, 30, false),
        (60, 16000, 0, false),
        (60, 16000, 15, true),
        (60, 16000, 16, true),
        (60, 16000, 24, true),
        (60, 16000, 32, true),
    ] {
        let mut channel = ControlChannel::connect(addr).await.unwrap();
        channel
            .send(&ControlMessage::Hello(pb::Hello {
                system: vnt_proto::system::local() as i32,
                hardware_codecs: Vec::new(),
                color_depth: depth,
                desktop_effects: Some(Default::default()),
                capabilities: vnt_proto::platform_capabilities(),
                fec_version: 0,
                resolution_mode: 0,
                target_width: 0,
                target_height: 0,
                audio_enabled: false,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                proto_ver: vnt_proto::PROTO_VER,
                codecs: vec![pb::Codec::H264.into()],
                client: None,
                fps,
                bitrate_kbps: bitrate,
            }))
            .await
            .unwrap();
        vnt_net::auth::respond(&mut channel, "pw").await.unwrap();
        let reply = recv_channel(&mut channel).await.unwrap();
        if accepted {
            let Some(ControlMessage::Welcome(welcome)) = reply else {
                panic!("valid settings must be accepted");
            };
            assert_eq!(welcome.fps, fps);
            assert_eq!(welcome.color_depth, depth);
        } else {
            assert!(matches!(reply, Some(ControlMessage::Denied(_))));
        }
    }
    server.stop().await;
}

/// 停止服务必须终止已建立的会话：
/// 控制端收到 SessionEnd、视频流停止、stop() 在有限时间内返回。
/// （回归测试：stop 曾只取消监听任务，旧会话可继续控制本机）
/// 多个主控并发握手都应得到独立会话，不能相互覆盖；均可正常收发心跳。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_handshakes_get_independent_sessions() {
    let sink = Arc::new(NoopEventSink);
    let clipboard = test_clipboard();
    let server = server::start(
        sink,
        server::ServerConfig {
            audio: false,
            password: "pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .expect("启动被控端");
    let addr: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();

    // 12 个并发握手
    let mut handles = Vec::new();
    for _ in 0..12 {
        handles.push(tokio::spawn(async move {
            let mut ch = ControlChannel::connect(addr).await.ok()?;
            ch.send(&ControlMessage::Hello(pb::Hello {
                system: vnt_proto::system::local() as i32,
                hardware_codecs: Vec::new(),
                color_depth: 24,
                desktop_effects: Some(Default::default()),
                capabilities: vnt_proto::platform_capabilities(),
                fec_version: 0,
                resolution_mode: 0,
                target_width: 0,
                target_height: 0,
                audio_enabled: false,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                fps: 30,
                bitrate_kbps: 8000,
                proto_ver: vnt_proto::PROTO_VER,
                codecs: vec![pb::Codec::H264.into()],
                client: None,
            }))
            .await
            .ok()?;
            vnt_net::auth::respond(&mut ch, "pw").await.ok()?;
            match tokio::time::timeout(Duration::from_secs(10), recv_channel(&mut ch)).await {
                Ok(Ok(Some(ControlMessage::Welcome(w)))) => Some((ch, w.session_id)),
                _ => None,
            }
        }));
    }

    let mut welcomed = Vec::new();
    for h in handles {
        if let Some(channel) = h.await.unwrap() {
            welcomed.push(channel);
        }
    }
    assert_eq!(
        welcomed.len(),
        12,
        "every authenticated controller must be accepted"
    );
    let ids: std::collections::HashSet<_> = welcomed.iter().map(|(_, id)| *id).collect();
    assert_eq!(ids.len(), 12);
    for (channel, _) in &mut welcomed {
        channel
            .send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 7 }))
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), recv_channel(channel))
                .await
                .unwrap()
                .unwrap(),
            Some(ControlMessage::Pong(_))
        ));
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_stop_terminates_active_session() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init();
    let sink = Arc::new(NoopEventSink);
    let clipboard = test_clipboard();
    let server = server::start(
        sink,
        server::ServerConfig {
            audio: false,
            password: "pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .expect("启动被控端");

    // ---- 原始协议客户端：握手 + 绑定 UDP ----
    let addr: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let mut ch = ControlChannel::connect(addr).await.expect("TCP 连接");
    ch.send(&ControlMessage::Hello(pb::Hello {
        system: vnt_proto::system::local() as i32,
        hardware_codecs: Vec::new(),
        color_depth: 24,
        desktop_effects: Some(Default::default()),
        capabilities: vnt_proto::platform_capabilities(),
        fec_version: 0,
        resolution_mode: 0,
        target_width: 0,
        target_height: 0,
        audio_enabled: false,
        transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
        fps: 30,
        bitrate_kbps: 8000,
        proto_ver: vnt_proto::PROTO_VER,
        codecs: vec![pb::Codec::H264.into()],
        client: None,
    }))
    .await
    .unwrap();
    vnt_net::auth::respond(&mut ch, "pw").await.unwrap();
    let welcome = match recv_channel(&mut ch).await.unwrap() {
        Some(ControlMessage::Welcome(w)) => w,
        other => panic!("期望 Welcome，得到 {other:?}"),
    };

    let mut rx = vnt_net::udp::UdpVideoReceiver::bind(addr).await.unwrap();
    rx.set_session(welcome.session_id);
    let bind_token = welcome.bind_token;

    // 收到 2 帧证明推流管线已在运行
    let t0 = tokio::time::Instant::now();
    let mut got = 0;
    while got < 2 {
        assert!(t0.elapsed() < Duration::from_secs(30), "等待视频帧超时");
        rx.send_bind(welcome.session_id, &bind_token).await.unwrap();
        if let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_secs(2), rx.recv_frame()).await
        {
            got += 1;
        }
    }

    // ---- 停止服务：必须在有限时间内切断会话 ----
    let t_stop = tokio::time::Instant::now();
    server.stop().await;
    assert!(
        t_stop.elapsed() < Duration::from_secs(10),
        "stop() 应在会话清理后及时返回，耗时 {:?}",
        t_stop.elapsed()
    );

    // 控制通道收到 SessionEnd（或对端关闭）；此前排队的 UdpBindAck 等先跳过
    let mut got_end = false;
    let end_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < end_deadline {
        let msg = match tokio::time::timeout(Duration::from_secs(1), recv_channel(&mut ch)).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => {
                got_end = true;
                break;
            }
            Err(_) => panic!("停止后 5s 内未收到结束消息"),
        };
        match msg {
            Some(ControlMessage::SessionEnd(se)) => {
                assert!(
                    se.reason.contains("停止"),
                    "原因应表明服务停止: {}",
                    se.reason
                );
                got_end = true;
                break;
            }
            None => {
                got_end = true;
                break;
            }
            Some(_) => continue, // 排队中的 UdpBindAck/Pong 等
        }
    }
    assert!(got_end, "应收到 SessionEnd（或连接关闭）");

    // 视频流停止：清空在途帧后应静默（3 秒无新帧）
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < drain_deadline {
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.recv_frame()).await;
    }
    let silent = tokio::time::timeout(Duration::from_secs(3), rx.recv_frame()).await;
    assert!(
        silent.is_err(),
        "停止服务后不应再收到视频帧（收到 {:?} 字节）",
        silent
            .ok()
            .and_then(|r| r.ok().flatten())
            .map(|f| f.data.len())
    );
}

/// 未认证的 UDP 绑定不得劫走视频流（P1 回归）：
/// 正确会话 ID + 正确凭证 + 同 IP 才接受；缺凭证/错 IP 一律拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_bind_requires_token() {
    let sink = Arc::new(NoopEventSink);
    let clipboard = test_clipboard();
    let server = server::start(
        sink,
        server::ServerConfig {
            audio: false,
            password: "pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .expect("启动被控端");

    // 合法客户端：完成 TCP 握手拿到 Welcome（含 bind_token）
    let addr: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let mut ch = ControlChannel::connect(addr).await.unwrap();
    ch.send(&ControlMessage::Hello(pb::Hello {
        system: vnt_proto::system::local() as i32,
        hardware_codecs: Vec::new(),
        color_depth: 24,
        desktop_effects: Some(Default::default()),
        capabilities: vnt_proto::platform_capabilities(),
        fec_version: 0,
        resolution_mode: 0,
        target_width: 0,
        target_height: 0,
        audio_enabled: false,
        transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
        fps: 30,
        bitrate_kbps: 8000,
        proto_ver: vnt_proto::PROTO_VER,
        codecs: vec![pb::Codec::H264.into()],
        client: None,
    }))
    .await
    .unwrap();
    vnt_net::auth::respond(&mut ch, "pw").await.unwrap();
    let welcome = match recv_channel(&mut ch).await.unwrap() {
        Some(ControlMessage::Welcome(w)) => w,
        other => panic!("期望 Welcome，得到 {other:?}"),
    };

    // 攻击者：同 IP、正确 session_id，但**无绑定凭证**
    let evil = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    let forged = vnt_proto::control::encode_udp_bind_frame(welcome.session_id, &[0xBA; 16]);
    evil.send_to(&forged, addr).unwrap();

    // 合法客户端用正确凭证绑定并收到视频
    let mut rx = vnt_net::udp::UdpVideoReceiver::bind(addr).await.unwrap();
    rx.set_session(welcome.session_id);
    let t0 = tokio::time::Instant::now();
    let mut got = 0;
    while got < 2 {
        assert!(t0.elapsed() < Duration::from_secs(30), "等待视频超时");
        rx.send_bind(welcome.session_id, &welcome.bind_token)
            .await
            .unwrap();
        if let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_secs(2), rx.recv_frame()).await
        {
            got += 1;
        }
    }
    // 伪造绑定从未生效：视频始终流向合法客户端
    assert!(got >= 2);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_loop_video_over_ws_bridge() {
    video_over_ws_bridge(true, vec!["h264".into()], vnt_proto::Transport::Hybrid).await;
}

/// AV1 全链路（svt-av1 软编）：校验 OBU 码流格式随协商结果下发。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_loop_av1_video_over_ws_bridge() {
    video_over_ws_bridge(true, vec!["av1".into()], vnt_proto::Transport::Hybrid).await;
}

/// Regression: HEVC-only clients must connect with host hardware encoding off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_loop_software_hevc_video_over_ws_bridge() {
    for transport in [vnt_proto::Transport::Tcp, vnt_proto::Transport::Hybrid] {
        video_over_ws_bridge_with_encoding(true, vec!["hevc".into()], transport, false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_video_over_ws_bridge() {
    video_over_ws_bridge(
        false,
        vec!["hevc".into(), "h264".into()],
        vnt_proto::Transport::Hybrid,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_video_over_tcp_ws_bridge() {
    video_over_ws_bridge(
        false,
        vec!["hevc".into(), "h264".into()],
        vnt_proto::Transport::Tcp,
    )
    .await;
}

/// H264 指定连接：覆盖 AMD/NVENC 硬件 H264 编码器在原生分辨率下的完整链路。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_video_h264_over_ws_bridge() {
    video_over_ws_bridge(false, vec!["h264".into()], vnt_proto::Transport::Hybrid).await;
}

async fn video_over_ws_bridge(
    synthetic: bool,
    codecs: Vec<String>,
    transport: vnt_proto::Transport,
) {
    video_over_ws_bridge_with_encoding(synthetic, codecs, transport, true).await;
}

async fn video_over_ws_bridge_with_encoding(
    synthetic: bool,
    codecs: Vec<String>,
    transport: vnt_proto::Transport,
    hardware_encode: bool,
) {
    video_over_ws_bridge_with_resolution(
        synthetic,
        codecs,
        transport,
        hardware_encode,
        "none",
        (0, 0),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scaled_desktop_video_over_tcp_and_hybrid_with_software_and_hardware_encoders() {
    for transport in [vnt_proto::Transport::Tcp, vnt_proto::Transport::Hybrid] {
        for hardware in [false, true] {
            video_over_ws_bridge_with_resolution(
                false,
                vec!["h264".into()],
                transport,
                hardware,
                "scale",
                (640, 360),
            )
            .await;
        }
    }
}

async fn video_over_ws_bridge_with_resolution(
    synthetic: bool,
    codecs: Vec<String>,
    transport: vnt_proto::Transport,
    hardware_encode: bool,
    resolution_mode: &str,
    target: (u32, u32),
) {
    video_over_ws_bridge_with_color_depth(
        synthetic,
        codecs,
        transport,
        hardware_encode,
        resolution_mode,
        target,
        24,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn color_depth_native_desktop_video_over_tcp_and_hybrid() {
    for (depth, transport) in [
        (15, vnt_proto::Transport::Tcp),
        (16, vnt_proto::Transport::Hybrid),
        (24, vnt_proto::Transport::Tcp),
        (32, vnt_proto::Transport::Hybrid),
    ] {
        video_over_ws_bridge_with_color_depth(
            false,
            vec!["h264".into()],
            transport,
            false,
            "scale",
            (640, 360),
            depth,
        )
        .await;
    }
}

async fn video_over_ws_bridge_with_color_depth(
    synthetic: bool,
    codecs: Vec<String>,
    transport: vnt_proto::Transport,
    hardware_encode: bool,
    resolution_mode: &str,
    target: (u32, u32),
    color_depth: u32,
) {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .try_init();
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init();
    let sink = Arc::new(NoopEventSink);
    let clipboard = test_clipboard();

    // ---- 被控端（port=0 自动分配，避免保留端口段）----
    let server = server::start(
        sink.clone(),
        server::ServerConfig {
            audio: false,
            password: "e2e-pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode,
            clipboard: clipboard.clone(),
            synthetic,
            view_only: true, // Video tests do not require desktop input-injection privileges.
        },
    )
    .await
    .expect("启动被控端");
    let port = server.port;
    assert!(port > 0);

    // ---- 控制端（连接本机被控端）----
    let single_requested = codecs.first().filter(|_| codecs.len() == 1).cloned();
    let (client, info) = client::connect_split(
        sink,
        ConnectParams {
            clipboard_enabled: true,
            server_config_id: None,
            server_node_id: None,
            server_network_code: None,
            hardware_codecs: Vec::new(),
            color_depth,
            show_wallpaper: true,
            drag_full_windows: false,
            fec_enabled: false,
            resolution_mode: resolution_mode.into(),
            target_width: target.0,
            target_height: target.1,
            audio_device_id: None,
            audio_enabled: false,
            transport,
            host: "127.0.0.1".into(),
            port,
            password: "e2e-pw".into(),
            codecs,
            fps: 30,
            bitrate_kbps: 8000,
        },
        clipboard.clone(),
    )
    .await
    .expect("客户端接入");
    if let Some(only) = single_requested {
        assert_eq!(info.codec, only, "单选编解码时协商结果必须与请求一致");
    }
    if !hardware_encode {
        assert!(!info.hardware);
        if info.codec == "hevc" {
            assert_eq!(info.encoder_name, "kvazaar");
        }
    }
    assert!(info.width > 0 && info.height > 0);
    assert_eq!(info.resolution_mode, resolution_mode);
    if resolution_mode == "scale" {
        assert!(info.width <= target.0 && info.height <= target.1);
        assert_eq!((info.width % 2, info.height % 2), (0, 0));
    }

    // ---- 伪前端：连接回环 WS 桥，收视频帧 ----
    let url = format!("ws://127.0.0.1:{}/?t={}", info.ws_port, info.token);
    let (ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("连接视频桥");

    let mut ws = ws;
    client
        .control_tx
        .try_send(ControlMessage::RequestKeyframe)
        .unwrap();
    let mut video_frames = 0usize;
    let mut saw_keyframe = false;
    let mut annexb_ok = false;
    let mut obu_ok = false;
    let is_av1 = info.codec == "av1";
    let mut last_pts = 0;
    let deadline = Duration::from_secs(45);
    let start = tokio::time::Instant::now();
    while video_frames < 10 && start.elapsed() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next()).await;
        match msg {
            Ok(Some(Ok(Message::Binary(bin)))) => {
                let bin = &bin[8..];
                assert!(bin.len() > 9, "视频帧过短");
                let keyframe = bin[0] != 0;
                let pts = u32::from_be_bytes([bin[1], bin[2], bin[3], bin[4]]);
                let len = u32::from_be_bytes([bin[5], bin[6], bin[7], bin[8]]) as usize;
                assert_eq!(9 + len, bin.len(), "帧长度头不一致");
                let data = &bin[9..];
                if is_av1 {
                    // OBU 头：forbidden(bit7)=0 | type | ext | has_size | reserved(bit0)=0
                    if data.len() >= 2 && data[0] & 0x81 == 0 && (data[0] & 0x2) != 0 {
                        obu_ok = true;
                    }
                } else if data.len() >= 4 && (data[..4] == [0, 0, 0, 1] || data[1..4] == [0, 0, 1])
                {
                    // Annex B 起始码 00 00 00 01 / 00 00 01
                    annexb_ok = true;
                }
                if keyframe {
                    saw_keyframe = true;
                }
                assert!(pts < 3_600_000, "pts 异常: {pts}");
                video_frames += 1;
                last_pts = pts;
            }
            Ok(Some(Ok(Message::Text(t)))) => {
                // streamInfo / stats 文本消息：不应崩溃
                let _ = t;
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("WS 错误: {e}"),
            Ok(None) => panic!("WS 提前关闭，仅收到 {video_frames} 帧"),
            Err(_) => break, // 单帧超时，继续尝试直到总 deadline
        }
    }
    assert!(
        video_frames >= 5,
        "视频帧过少: {video_frames}（45s 内应有远超 5 帧）"
    );
    assert!(saw_keyframe, "未收到关键帧");
    assert!(
        if is_av1 { obu_ok } else { annexb_ok },
        "未检测到 {} 码流标记",
        if is_av1 { "OBU" } else { "Annex B 起始码" }
    );
    eprintln!(
        "received {video_frames} frames in {:?}, codec={} encoder={}",
        start.elapsed(),
        info.codec,
        info.encoder_name
    );

    // ---- 请求关键帧（走 TCP 控制通道）----
    client
        .control_tx
        .try_send(vnt_proto::control::ControlMessage::RequestKeyframe)
        .expect("发送 RequestKeyframe");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let msg = ws
                .next()
                .await
                .expect("bridge closed before recovery")
                .expect("video bridge error");
            if let Message::Binary(bin) = msg {
                let bin = &bin[8..];
                assert!(bin.len() > 9);
                let pts = u32::from_be_bytes(bin[1..5].try_into().unwrap());
                if bin[0] != 0 && pts > last_pts {
                    break;
                }
            }
        }
    })
    .await
    .expect("keyframe request must recover even on an idle desktop");

    // ---- 断开与清理 ----
    client.disconnect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_and_hybrid_stream_together_without_udp_binding_for_tcp() {
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "transports".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let mut clients = Vec::new();
    for transport in [vnt_proto::Transport::Tcp, vnt_proto::Transport::Hybrid] {
        let (client, info) = client::connect_split(
            Arc::new(NoopEventSink),
            ConnectParams {
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
                transport,
                host: "127.0.0.1".into(),
                port: server.port,
                password: "transports".into(),
                codecs: vec!["h264".into()],
                fps: 30,
                bitrate_kbps: 8000,
            },
            clipboard.clone(),
        )
        .await
        .unwrap();
        assert_eq!(info.transport, transport);
        let (ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/?t={}",
            info.ws_port, info.token
        ))
        .await
        .unwrap();
        clients.push((client, info, ws));
    }
    wait_for_session_count(&server, 2).await;
    assert!(server
        .active_sessions()
        .iter()
        .any(|s| s.transport == vnt_proto::Transport::Tcp));
    // Cross the old ten-second UDP binding deadline while continuously consuming both streams.
    let until = tokio::time::Instant::now() + Duration::from_secs(11);
    tokio::time::timeout(Duration::from_secs(25), async {
        while tokio::time::Instant::now() < until {
            for (client, info, ws) in &mut clients {
                next_video_with_bridge_recovery(ws, info, client).await;
            }
        }
    })
    .await
    .expect("both transports must keep streaming past UDP binding timeout");
    clients.remove(0);
    wait_for_session_count(&server, 1).await;
    let (client, info, ws) = &mut clients[0];
    tokio::time::timeout(
        Duration::from_secs(5),
        next_video_with_bridge_recovery(ws, info, client),
    )
    .await
    .unwrap();
    drop(clients);
    wait_for_session_count(&server, 0).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_unavailable_rejects_hybrid_but_tcp_streams() {
    let udp = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let port = udp.local_addr().unwrap().port();
    let clipboard = test_clipboard();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "transports".into(),
            port,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    for wire_mode in [0, 3, -1, vnt_proto::Transport::Hybrid.to_proto() as i32] {
        let mut channel = ControlChannel::connect(([127, 0, 0, 1], port).into())
            .await
            .unwrap();
        channel
            .send(&ControlMessage::Hello(pb::Hello {
                system: vnt_proto::system::local() as i32,
                hardware_codecs: Vec::new(),
                color_depth: 24,
                desktop_effects: Some(Default::default()),
                capabilities: vnt_proto::platform_capabilities(),
                fec_version: 0,
                resolution_mode: 0,
                target_width: 0,
                target_height: 0,
                audio_enabled: false,
                transport: wire_mode,
                proto_ver: vnt_proto::PROTO_VER,
                codecs: vec![pb::Codec::H264 as i32],
                fps: 30,
                bitrate_kbps: 8000,
                client: None,
            }))
            .await
            .unwrap();
        vnt_net::auth::respond(&mut channel, "transports")
            .await
            .unwrap();
        assert!(matches!(
            recv_channel(&mut channel).await.unwrap(),
            Some(ControlMessage::Denied(_))
        ));
    }
    let (client, info) = client::connect_split(
        Arc::new(NoopEventSink),
        ConnectParams {
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
            transport: vnt_proto::Transport::Tcp,
            host: "127.0.0.1".into(),
            port,
            password: "transports".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        clipboard,
    )
    .await
    .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/?t={}",
        info.ws_port, info.token
    ))
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        next_video_with_bridge_recovery(&mut ws, &info, &client),
    )
    .await
    .unwrap();
    let mut packet = [0u8; 128];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), udp.recv_from(&mut packet))
            .await
            .is_err(),
        "pure TCP must not send UDP bind packets"
    );
    drop(client);
    wait_for_session_count(&server, 0).await;
    server.stop().await;
    drop(udp);
}

// ---------- 剪贴板同步 ----------

/// 记录每次本机剪贴板写入（内容 + 写入路径）。
type WriteLog = Arc<std::sync::Mutex<Vec<(vnt_clip::ClipboardContent, Vec<std::path::PathBuf>)>>>;
/// 剪贴板写入实现（测试用：只记录，不动真实剪贴板）。
type TestWriter = Arc<dyn Fn(vnt_clip::ClipboardContent, Vec<std::path::PathBuf>) + Send + Sync>;

fn recording_writer() -> (TestWriter, WriteLog) {
    let log: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = log.clone();
    let writer: TestWriter = Arc::new(move |content, paths| {
        sink.lock().unwrap().push((content, paths));
    });
    (writer, log)
}

/// A disabled preference applies before either side can synchronize over the real connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_disabled_preference_blocks_both_directions_from_connection_start() {
    let (host_writer, host_writes) = recording_writer();
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(host_writer);
    let (client_writer, client_writes) = recording_writer();
    let client_clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(client_writer);
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "clip-pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let (client, info) = client::connect_split(
        Arc::new(NoopEventSink),
        ConnectParams {
            clipboard_enabled: false,
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
            port: server.port,
            password: "clip-pw".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        client_clipboard.clone(),
    )
    .await
    .unwrap();

    let key = info.clipboard_connection_id.parse().unwrap();
    assert!(!info.clipboard_enabled);
    assert!(!client_clipboard.session_enabled(key));
    assert!(clipboard.is_enabled() && client_clipboard.is_enabled());

    // No UI command has run: copying immediately after connect must be blocked both ways.
    clipboard.on_local_change(vnt_clip::ClipboardContent::Text("host-before-ui".into()));
    client_clipboard.on_local_change(vnt_clip::ClipboardContent::Text("client-before-ui".into()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(host_writes.lock().unwrap().is_empty());
    assert!(client_writes.lock().unwrap().is_empty());

    // Enabling the same session must restore both directions, proving the channel is live.
    assert!(client_clipboard.set_session_enabled(key, true));
    clipboard.on_local_change(vnt_clip::ClipboardContent::Text("host-enabled".into()));
    client_clipboard.on_local_change(vnt_clip::ClipboardContent::Text("client-enabled".into()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while host_writes.lock().unwrap().is_empty() || client_writes.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both directions must resume after enabling the session");
    for (log, expected) in [
        (&host_writes, "client-enabled"),
        (&client_writes, "host-enabled"),
    ] {
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(matches!(&log[0].0, vnt_clip::ClipboardContent::Text(text) if text == expected));
    }

    client.disconnect();
    wait_for_session_count(&server, 0).await;
    server.stop().await;
}

/// Text copies synchronize through the real control channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_text_syncs_over_the_control_channel() {
    let (writer, writes) = recording_writer();
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(writer);
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "clip-pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let (client, _info) = client::connect_split(
        Arc::new(NoopEventSink),
        ConnectParams {
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
            port: server.port,
            password: "clip-pw".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        clipboard.clone(),
    )
    .await
    .unwrap();

    clipboard.on_local_change(vnt_clip::ClipboardContent::Text("剪贴板 e2e".into()));
    // 同一进程内被控端会话与控制端会话各收一份（两者都是本机剪贴板的听众）
    tokio::time::timeout(Duration::from_secs(5), async {
        while writes.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("两端都应写入剪贴板");
    let texts: Vec<String> = writes
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(content, _)| match content {
            vnt_clip::ClipboardContent::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["剪贴板 e2e".to_string(); 2]);

    client.disconnect();
    wait_for_session_count(&server, 0).await;
    server.stop().await;
}

/// Multi-chunk PNGs cross the actual client/server TCP routes in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_images_sync_over_tcp_in_both_directions() {
    let (host_writer, host_writes) = recording_writer();
    let host_clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(host_writer);
    let (client_writer, client_writes) = recording_writer();
    let client_clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(client_writer);
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "clip-pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: host_clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let (client, _info) = client::connect_split(
        Arc::new(NoopEventSink),
        ConnectParams {
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
            port: server.port,
            password: "clip-pw".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        client_clipboard.clone(),
    )
    .await
    .unwrap();

    let mut seed = 123456789u32;
    let pixels: Vec<u8> = (0..256 * 256 * 4)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let image = vnt_clip::image::from_rgba(256, 256, &pixels).unwrap();
    assert!(image.png.len() > vnt_proto::control::CLIPBOARD_CHUNK_BYTES);
    for (source, received) in [
        (&host_clipboard, &client_writes),
        (&client_clipboard, &host_writes),
    ] {
        source.on_local_change(vnt_clip::ClipboardContent::Image(image.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while received.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("image must arrive over real TCP");
        let log = received.lock().unwrap();
        assert!(matches!(&log[0].0, vnt_clip::ClipboardContent::Image(actual) if actual == &image));
        assert_eq!(log.len(), 1);
    }
    client.disconnect();
    wait_for_session_count(&server, 0).await;
    server.stop().await;
}

/// 仅共享屏幕（观看模式）：view_only 随 Welcome/会话信息下发，
/// 双向剪贴板同步全部被挡住，对端硬发剪贴板文本也被服务端忽略。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn view_only_session_blocks_clipboard_and_advertises_the_flag() {
    let (host_writer, host_writes) = recording_writer();
    let host_clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(host_writer);
    let (client_writer, client_writes) = recording_writer();
    let client_clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(client_writer);
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "view-pw".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: host_clipboard.clone(),
            synthetic: true,
            view_only: true,
        },
    )
    .await
    .unwrap();
    let (client, info) = client::connect_split(
        Arc::new(NoopEventSink),
        ConnectParams {
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
            port: server.port,
            password: "view-pw".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        client_clipboard.clone(),
    )
    .await
    .unwrap();
    assert!(info.view_only, "观看模式必须随 Welcome 下发");
    assert!(
        server.active_sessions().iter().all(|s| s.view_only),
        "被控端会话信息也要标记观看模式"
    );

    // 两端各自本机复制：会话剪贴板均已禁用，谁都不该把文本写进对端
    host_clipboard.on_local_change(vnt_clip::ClipboardContent::Text("host-copy".into()));
    client_clipboard.on_local_change(vnt_clip::ClipboardContent::Text("client-copy".into()));
    // 不知情的对端仍可能硬发剪贴板文本：服务端 reader 直接丢弃
    client
        .control_tx
        .send(ControlMessage::ClipboardText(pb::ClipboardText {
            text: "sneaky".into(),
        }))
        .await
        .unwrap();
    let image = vnt_clip::image::from_rgba(1, 1, &[255, 0, 0, 255]).unwrap();
    host_clipboard.on_local_change(vnt_clip::ClipboardContent::Image(image.clone()));
    client_clipboard.on_local_change(vnt_clip::ClipboardContent::Image(image.clone()));
    client
        .control_tx
        .send(ControlMessage::ClipboardImage(pb::ClipboardImage {
            image_id: 1,
            width: 1,
            height: 1,
            total_bytes: image.png.len() as u32,
        }))
        .await
        .unwrap();
    client
        .control_tx
        .send(ControlMessage::ClipboardImageChunk(
            pb::ClipboardImageChunk {
                image_id: 1,
                offset: 0,
                data: image.png,
            },
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        host_writes.lock().unwrap().is_empty(),
        "观看模式下被控端不得写入剪贴板"
    );
    assert!(
        client_writes.lock().unwrap().is_empty(),
        "观看模式下主控端不得写入剪贴板"
    );

    // 剪贴板禁用不影响会话本身：仍在线，正常断开
    wait_for_session_count(&server, 1).await;
    client.disconnect();
    wait_for_session_count(&server, 0).await;
    server.stop().await;
}

/// Real TCP metadata publication and on-demand reads, in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(windows)]
async fn clipboard_native_reads_are_lazy_bidirectional_and_repeatable() {
    use std::os::windows::fs::OpenOptionsExt;
    use vnt_clip::native::FileProvider;
    type OfferLog =
        Arc<std::sync::Mutex<Option<(Arc<vnt_proto::pb::ClipboardOffer>, Arc<dyn FileProvider>)>>>;
    fn peer() -> (Arc<vnt_remote_lib::clipboard::ClipboardService>, OfferLog) {
        let log: OfferLog = Arc::new(std::sync::Mutex::new(None));
        let target = log.clone();
        let service = vnt_remote_lib::clipboard::ClipboardService::with_publishers(
            Arc::new(|_, _| {}),
            Arc::new(move |offer, provider, _| {
                *target.lock().unwrap() = Some((offer, provider));
                Ok(())
            }),
        );
        (service, log)
    }
    async fn published(
        log: &OfferLog,
    ) -> (Arc<vnt_proto::pb::ClipboardOffer>, Arc<dyn FileProvider>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(offer) = log.lock().unwrap().as_ref() {
                    return offer.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    let (host_clipboard, host_log) = peer();
    let (client_clipboard, client_log) = peer();
    let server = server::start(
        Arc::new(NoopEventSink),
        server::ServerConfig {
            audio: false,
            password: "clip-file".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: true,
            clipboard: host_clipboard.clone(),
            synthetic: true,
            view_only: false,
        },
    )
    .await
    .unwrap();
    let (client, _) = client::connect_split(
        Arc::new(NoopEventSink),
        ConnectParams {
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
            transport: vnt_proto::Transport::Tcp,
            host: "127.0.0.1".into(),
            port: server.port,
            password: "clip-file".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
        },
        client_clipboard.clone(),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("payload");
    std::fs::create_dir_all(source.join("empty-directory")).unwrap();
    std::fs::write(source.join("empty-file"), []).unwrap();
    let payload: Vec<u8> = (0..90_000).map(|i| (i % 251) as u8).collect();
    std::fs::write(source.join("report.bin"), &payload).unwrap();
    host_clipboard
        .offer_paths(std::slice::from_ref(&source))
        .unwrap();
    let (manifest, provider) = published(&client_log).await;
    assert!(manifest
        .items
        .iter()
        .any(|i| i.directory && i.rel_path.ends_with("empty-directory")));
    let index = manifest
        .items
        .iter()
        .position(|i| i.rel_path.ends_with("report.bin"))
        .unwrap() as u32;
    assert!(std::fs::OpenOptions::new()
        .write(true)
        .open(source.join("report.bin"))
        .is_ok());
    for _ in 0..2 {
        let op = provider.begin().unwrap();
        let expected = payload.clone();
        tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            while bytes.len() < expected.len() {
                bytes.extend(op.read(index, bytes.len() as u64, 32768).unwrap());
            }
            assert_eq!(bytes, expected);
            op.finish(true, "Shell confirmed");
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Reverse direction over the same connection, including a read error on a retired offer.
    client_clipboard
        .offer_paths(&[source.join("report.bin")])
        .unwrap();
    let (_, reverse) = published(&host_log).await;
    let op = reverse.begin().unwrap();
    assert_eq!(
        tokio::task::spawn_blocking(move || {
            let bytes = op.read(0, 37, 1024).unwrap();
            op.finish(false, "用户已取消");
            bytes
        })
        .await
        .unwrap(),
        payload[37..1061]
    );
    // A network failure must release client-side source handles even while the UI
    // retains its ClientHandle to display the disconnection reason.
    let active = reverse.begin().unwrap();
    let reading = active.clone();
    tokio::task::spawn_blocking(move || reading.read(0, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(std::fs::OpenOptions::new()
        .write(true)
        .open(source.join("report.bin"))
        .is_ok());
    assert!(std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(source.join("report.bin"))
        .is_err());
    server.stop().await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(source.join("report.bin"))
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("network failure must release source handles without dropping ClientHandle");
    assert!(tokio::task::spawn_blocking(move || active.read(0, 0, 1))
        .await
        .unwrap()
        .is_err());
    client.disconnect();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_switch_rejects_late_frames_from_the_previous_desktop() {
    for transport in [vnt_proto::Transport::Tcp, vnt_proto::Transport::Hybrid] {
        let mut bound = None;
        for _ in 0..256 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            match tokio::net::UdpSocket::bind(address).await {
                Ok(udp) => {
                    bound = Some((listener, udp));
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrInUse
                    ) => {}
                Err(error) => panic!("mock UDP bind: {error}"),
            }
        }
        let (listener, udp) = bound.expect("available TCP/UDP test port");
        let address = listener.local_addr().unwrap();
        let (begin, start) = tokio::sync::oneshot::channel();
        let (done, stop) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            // Exercise the same managed control/file/media lanes as the real
            // host, including recovery credentials and TCP media attachment.
            let (connected, mut sessions) = tokio::sync::mpsc::channel(1);
            let acceptor = tokio::spawn(async move {
                let listener_sessions = vnt_net::session::SessionListener::default();
                let mut attachments = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, peer) = accepted.unwrap();
                            let listener_sessions = listener_sessions.clone();
                            let connected = connected.clone();
                            attachments.spawn(async move {
                                if let Some(channel) = listener_sessions.accept(stream, peer).await.unwrap() {
                                    connected.send(channel).await.unwrap();
                                }
                            });
                        }
                        Some(result) = attachments.join_next(), if !attachments.is_empty() => {
                            result.unwrap();
                        }
                    }
                }
            });
            let mut channel = sessions.recv().await.unwrap();
            assert!(matches!(
                recv_channel(&mut channel).await.unwrap(),
                Some(ControlMessage::Hello(_))
            ));
            let _ = vnt_net::auth::challenge(&mut channel).await.unwrap();
            channel
                .send(&ControlMessage::Welcome(pb::Welcome {
                    system: vnt_proto::system::local() as i32,
                    color_depth: 24,
                    screens: vec![pb::ScreenInfo {
                        stream_id: 1,
                        generation: 1,
                        name: "Screen".into(),
                        width: 64,
                        height: 48,
                        fps: 30,
                        codec: pb::Codec::H264 as i32,
                        available: true,
                        ..Default::default()
                    }],
                    host_edition: pb::HostEdition::Portable as i32,
                    proto_ver: vnt_proto::PROTO_VER,
                    transport: transport.to_proto() as i32,
                    session_id: 42,
                    bind_token: if transport == vnt_proto::Transport::Hybrid {
                        vec![7; 16]
                    } else {
                        vec![]
                    },
                    codec: pb::Codec::H264 as i32,
                    width: 64,
                    height: 48,
                    fps: 30,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let target = if transport == vnt_proto::Transport::Hybrid {
                let mut buf = vec![0; 2048];
                let (_, peer) = udp.recv_from(&mut buf).await.unwrap();
                channel
                    .send(&ControlMessage::UdpBindAck(pb::UdpBindAck {
                        session_id: 42,
                    }))
                    .await
                    .unwrap();
                Some(peer)
            } else {
                None
            };
            start.await.unwrap();
            if transport == vnt_proto::Transport::Tcp {
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if matches!(recv_channel(&mut channel).await.unwrap(), Some(ControlMessage::TransportState(state)) if state.kind == 2 && state.available) {
                            break;
                        }
                    }
                }).await.expect("TCP media attachment");
            }
            channel
                .send(&ControlMessage::ScreensState(pb::ScreensState {
                    revision: 2,
                    active_stream_id: 1,
                    screens: vec![pb::ScreenInfo {
                        stream_id: 1,
                        generation: 2,
                        name: "Screen".into(),
                        width: 64,
                        height: 48,
                        fps: 30,
                        codec: pb::Codec::H264 as i32,
                        available: true,
                        ..Default::default()
                    }],
                }))
                .await
                .unwrap();
            // Ping/Pong on the ordered control lane acknowledges that the
            // client has applied DesktopState, unlike a keyframe request that
            // can also originate from initial media attachment.
            let acknowledged = 0xD35C;
            channel
                .send(&ControlMessage::Ping(pb::Ping {
                    timestamp_ms: acknowledged,
                }))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match recv_channel(&mut channel).await.unwrap().unwrap() {
                        ControlMessage::Pong(p) if p.timestamp_ms == acknowledged => break,
                        ControlMessage::Ping(p) => channel
                            .send(&ControlMessage::Pong(pb::Pong {
                                timestamp_ms: p.timestamp_ms,
                            }))
                            .await
                            .unwrap(),
                        _ => {}
                    }
                }
            })
            .await
            .expect("client must apply the new desktop state");
            let mut stop = stop;
            for seq in 1..=120 {
                // Several spaced stale frames ensure a one-time media queue
                // flush cannot accidentally make the timestamp assertion pass.
                let pts = if seq <= 3 { 50 } else { 100 };
                if let Some(target) = target {
                    let mut packets = Vec::new();
                    vnt_proto::udp::fragment_frame(
                        42,
                        seq,
                        true,
                        pts,
                        &[1; 8],
                        vnt_proto::udp::DEFAULT_MTU - 8,
                        &mut packets,
                    )
                    .unwrap();
                    for mut packet in packets {
                        packet[10] |= 0x04;
                        let route = [
                            1u32.to_be_bytes(),
                            (if seq <= 3 { 1u32 } else { 2u32 }).to_be_bytes(),
                        ]
                        .concat();
                        packet.splice(
                            vnt_proto::udp::FRAG_HEADER_LEN..vnt_proto::udp::FRAG_HEADER_LEN,
                            route,
                        );
                        udp.send_to(&packet, target).await.unwrap();
                    }
                } else {
                    channel
                        .send(&vnt_proto::screen::wrap(
                            1,
                            if seq <= 3 { 1 } else { 2 },
                            ControlMessage::VideoFrame(pb::VideoFrame {
                                dependencies: None,
                                keyframe: true,
                                pts_ms: pts,
                                data: vec![1; 8],
                            }),
                        ))
                        .await
                        .unwrap();
                }
                // Model an ongoing new-desktop stream. Initial media attachment
                // may flush queued frames before its generation is observed.
                if tokio::select! {
                    _ = &mut stop => true,
                    _ = tokio::time::sleep(Duration::from_millis(33)) => false,
                } {
                    break;
                }
            }
            acceptor.abort();
            let _ = acceptor.await;
        });
        let (client, info) = client::connect_split(
            Arc::new(NoopEventSink),
            ConnectParams {
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
                transport,
                host: "127.0.0.1".into(),
                port: address.port(),
                password: "test".into(),
                codecs: vec!["h264".into()],
                fps: 30,
                bitrate_kbps: 8000,
                audio_enabled: false,
                audio_device_id: None,
            },
            test_clipboard(),
        )
        .await
        .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/?t={}",
            info.ws_port, info.token
        ))
        .await
        .unwrap();
        begin.send(()).unwrap();
        let pts = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if let Some(Ok(Message::Binary(frame))) = ws.next().await {
                    break u32::from_be_bytes(frame[9..13].try_into().unwrap());
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            pts, 100,
            "old desktop keyframes must never reach the viewer"
        );
        drop(ws);
        drop(client);
        let _ = done.send(());
        server.await.unwrap();
    }
}

async fn recv_channel<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    channel: &mut ControlChannel<S>,
) -> std::io::Result<Option<ControlMessage>> {
    loop {
        return match channel.recv().await? {
            Some(ControlMessage::ScreenPacket(packet)) => Ok(vnt_proto::screen::unwrap(&packet)),
            Some(ControlMessage::ScreensState(_)) => continue,
            message => Ok(message),
        };
    }
}
