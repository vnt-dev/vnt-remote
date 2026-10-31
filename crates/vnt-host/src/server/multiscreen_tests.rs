use super::*;
use crate::config::{AppConfig, CaptureTarget};
use std::collections::HashSet;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an accessible interactive Windows desktop; captures pixels in memory over loopback"]
async fn native_full_display_and_region_stream_concurrently() {
    let monitors = vnt_capture::list_monitors().unwrap();
    let monitor = monitors.first().expect("physical display");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let saved = AppConfig {
        password: "native-multiscreen-test".into(),
        hardware_encode: false,
        allow_control: false,
        audio: false,
        clipboard: false,
        monitor_index: monitor.index,
        screen_regions: vec![vnt_capture::ScreenRegion {
            id: "native-region".into(),
            name: "native region".into(),
            monitor_name: monitor.name.clone(),
            screen_width: monitor.width,
            screen_height: monitor.height,
            orientation: monitor.orientation,
            x: 0,
            y: 0,
            width: monitor.width.min(400) & !1,
            height: monitor.height.min(300) & !1,
        }],
        selected_targets: vec![
            CaptureTarget::Monitor {
                name: monitor.name.clone(),
            },
            CaptureTarget::Region {
                id: "native-region".into(),
            },
        ],
        ..Default::default()
    };
    crate::store::save_at(&path, &saved).unwrap();
    let host = start_portable(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: saved.password.clone(),
            port: 0,
            monitor_index: monitor.index,
            region: None,
            fps: 15,
            bitrate_kbps: 2000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: false,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
        path,
    )
    .await
    .unwrap();
    let (mut client, welcome) =
        connect(host.port, &saved.password, vnt_proto::Transport::Tcp).await;
    assert_eq!(welcome.screens.len(), 2);
    collect_tcp(&mut client, &[1, 2]).await;
    client
        .send(&ControlMessage::ScreenCommand(pb::ScreenCommand {
            action: 0,
            stream_id: 2,
            revision: 0,
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ControlMessage::ScreensState(state)) = client.recv().await.unwrap() {
                if state.active_stream_id == 2 {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    collect_tcp(&mut client, &[1, 2]).await;
    client
        .send(&ControlMessage::ScreenCommand(pb::ScreenCommand {
            action: 1,
            stream_id: 1,
            revision: 0,
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ControlMessage::ScreensState(state)) = client.recv().await.unwrap() {
                if state.screens.len() == 1 {
                    assert_eq!(state.active_stream_id, 2);
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    collect_tcp(&mut client, &[2]).await;
    host.stop().await;
}

async fn connect(
    port: u16,
    password: &str,
    transport: vnt_proto::Transport,
) -> (vnt_net::session::SessionChannel, pb::Welcome) {
    connect_fec(port, password, transport, false).await
}

async fn connect_fec(
    port: u16,
    password: &str,
    transport: vnt_proto::Transport,
    fec: bool,
) -> (vnt_net::session::SessionChannel, pb::Welcome) {
    let (client, welcome, _) = connect_media(port, password, transport, fec).await;
    (client, welcome)
}
async fn connect_media(
    port: u16,
    password: &str,
    transport: vnt_proto::Transport,
    fec: bool,
) -> (
    vnt_net::session::SessionChannel,
    pb::Welcome,
    Option<UdpSocket>,
) {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let (mut client, media) = if transport == vnt_proto::Transport::Quic {
        let (client, media) = vnt_net::quic::connect(address, password.to_string())
            .await
            .unwrap();
        (client, Some(media))
    } else {
        (vnt_net::session::connect(address).await.unwrap(), None)
    };
    client
        .send(&ControlMessage::Hello(pb::Hello {
            desktop_effects: Some(Default::default()),
            color_depth: if fec { 15 } else { 16 },
            capabilities: vnt_proto::platform_capabilities(),
            fec_version: u32::from(fec),
            proto_ver: vnt_proto::PROTO_VER,
            transport: transport.to_proto() as i32,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 15,
            bitrate_kbps: 2000,
            audio_enabled: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, password).await.unwrap();
    let welcome = match client.recv().await.unwrap().unwrap() {
        ControlMessage::Welcome(w) => w,
        other => panic!("{other:?}"),
    };
    assert_eq!(welcome.color_depth, if fec { 15 } else { 16 });
    (client, welcome, media)
}

async fn collect_tcp(client: &mut vnt_net::session::SessionChannel, expected: &[u32]) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut seen = HashSet::new();
        while !expected.iter().all(|id| seen.contains(id)) {
            match client.recv().await.unwrap().expect("session ended") {
                ControlMessage::ScreenPacket(packet)
                    if vnt_proto::screen::is_video(&ControlMessage::ScreenPacket(
                        packet.clone(),
                    )) =>
                {
                    seen.insert(packet.stream_id);
                }
                ControlMessage::SessionEnd(end) => panic!("{}", end.reason),
                _ => {}
            }
        }
    })
    .await
    .expect("all screens must transmit without selecting them");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiscreen_streams_unviewed_targets_and_closes_only_one_session_subscription() {
    exercise(vnt_proto::Transport::Tcp, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiscreen_hybrid_keeps_independent_streams_and_closure() {
    exercise(vnt_proto::Transport::Hybrid, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiscreen_fec_protects_video_and_audio_and_preserves_other_connections() {
    exercise(vnt_proto::Transport::Hybrid, true).await;
}

async fn exercise(transport: vnt_proto::Transport, fec: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let saved = AppConfig {
        password: "multiscreen-test".into(),
        hardware_encode: false,
        allow_control: false,
        audio: true,
        screen_regions: vec![vnt_capture::ScreenRegion {
            id: "r".into(),
            name: "区域".into(),
            monitor_name: "Synthetic".into(),
            screen_width: 1280,
            screen_height: 720,
            orientation: 0,
            x: 10,
            y: 10,
            width: 320,
            height: 240,
        }],
        selected_targets: vec![
            CaptureTarget::Monitor {
                name: "Synthetic".into(),
            },
            CaptureTarget::Region { id: "r".into() },
        ],
        ..Default::default()
    };
    crate::store::save_at(&path, &saved).unwrap();
    let mut portable = portable::Portable::new(path);
    Arc::get_mut(&mut portable).unwrap().probe = Some(Arc::new(|| true));
    let host = start_inner(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: saved.password.clone(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 15,
            bitrate_kbps: 2000,
            hardware_encode: false,
            audio: true,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
        Some(portable.clone()),
    )
    .await
    .unwrap();
    let (mut first, welcome, media) =
        connect_media(host.port, &saved.password, transport, fec).await;
    assert_eq!(welcome.fec_version, u32::from(fec));
    assert_eq!(welcome.screens.len(), 2);
    assert_eq!(
        (welcome.screens[1].width, welcome.screens[1].height),
        (320, 240)
    );
    let (mut second, _) = connect(host.port, &saved.password, vnt_proto::Transport::Tcp).await;
    collect_tcp(&mut second, &[1, 2]).await;
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(64);
    let mut receiver = if transport != vnt_proto::Transport::Tcp {
        let address = SocketAddr::from(([127, 0, 0, 1], host.port));
        let mut receiver = if let Some(media) = media {
            vnt_net::udp::UdpVideoReceiver::from_socket(media, address).unwrap()
        } else {
            vnt_net::udp::UdpVideoReceiver::bind(address).await.unwrap()
        };
        receiver.set_session(welcome.session_id);
        receiver.set_fec(fec);
        receiver.set_audio_sender(audio_tx);
        receiver
            .send_bind(welcome.session_id, &welcome.bind_token)
            .await
            .unwrap();
        Some(receiver)
    } else {
        None
    };
    if let Some(receiver) = receiver.as_mut() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut seen = HashSet::new();
            while seen.len() < 2 {
                let (id, _, _) = receiver.recv_screen_frame().await.unwrap().unwrap();
                seen.insert(id);
            }
        })
        .await
        .unwrap();
    } else {
        collect_tcp(&mut first, &[1, 2]).await;
    }
    if fec {
        assert!(
            audio_rx.try_recv().is_ok(),
            "FEC audio must share the UDP socket without entering video reassembly"
        );
    }
    assert_eq!(
        portable.captures.lock().unwrap().len(),
        1,
        "overlapping targets share the raw capture"
    );
    first
        .send(&ControlMessage::ScreenCommand(pb::ScreenCommand {
            action: 1,
            stream_id: 1,
            revision: 0,
        }))
        .await
        .unwrap();
    let state = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ControlMessage::ScreensState(state)) = first.recv().await.unwrap() {
                if state.screens.len() == 1 {
                    break state;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(state.active_stream_id, 2);
    assert_eq!(state.screens[0].stream_id, 2);
    collect_tcp(&mut second, &[1, 2]).await;
    first
        .send(&ControlMessage::ScreenCommand(pb::ScreenCommand {
            action: 1,
            stream_id: 2,
            revision: state.revision,
        }))
        .await
        .unwrap();
    first
        .send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 987 }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match first.recv().await.unwrap().unwrap() {
                ControlMessage::ScreensState(s) => assert_eq!(s.screens.len(), 1),
                ControlMessage::Pong(p) if p.timestamp_ms == 987 => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    if transport == vnt_proto::Transport::Tcp {
        let sample = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(ControlMessage::AudioFrame(frame)) = first.recv().await.unwrap() {
                    break frame.timestamp_samples;
                }
            }
        })
        .await
        .unwrap();
        portable.capture_available.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(200)).await;
        tokio::time::timeout(Duration::from_secs(3),async{loop{if let Some(ControlMessage::AudioFrame(frame))=first.recv().await.unwrap(){if frame.timestamp_samples>=sample+9600{break;}}}}).await.expect("closing the control owner's video must preserve audio even when its capture becomes unavailable");
        portable.capture_available.store(true, Ordering::Release);
    }
    if let Some(receiver) = receiver.as_mut() {
        // Packets already in flight before the close acknowledgment can arrive;
        // drain them before proving that no new packets for stream one are sent.
        let until = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < until {
            let _ =
                tokio::time::timeout(Duration::from_millis(50), receiver.recv_screen_frame()).await;
        }
        for _ in 0..3 {
            let (id, _, _) =
                tokio::time::timeout(Duration::from_secs(3), receiver.recv_screen_frame())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            assert_eq!(id, 2);
        }
    } else {
        collect_tcp(&mut first, &[2]).await;
    }
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_throttle_tcp_preserves_other_screens_and_sessions_and_restores() {
    exercise_background_throttle(vnt_proto::Transport::Tcp).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_throttle_hybrid_preserves_other_screens_and_sessions_and_restores() {
    exercise_background_throttle(vnt_proto::Transport::Hybrid).await;
}

async fn exercise_background_throttle(transport: vnt_proto::Transport) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let saved = AppConfig {
        password: "background-throttle-test".into(),
        hardware_encode: false,
        allow_control: false,
        audio: false,
        selected_targets: vec![
            CaptureTarget::Monitor {
                name: "Synthetic".into(),
            },
            CaptureTarget::Region {
                id: "background-region".into(),
            },
        ],
        screen_regions: vec![vnt_capture::ScreenRegion {
            id: "background-region".into(),
            name: "region".into(),
            monitor_name: "Synthetic".into(),
            screen_width: 1280,
            screen_height: 720,
            orientation: 0,
            x: 0,
            y: 0,
            width: 320,
            height: 240,
        }],
        ..Default::default()
    };
    crate::store::save_at(&path, &saved).unwrap();
    let mut portable = portable::Portable::new(path);
    Arc::get_mut(&mut portable).unwrap().probe = Some(Arc::new(|| true));
    let host = start_inner(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: saved.password.clone(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 15,
            bitrate_kbps: 2000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
        Some(portable),
    )
    .await
    .unwrap();
    let (first, welcome) = connect(host.port, &saved.password, transport).await;
    let mut receiver = if transport != vnt_proto::Transport::Tcp {
        let mut rx =
            vnt_net::udp::UdpVideoReceiver::bind(SocketAddr::from(([127, 0, 0, 1], host.port)))
                .await
                .unwrap();
        rx.set_session(welcome.session_id);
        rx.send_bind(welcome.session_id, &welcome.bind_token)
            .await
            .unwrap();
        Some(rx)
    } else {
        None
    };
    let udp_task = tokio::spawn(async move {
        if let Some(receiver) = receiver.as_mut() {
            loop {
                if receiver.recv_screen_frame().await.is_err() {
                    break;
                }
            }
        }
    });
    let (second, _) = connect(host.port, &saved.password, vnt_proto::Transport::Tcp).await;
    let (mut read, mut write) = first.into_split();
    let (mut other_read, mut other_write) = second.into_split();
    let (stats_tx, mut stats_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = read.recv().await {
            match msg {
                ControlMessage::ScreenPacket(p) => {
                    if let Some(ControlMessage::StatsReport(s)) = vnt_proto::screen::unwrap(&p) {
                        let _ = stats_tx.send((p.stream_id, p.generation, s));
                    }
                }
                ControlMessage::SessionEnd(e) => panic!("{}", e.reason),
                _ => {}
            }
        }
    });
    let (other_stats_tx, mut other_stats_rx) = tokio::sync::mpsc::unbounded_channel();
    let second_task = tokio::spawn(async move {
        while let Ok(Some(message)) = other_read.recv().await {
            if let ControlMessage::ScreenPacket(p) = message {
                if let Some(ControlMessage::StatsReport(s)) = vnt_proto::screen::unwrap(&p) {
                    let _ = other_stats_tx.send((p.stream_id, s));
                }
            }
        }
    });
    // Wait for current generation after initial configuration.
    let (_, generation, initial) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = stats_rx.recv().await.unwrap();
            if value.0 == 2 {
                break value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(initial.applied_fps, 15);
    // Stale generations must not reach the encoder.
    write
        .send(&vnt_proto::screen::wrap(
            2,
            generation.wrapping_add(10),
            ControlMessage::ScreenThrottle(pb::ScreenThrottle { background: true }),
        ))
        .await
        .unwrap();
    let unchanged = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = stats_rx.recv().await.unwrap();
            if value.0 == 2 {
                break value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(unchanged.2.applied_fps, 15);
    write
        .send(&vnt_proto::screen::wrap(
            2,
            unchanged.1,
            ControlMessage::ScreenThrottle(pb::ScreenThrottle { background: true }),
        ))
        .await
        .unwrap();
    let background_generation = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (id, generation, s) = stats_rx.recv().await.unwrap();
            if id == 1 {
                assert_eq!(s.applied_fps, 15);
            }
            if id == 2 && s.applied_fps == 5 {
                assert!(s.applied_bitrate_kbps <= 666);
                if s.fps <= 7 {
                    break generation;
                }
            }
        }
    })
    .await
    .expect("background must reduce actual sent FPS and encoder budget");
    let other = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (id, s) = other_stats_rx.recv().await.unwrap();
            if id == 2 {
                break s;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(other.applied_fps, 15);
    assert_eq!(other.applied_bitrate_kbps, 2000);
    write
        .send(&vnt_proto::screen::wrap(
            2,
            background_generation,
            ControlMessage::ScreenThrottle(pb::ScreenThrottle { background: false }),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (id, _, s) = stats_rx.recv().await.unwrap();
            if id == 2 && s.applied_fps == 15 {
                assert_eq!(s.applied_bitrate_kbps, 2000);
                break;
            }
        }
    })
    .await
    .expect("visible stream must restore without waiting for network adaptation");
    write
        .send(&ControlMessage::SessionEnd(pb::SessionEnd {
            reason: "done".into(),
        }))
        .await
        .unwrap();
    let _ = other_write
        .send(&ControlMessage::SessionEnd(pb::SessionEnd {
            reason: "done".into(),
        }))
        .await;
    host.stop().await;
    first_task.abort();
    second_task.abort();
    udp_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_streams_all_screens_and_audio() {
    exercise(vnt_proto::Transport::Quic, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_streams_all_screens_and_audio_with_fec() {
    exercise(vnt_proto::Transport::Quic, true).await;
}
