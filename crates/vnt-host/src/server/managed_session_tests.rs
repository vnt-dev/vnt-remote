use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_single_screen_video_audio_and_fec_use_datagrams() {
    for fec in [false, true] {
        let host = start(
            Arc::new(crate::NoopEventSink),
            ServerConfig {
                password: "quic-integration".into(),
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
        )
        .await
        .unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], host.port));
        let (mut client, media) = vnt_net::quic::connect(address, "quic-integration".into())
            .await
            .unwrap();
        client
            .send(&ControlMessage::Hello(pb::Hello {
                system: vnt_proto::system::local() as i32,
                color_depth: 24,
                desktop_effects: Some(Default::default()),
                proto_ver: vnt_proto::PROTO_VER,
                transport: 4,
                codecs: vec![pb::Codec::H264 as i32],
                fps: 15,
                bitrate_kbps: 2000,
                audio_enabled: true,
                fec_version: u32::from(fec),
                ..Default::default()
            }))
            .await
            .unwrap();
        vnt_net::auth::respond(&mut client, "quic-integration")
            .await
            .unwrap();
        let welcome = match recv(&mut client).await.unwrap().unwrap() {
            ControlMessage::Welcome(welcome) => welcome,
            other => panic!("unexpected handshake: {other:?}"),
        };
        assert_eq!(welcome.transport, 4);
        assert_eq!(welcome.fec_version, u32::from(fec));
        let mut receiver = vnt_net::udp::UdpVideoReceiver::from_socket(media, address).unwrap();
        receiver.set_session(welcome.session_id);
        receiver.set_fec(fec);
        let (audio, mut audio_rx) = mpsc::channel(64);
        receiver.set_audio_sender(audio);
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut video_config, mut audio_config) = (false, false);
            while !video_config || !audio_config {
                match recv(&mut client).await.unwrap().unwrap() {
                    ControlMessage::StreamReset(_) => video_config = true,
                    ControlMessage::AudioState(state) => audio_config = state.available,
                    ControlMessage::VideoFrame(_) | ControlMessage::AudioFrame(_) => panic!("QUIC media used a reliable lane"),
                    ControlMessage::SessionEnd(end) => panic!("{}", end.reason),
                    _ => {}
                }
            }
            let (mut video, mut audio) = (false, false);
            while !video || !audio {
                tokio::select! {
                    frame = receiver.recv_frame() => {
                        if let Some(frame) = frame.unwrap() { assert!(!frame.data.is_empty()); video = true; }
                    },
                    Some(frame) = audio_rx.recv() => { assert!(!frame.data.is_empty()); audio = true; }
                }
            }
        }).await.unwrap();
        client
            .send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 77 }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match recv(&mut client).await.unwrap().unwrap() {
                    ControlMessage::Pong(pong) if pong.timestamp_ms == 77 => break,
                    ControlMessage::VideoFrame(_) | ControlMessage::AudioFrame(_) => {
                        panic!("QUIC media used a reliable lane")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        client
            .send(&ControlMessage::SessionEnd(pb::SessionEnd {
                reason: "test complete".into(),
            }))
            .await
            .unwrap();
        host.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_udp_retransmits_a_whole_lost_frame_through_authenticated_control() {
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "repair-test".into(),
            port: 0,
            monitor_index: 0,
            region: Some(vnt_capture::ScreenRegion {
                id: "repair".into(),
                name: "repair".into(),
                monitor_name: "Synthetic".into(),
                screen_width: 1280,
                screen_height: 720,
                orientation: 0,
                x: 0,
                y: 0,
                width: 200,
                height: 150,
            }),
            fps: 30,
            bitrate_kbps: 2000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
    )
    .await
    .unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], host.port));
    let mut old = vnt_net::session::connect(address).await.unwrap();
    old.send(&ControlMessage::Hello(pb::Hello {
        system: vnt_proto::system::local() as i32,
        color_depth: 24,
        desktop_effects: Some(Default::default()),
        capabilities: vnt_proto::platform_capabilities(),
        proto_ver: 4,
        ..Default::default()
    }))
    .await
    .unwrap();
    assert!(
        matches!(recv(&mut old).await.unwrap(), Some(ControlMessage::Denied(d)) if d.reason.contains("协议版本"))
    );
    drop(old);
    let mut client = vnt_net::session::connect(address).await.unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 30,
            bitrate_kbps: 2000,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "repair-test")
        .await
        .unwrap();
    let welcome = match recv(&mut client).await.unwrap().unwrap() {
        ControlMessage::Welcome(w) => w,
        other => panic!("{other:?}"),
    };
    let relay = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut receiver = vnt_net::udp::UdpVideoReceiver::bind(relay.local_addr().unwrap())
        .await
        .unwrap();
    receiver.set_session(welcome.session_id);
    receiver.set_rtt(Some(2));
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    receiver.set_recovery_sender(tx);
    let (mut reader, mut writer) = client.into_split();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let idrs = requests.clone();
    let control = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if matches!(msg, ControlMessage::RequestKeyframe) {
                idrs.fetch_add(1, Ordering::Relaxed);
            }
            if writer.send_owned(msg).await.is_err() {
                break;
            }
        }
    });
    let drain = tokio::spawn(async move { while matches!(reader.recv().await, Ok(Some(_))) {} });
    let proxy = tokio::spawn(async move {
        let mut client = None;
        let mut dropped = None;
        let mut bytes = vec![0; 65536];
        loop {
            let (len, from) = relay.recv_from(&mut bytes).await.unwrap();
            if from != address {
                client = Some(from);
                relay.send_to(&bytes[..len], address).await.unwrap();
                continue;
            }
            let Some(client) = client else {
                continue;
            };
            let (h, payload) = vnt_proto::udp::FragHeader::decode(&bytes[..len]).unwrap();
            let (d, _) = vnt_proto::recovery::decode_dependencies(if h.flags & 0x04 != 0 {
                &payload[8..]
            } else {
                payload
            })
            .unwrap();
            if !h.is_keyframe() && dropped.is_none() {
                dropped = Some((d.epoch, d.frame_id));
            }
            if dropped == Some((d.epoch, d.frame_id))
                && h.flags & vnt_proto::recovery::FLAG_REPAIR == 0
            {
                continue;
            }
            relay.send_to(&bytes[..len], client).await.unwrap();
        }
    });
    receiver
        .send_bind(welcome.session_id, &welcome.bind_token)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..8 {
            assert!(receiver.recv_frame().await.unwrap().is_some());
        }
    })
    .await
    .unwrap();
    assert_eq!(receiver.recovery_stats(1).recovered_frames, 1);
    assert_eq!(receiver.stats.lost_frames, 0);
    assert_eq!(requests.load(Ordering::Relaxed), 0);
    host.stop().await;
    proxy.abort();
    control.abort();
    drain.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyframe_refresh_preserves_single_screen_throughput() {
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "keyframe-throughput".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
    )
    .await
    .unwrap();
    let mut client = vnt_net::session::connect(SocketAddr::from(([127, 0, 0, 1], host.port)))
        .await
        .unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
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
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "keyframe-throughput")
        .await
        .unwrap();
    let started = Instant::now();
    let mut frames = [0u32; 2];
    let mut refresh = tokio::time::interval(Duration::from_millis(100));
    while started.elapsed() < Duration::from_secs(5) {
        tokio::select! {
            _ = refresh.tick() => {
                if started.elapsed() >= Duration::from_secs(3) {
                    client.send(&ControlMessage::RequestKeyframe).await.unwrap();
                }
            }
            message = recv(&mut client) => {
                let message = message.unwrap().expect("video session ended");
                if let ControlMessage::StatsReport(ref stats) = message {
                    eprintln!("throughput stats: {stats:?}");
                }
                if matches!(message, ControlMessage::VideoFrame(_)) {
                    let elapsed = started.elapsed();
                    if elapsed >= Duration::from_secs(3) { frames[1] += 1; }
                    else if elapsed >= Duration::from_secs(1) { frames[0] += 1; }
                }
            }
        }
    }
    host.stop().await;
    eprintln!(
        "single-screen loopback: baseline={} fps, repeated keyframe requests={} fps",
        frames[0] as f64 / 2.0,
        frames[1] as f64 / 2.0
    );
    assert!(
        frames[0] >= 20,
        "baseline did not produce enough frames: {frames:?}"
    );
    assert!(
        frames[1] * 100 >= frames[0] * 80,
        "keyframe requests stalled healthy video: {frames:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn even_region_dimensions_stream_over_hybrid_transport_without_block_alignment() {
    let region = vnt_capture::ScreenRegion {
        id: "r".into(),
        name: "r".into(),
        monitor_name: "Synthetic".into(),
        screen_width: 1280,
        screen_height: 720,
        orientation: 0,
        x: 101,
        y: 57,
        width: 200,
        height: 150,
    };
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "region-hybrid-test".into(),
            port: 0,
            monitor_index: 0,
            region: Some(region),
            fps: 15,
            bitrate_kbps: 2000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
    )
    .await
    .unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], host.port));
    let mut client = vnt_net::session::connect(address).await.unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 15,
            bitrate_kbps: 2000,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "region-hybrid-test")
        .await
        .unwrap();
    let welcome = match recv(&mut client).await.unwrap().unwrap() {
        ControlMessage::Welcome(w) => w,
        other => panic!("{other:?}"),
    };
    assert_eq!((welcome.width, welcome.height), (200, 150));
    let mut receiver = vnt_net::udp::UdpVideoReceiver::bind(address).await.unwrap();
    receiver.set_session(welcome.session_id);
    receiver
        .send_bind(welcome.session_id, &welcome.bind_token)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match recv(&mut client).await.unwrap().unwrap() {
                ControlMessage::StreamReset(s) => {
                    assert_eq!((s.width, s.height), (200, 150));
                    break;
                }
                ControlMessage::SessionEnd(e) => panic!("{}", e.reason),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), receiver.recv_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(frame.keyframe);
    host.stop().await;
}

#[test]
fn service_target_cache_uses_region_snapshots_and_shares_the_raw_producer() {
    let host = AgentHost::new(ServerConfig {
        password: "test".into(),
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
    });
    let mut region = vnt_capture::ScreenRegion {
        id: "r".into(),
        name: "r".into(),
        monitor_name: "Synthetic".into(),
        screen_width: 1280,
        screen_height: 720,
        orientation: 0,
        x: 100,
        y: 100,
        width: 320,
        height: 240,
    };
    let first = host.for_region(region.clone());
    assert!(Arc::ptr_eq(&first, &host.for_region(region.clone())));
    region.width = 640;
    let second = host.for_region(region);
    assert!(!Arc::ptr_eq(&first, &second));
    assert!(first.capture.shares_source(&second.capture));
    assert!(first.capture.shares_source(&host.capture));
    assert_eq!(first.cfg.region.as_ref().unwrap().width, 320);
    assert_eq!(second.cfg.region.as_ref().unwrap().width, 640);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn region_snapshots_share_capture_and_survive_edits_deletion_and_desktop_pause() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let region = vnt_capture::ScreenRegion {
        id: "r".into(),
        name: "区域".into(),
        monitor_name: "Synthetic".into(),
        screen_width: 1280,
        screen_height: 720,
        orientation: 0,
        x: 100,
        y: 100,
        width: 320,
        height: 240,
    };
    let mut saved = crate::config::AppConfig {
        password: "region-session-test".into(),
        hardware_encode: false,
        allow_control: false,
        audio: false,
        screen_regions: vec![region],
        selected_targets: vec![crate::config::CaptureTarget::Region { id: "r".into() }],
        ..Default::default()
    };
    crate::store::save_at(&path, &saved).unwrap();
    let normal = Arc::new(AtomicBool::new(true));
    let probe = normal.clone();
    let mut portable = portable::Portable::new(path.clone());
    Arc::get_mut(&mut portable).unwrap().probe =
        Some(Arc::new(move || probe.load(Ordering::Acquire)));
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
        Some(portable.clone()),
    )
    .await
    .unwrap();
    async fn frame(client: &mut vnt_net::session::SessionChannel, expected: (u32, u32)) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match recv(client).await.unwrap().unwrap() {
                    ControlMessage::StreamReset(s) => assert_eq!((s.width, s.height), expected),
                    ControlMessage::VideoFrame(f) if f.keyframe => break,
                    ControlMessage::SessionEnd(e) => panic!("{}", e.reason),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
    let mut old = portable_connect(host.port, &saved.password).await;
    assert!(
        matches!(recv(&mut old).await.unwrap(),Some(ControlMessage::Welcome(w)) if (w.width,w.height)==(320,240))
    );
    frame(&mut old, (320, 240)).await;
    saved.screen_regions[0].width = 640;
    saved.screen_regions[0].height = 360;
    crate::store::save_at(&path, &saved).unwrap();
    let mut new = portable_connect(host.port, &saved.password).await;
    assert!(
        matches!(recv(&mut new).await.unwrap(),Some(ControlMessage::Welcome(w)) if (w.width,w.height)==(640,360))
    );
    frame(&mut new, (640, 360)).await;
    assert_eq!(portable.captures.lock().unwrap().len(), 1);
    saved.screen_regions.clear();
    saved.selected_targets.clear();
    crate::store::save_at(&path, &saved).unwrap();
    let mut denied = portable_connect(host.port, &saved.password).await;
    assert!(matches!(
        recv(&mut denied).await.unwrap(),
        Some(ControlMessage::Denied(_))
    ));
    normal.store(false, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(recv(&mut old).await.unwrap(),Some(ControlMessage::DesktopState(s)) if !s.ready)
        {
        }
    })
    .await
    .unwrap();
    old.send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 999 }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match recv(&mut old).await.unwrap().unwrap() {
                ControlMessage::Pong(p) if p.timestamp_ms == 999 => break,
                ControlMessage::VideoFrame(_) => panic!("region video leaked while paused"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    normal.store(true, Ordering::Release);
    portable.capture_available.store(true, Ordering::Release);
    frame(&mut old, (320, 240)).await;
    frame(&mut new, (640, 360)).await;
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hybrid_defers_capture_subscription_until_udp_is_bound() {
    let capture = SharedCapture::synthetic(16, 16);
    // Any attempt to subscribe fails immediately. The control connection must
    // remain healthy until a valid UDP binding actually enables video.
    capture.cancel();
    let host = start_with_capture(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "lazy-capture-test".into(),
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
        capture,
    )
    .await
    .unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], host.port));
    let mut client = vnt_net::session::connect(address).await.unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 15,
            bitrate_kbps: 2000,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "lazy-capture-test")
        .await
        .unwrap();
    let welcome = match recv(&mut client).await.unwrap().unwrap() {
        ControlMessage::Welcome(welcome) => welcome,
        _ => panic!("missing welcome"),
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 42 }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match recv(&mut client).await.unwrap().unwrap() {
                ControlMessage::Pong(pong) if pong.timestamp_ms == 42 => break,
                ControlMessage::SessionEnd(end) => {
                    panic!("capture started before UDP binding: {}", end.reason)
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.send_to(
        &vnt_proto::control::encode_udp_bind_frame(welcome.session_id, &welcome.bind_token),
        address,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ControlMessage::ScreensState(state)) = client.recv().await.unwrap() {
                if state
                    .screens
                    .iter()
                    .any(|screen| !screen.available && !screen.reason.is_empty())
                {
                    break;
                }
            }
        }
    })
    .await
    .expect("UDP binding must expose the failed screen subscription");
    host.stop().await;
}

async fn portable_connect(port: u16, password: &str) -> vnt_net::session::SessionChannel {
    let mut client = vnt_net::session::connect(([127, 0, 0, 1], port).into())
        .await
        .unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            transport: vnt_proto::Transport::Tcp.to_proto() as i32,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 15,
            bitrate_kbps: 2000,
            audio_enabled: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, password).await.unwrap();
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn portable_config_changes_apply_only_to_new_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let mut saved = crate::config::AppConfig {
        selected_targets: vec![crate::config::CaptureTarget::Monitor {
            name: "Synthetic".into(),
        }],
        password: "session-policy-test".into(),
        hardware_encode: false,
        ..Default::default()
    };
    crate::store::save_at(&path, &saved).unwrap();
    let writes = Arc::new(AtomicU32::new(0));
    let count = writes.clone();
    let clipboard = crate::clipboard::ClipboardService::with_writer(Arc::new(move |_, _| {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    clipboard.set_enabled(false);
    let mut portable = portable::Portable::new(path.clone());
    Arc::get_mut(&mut portable).unwrap().probe = Some(Arc::new(|| true));
    portable.desktop.store(true, Ordering::Release);
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
            clipboard,
        },
        Some(portable.clone()),
    )
    .await
    .unwrap();
    let mut original = portable_connect(host.port, &saved.password).await;
    assert!(
        matches!(recv(&mut original).await.unwrap(), Some(ControlMessage::Welcome(w))
        if !w.view_only && w.audio_enabled && !w.hardware)
    );
    // Wait for the original session's clipboard registration and desktop readiness.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            recv(&mut original).await.unwrap(),
            Some(ControlMessage::VideoFrame(_))
        ) {}
    })
    .await
    .unwrap();

    saved.allow_control = false;
    saved.audio = false;
    saved.clipboard = false;
    crate::store::save_at(&path, &saved).unwrap();
    let mut restricted = portable_connect(host.port, &saved.password).await;
    assert!(
        matches!(recv(&mut restricted).await.unwrap(), Some(ControlMessage::Welcome(w))
        if w.view_only && !w.audio_enabled && !w.hardware)
    );
    original
        .send(&ControlMessage::ClipboardText(pb::ClipboardText {
            text: "original session still allows clipboard".into(),
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while writes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    saved.allow_control = true;
    crate::store::save_at(&path, &saved).unwrap();
    let mut no_clipboard = portable_connect(host.port, &saved.password).await;
    assert!(
        matches!(recv(&mut no_clipboard).await.unwrap(), Some(ControlMessage::Welcome(w))
        if !w.view_only && !w.audio_enabled)
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            recv(&mut no_clipboard).await.unwrap(),
            Some(ControlMessage::VideoFrame(_))
        ) {}
    })
    .await
    .unwrap();
    let before = writes.load(Ordering::SeqCst);
    no_clipboard
        .send(&ControlMessage::ClipboardText(pb::ClipboardText {
            text: "new session disallows clipboard".into(),
        }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(writes.load(Ordering::SeqCst), before);

    saved.selected_targets = vec![crate::config::CaptureTarget::Monitor {
        name: "offline".into(),
    }];
    crate::store::save_at(&path, &saved).unwrap();
    let mut missing_monitor = portable_connect(host.port, &saved.password).await;
    assert!(matches!(
        recv(&mut missing_monitor).await.unwrap(),
        Some(ControlMessage::Denied(_)) | Some(ControlMessage::Welcome(_))
    ));
    assert!(saved
        .selected_targets
        .iter()
        .any(|t| matches!(t, crate::config::CaptureTarget::Monitor { name } if name == "offline")));
    assert!(portable.available());
    assert!(host.active_sessions().iter().any(|s| !s.view_only));
    assert!(host.active_sessions().iter().any(|s| s.view_only));
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn portable_keeps_heartbeat_during_pause_resumes_with_keyframe_and_reads_updated_password() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("portable/config.json");
    let mut saved = crate::config::AppConfig {
        selected_targets: vec![crate::config::CaptureTarget::Monitor {
            name: "Synthetic".into(),
        }],
        password: "first-password".into(),
        hardware_encode: false,
        audio: false,
        ..Default::default()
    };
    crate::store::save_at(&path, &saved).unwrap();
    let normal = Arc::new(AtomicBool::new(true));
    let mut portable = portable::Portable::new(path.clone());
    let probe = normal.clone();
    Arc::get_mut(&mut portable).unwrap().probe =
        Some(Arc::new(move || probe.load(Ordering::Acquire)));
    portable.desktop.store(true, Ordering::Release);
    let mut config = ServerConfig {
        password: saved.password.clone(),
        port: 0,
        monitor_index: 0,
        region: None,
        fps: 15,
        bitrate_kbps: 2000,
        hardware_encode: false,
        audio: false,
        view_only: false,
        synthetic: true,
        clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
    };
    let host = start_inner(
        Arc::new(crate::NoopEventSink),
        config.clone(),
        Some(portable.clone()),
    )
    .await
    .unwrap();
    config.port = host.port;
    let collision = start_inner(
        Arc::new(crate::NoopEventSink),
        config.clone(),
        Some(portable.clone()),
    )
    .await;
    assert!(matches!(collision, Err(error) if error.contains("TCP 绑定端口")));
    let mut client = portable_connect(host.port, "first-password").await;
    assert!(matches!(
        recv(&mut client).await.unwrap(),
        Some(ControlMessage::Welcome(w)) if w.host_edition == pb::HostEdition::Portable as i32
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            recv(&mut client).await.unwrap(),
            Some(ControlMessage::VideoFrame(_))
        ) {}
    })
    .await
    .unwrap();

    normal.store(false, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(recv(&mut client).await.unwrap(), Some(ControlMessage::DesktopState(s)) if !s.ready) { break; }
        }
    }).await.unwrap();
    client
        .send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 123 }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match recv(&mut client).await.unwrap().unwrap() {
                ControlMessage::Pong(p) if p.timestamp_ms == 123 => break,
                ControlMessage::VideoFrame(_) => panic!("video leaked while paused"),
                ControlMessage::SessionEnd(end) => panic!("paused session ended: {}", end.reason),
                _ => {}
            }
        }
    })
    .await
    .unwrap();

    normal.store(true, Ordering::Release);
    // A synthetic source models the first new frame of a recreated real capture.
    portable.capture_available.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut floor = None;
        let mut reset = false;
        loop {
            match recv(&mut client).await.unwrap().unwrap() {
                ControlMessage::DesktopState(s) if s.ready => floor = Some(s.min_pts_ms),
                ControlMessage::StreamReset(_) => reset = true,
                ControlMessage::VideoFrame(frame) if floor.is_some() => {
                    assert!(reset);
                    assert!(frame.keyframe);
                    assert!(frame.pts_ms >= floor.unwrap());
                    break;
                }
                ControlMessage::SessionEnd(end) => panic!("resume ended: {}", end.reason),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    // A stray/forged unsupported SAS must leave this very session usable.
    for timestamp_ms in 200..205 {
        client.send(&ControlMessage::SendSas).await.unwrap();
        client
            .send(&ControlMessage::Ping(pb::Ping { timestamp_ms }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut pong, mut video) = (false, false);
            while !pong || !video {
                match recv(&mut client)
                    .await
                    .unwrap()
                    .expect("SAS disconnected portable session")
                {
                    ControlMessage::Pong(p) if p.timestamp_ms == timestamp_ms => pong = true,
                    ControlMessage::VideoFrame(_) => video = true,
                    ControlMessage::DesktopState(_) => {
                        panic!("unsupported SAS changed desktop state")
                    }
                    ControlMessage::SessionEnd(end) => panic!("SAS ended session: {}", end.reason),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }

    saved.password = "replacement-password".into();
    crate::store::save_at(&path, &saved).unwrap();
    let mut rejected = portable_connect(host.port, "first-password").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match recv(&mut rejected).await.unwrap() {
                Some(ControlMessage::Denied(_)) => break,
                Some(ControlMessage::TransportState(_)) => {}
                unexpected => panic!("unexpected authentication result: {unexpected:?}"),
            }
        }
    })
    .await
    .unwrap();
    let mut accepted = portable_connect(host.port, "replacement-password").await;
    assert!(matches!(
        recv(&mut accepted).await.unwrap(),
        Some(ControlMessage::Welcome(_))
    ));
    std::fs::write(&path, "corrupted config").unwrap();
    let mut invalid = portable_connect(host.port, "replacement-password").await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), recv(&mut invalid))
            .await
            .unwrap()
            .unwrap(),
        Some(ControlMessage::Denied(_))
    ));
    let port = host.port;
    host.stop().await;
    assert!(UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .is_ok());
    let restarted = start_portable(Arc::new(crate::NoopEventSink), config, path)
        .await
        .unwrap();
    restarted.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_remote_tcp_streams_synthetic_video_and_audio_after_configuration() {
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "transport-integration-test".into(),
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
    )
    .await
    .unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], host.port));
    let mut client = vnt_net::session::connect(address).await.unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            transport: vnt_proto::Transport::Tcp.to_proto() as i32,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 15,
            bitrate_kbps: 2000,
            audio_enabled: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "transport-integration-test")
        .await
        .unwrap();
    assert!(
        matches!(recv(&mut client).await.unwrap(),Some(ControlMessage::Welcome(w)) if w.session_token.len()==32 && w.view_only)
    );
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let (mut video_config, mut audio_config, mut video, mut audio) =
            (false, false, false, false);
        while !video || !audio {
            match recv(&mut client)
                .await
                .unwrap()
                .expect("remote session ended")
            {
                ControlMessage::StreamReset(_) => video_config = true,
                ControlMessage::AudioState(state) => audio_config = state.available,
                ControlMessage::VideoFrame(frame) => {
                    assert!(video_config, "video frame overtook stream configuration");
                    if !video {
                        assert!(frame.keyframe);
                    }
                    video = true;
                }
                ControlMessage::AudioFrame(_) => {
                    assert!(audio_config, "audio overtook audio state");
                    audio = true;
                }
                ControlMessage::Ping(p) => {
                    client
                        .send(&ControlMessage::Pong(pb::Pong {
                            timestamp_ms: p.timestamp_ms,
                        }))
                        .await
                        .unwrap();
                }
                ControlMessage::SessionEnd(end) => panic!("stream ended: {}", end.reason),
                _ => {}
            }
        }
    })
    .await;
    client
        .send(&ControlMessage::SessionEnd(pb::SessionEnd {
            reason: "test complete".into(),
        }))
        .await
        .unwrap();
    host.stop().await;
    result.expect("managed media did not start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_video_consumer_keeps_the_control_writer_responsive() {
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "video-pressure-test".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 8000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
    )
    .await
    .unwrap();
    let mut client = vnt_net::session::connect(SocketAddr::from(([127, 0, 0, 1], host.port)))
        .await
        .unwrap();
    client
        .send(&ControlMessage::Hello(pb::Hello {
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
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "video-pressure-test")
        .await
        .unwrap();
    assert!(matches!(
        recv(&mut client).await.unwrap(),
        Some(ControlMessage::Welcome(_))
    ));
    // Do not drain media. Its reader and bounded queues must become pressured
    // while the independently routed control connection remains usable.
    tokio::time::sleep(Duration::from_secs(4)).await;
    client
        .send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 123 }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match recv(&mut client)
                .await
                .unwrap()
                .expect("session ended under video pressure")
            {
                ControlMessage::Pong(p) if p.timestamp_ms == 123 => break,
                ControlMessage::VideoFrame(_) => {
                    panic!("control Pong was blocked behind media backpressure")
                }
                ControlMessage::SessionEnd(end) => {
                    panic!("media pressure ended session: {}", end.reason)
                }
                _ => {}
            }
        }
    })
    .await;
    client
        .send(&ControlMessage::SessionEnd(pb::SessionEnd {
            reason: "test complete".into(),
        }))
        .await
        .unwrap();
    host.stop().await;
    result.expect("Pong did not reach the independent control lane");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_session_scales_its_own_video_without_changing_shared_capture() {
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "scale-test".into(),
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
    )
    .await
    .unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], host.port));
    let mut clients = Vec::new();
    for (depth, mode, target, expected) in [
        (15, 0, (0, 0), (1280, 720)),
        (16, 1, (640, 360), (640, 360)),
        (32, 2, (320, 240), (320, 180)),
    ] {
        let mut client = vnt_net::session::connect(address).await.unwrap();
        client
            .send(&ControlMessage::Hello(pb::Hello {
                system: vnt_proto::system::local() as i32,
                desktop_effects: Some(Default::default()),
                color_depth: depth,
                capabilities: vnt_proto::platform_capabilities(),
                proto_ver: vnt_proto::PROTO_VER,
                transport: vnt_proto::Transport::Tcp.to_proto() as i32,
                codecs: vec![pb::Codec::H264 as i32],
                fps: 15,
                bitrate_kbps: 2000,
                resolution_mode: mode,
                target_width: target.0,
                target_height: target.1,
                ..Default::default()
            }))
            .await
            .unwrap();
        vnt_net::auth::respond(&mut client, "scale-test")
            .await
            .unwrap();
        let Some(ControlMessage::Welcome(welcome)) = recv(&mut client).await.unwrap() else {
            panic!("missing welcome")
        };
        assert_eq!(welcome.color_depth, depth);
        assert_eq!((welcome.width, welcome.height), expected);
        assert_eq!(welcome.resolution_mode, if mode == 2 { 1 } else { mode });
        assert_eq!(welcome.resolution_reason.is_empty(), mode != 2);
        clients.push((client, expected));
    }
    // Keep all three sessions alive while their independent encoder subscriptions run.
    for (client, expected) in &mut clients {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut configured = false;
            loop {
                match recv(client).await.unwrap().unwrap() {
                    ControlMessage::StreamReset(reset) => {
                        assert_eq!((reset.width, reset.height), *expected);
                        configured = true;
                    }
                    ControlMessage::VideoFrame(frame) => {
                        assert!(configured);
                        assert!(frame.keyframe);
                        break;
                    }
                    ControlMessage::SessionEnd(end) => panic!("video failed: {}", end.reason),
                    _ => {}
                }
            }
        })
        .await
        .expect("scaled stream did not start");
    }
    drop(clients);
    host.stop().await;
}

async fn recv(
    channel: &mut vnt_net::session::SessionChannel,
) -> std::io::Result<Option<ControlMessage>> {
    loop {
        return match channel.recv().await? {
            Some(ControlMessage::ScreenPacket(packet)) => Ok(vnt_proto::screen::unwrap(&packet)),
            Some(ControlMessage::ScreensState(_)) => continue,
            message => Ok(message),
        };
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_rejects_unsupported_version_and_incomplete_parameters() {
    let host = start(
        Arc::new(crate::NoopEventSink),
        ServerConfig {
            password: "handshake".into(),
            port: 0,
            monitor_index: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 2000,
            hardware_encode: false,
            audio: false,
            view_only: true,
            synthetic: true,
            clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        },
    )
    .await
    .unwrap();
    for case in 0..6 {
        let mut hello = pb::Hello {
            system: vnt_proto::system::local() as i32,
            proto_ver: vnt_proto::PROTO_VER,
            desktop_effects: Some(Default::default()),
            color_depth: 24,
            transport: 2,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 30,
            bitrate_kbps: 2000,
            ..Default::default()
        };
        match case {
            0 => hello.proto_ver = vnt_proto::PROTO_VER + 1,
            1 => hello.desktop_effects = None,
            2 => hello.color_depth = 0,
            3 => hello.fec_version = 2,
            4 => hello.system = 0,
            _ => hello.system = 999,
        }
        let mut client = vnt_net::session::connect(([127, 0, 0, 1], host.port).into())
            .await
            .unwrap();
        client.send(&ControlMessage::Hello(hello)).await.unwrap();
        if (2..4).contains(&case) {
            vnt_net::auth::respond(&mut client, "handshake")
                .await
                .unwrap();
        }
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(ControlMessage::Denied(_))
        ));
    }
    host.stop().await;
}
