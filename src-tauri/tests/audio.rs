//! Real TCP/UDP audio transport with synthetic capture; never records or plays local sound.
use std::{sync::Arc, time::Duration};
use vnt_net::udp::UdpVideoReceiver;
use vnt_proto::{control::ControlMessage, pb, Transport};
use vnt_remote_lib::{server, NoopEventSink};

#[derive(Default)]
struct AudioEvents(std::sync::Mutex<Vec<(String, serde_json::Value)>>);
impl vnt_remote_lib::EventSink for AudioEvents {
    fn emit_json(&self, event: &str, payload: String) {
        self.0
            .lock()
            .unwrap()
            .push((event.into(), serde_json::from_str(&payload).unwrap()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_device_failure_keeps_the_controller_video_and_control_alive() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut channel, _listener_task) = accept_managed(listener).await;
        let Some(ControlMessage::Hello(hello)) = channel.recv().await.unwrap() else {
            panic!("expected Hello")
        };
        let (nonce, proof) = vnt_net::auth::challenge(&mut channel).await.unwrap();
        assert!(vnt_proto::auth::verify("pw", &nonce, &proof));
        assert!(hello.audio_enabled);
        channel
            .send(&ControlMessage::Welcome(pb::Welcome {
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
                session_id: 42,
                transport: Transport::Tcp.to_proto() as i32,
                codec: pb::Codec::H264 as i32,
                width: 64,
                height: 48,
                fps: 30,
                audio_enabled: true,
                view_only: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        // Exercise the native controller inbox with 10ms Opus silence, then
        // simulate a failed capture device without closing the desktop stream.
        channel
            .send(&ControlMessage::AudioState(pb::AudioState {
                generation: 1,
                available: true,
                reason: String::new(),
            }))
            .await
            .unwrap();
        for sequence in 0..6 {
            channel
                .send(&ControlMessage::AudioFrame(pb::AudioFrame {
                    generation: 1,
                    sequence,
                    timestamp_samples: sequence as u64 * 480,
                    data: vec![0xf0, 0xff, 0xfe],
                }))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        channel
            .send(&ControlMessage::AudioState(pb::AudioState {
                generation: 1,
                available: false,
                reason: "模拟采集设备不可用".into(),
            }))
            .await
            .unwrap();
        let (mut reader, mut writer) = channel.into_split();
        let mut interval = tokio::time::interval(Duration::from_millis(33));
        let mut pts_ms = 0;
        let mut ping_seen = false;
        loop {
            tokio::select! {
                _=&mut stop_rx=>break,
                _=interval.tick()=>{
                    writer.send(&vnt_proto::screen::wrap(1, 1, ControlMessage::VideoFrame(pb::VideoFrame { dependencies: None,
                        keyframe:true,pts_ms,data:vec![0,0,0,1,0x65],
                    }))).await.unwrap();
                    pts_ms+=33;
                }
                message=reader.recv()=>{
                    if let Some(ControlMessage::Ping(ping))=message.unwrap() {
                        ping_seen=true;writer.send(&ControlMessage::Pong(pb::Pong {timestamp_ms:ping.timestamp_ms})).await.unwrap();
                    }
                }
            }
        }
        assert!(ping_seen, "audio failure blocked reliable control messages");
        writer
            .send(&ControlMessage::SessionEnd(pb::SessionEnd {
                reason: "audio test finished".into(),
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while reader.recv().await.unwrap().is_some() {}
        })
        .await
        .expect("controller tasks did not stop on SessionEnd");
    });
    let events = Arc::new(AudioEvents::default());
    let (client, info) = vnt_remote_lib::client::connect_split(
        events.clone(),
        vnt_remote_lib::client::ConnectParams {
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
            transport: Transport::Tcp,
            host: address.ip().to_string(),
            port: address.port(),
            password: "pw".into(),
            codecs: vec!["h264".into()],
            fps: 30,
            bitrate_kbps: 8000,
            audio_enabled: true,
        },
        vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
    )
    .await
    .unwrap();
    assert!(info.audio_enabled && info.view_only);
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let observed = {
                let events = events.0.lock().unwrap();
                assert!(!events.iter().any(|(event, _)| event == "client://closed"));
                events.iter().any(|(event, payload)| {
                    event == "client://audio" && payload["reason"] == "模拟采集设备不可用"
                })
            };
            if observed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("capture failure was not reported independently of desktop status");
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    client.disconnect();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_switch_matrix_over_tcp_and_hybrid_in_view_only_mode() {
    for server_audio in [false, true] {
        let server = server::start(
            Arc::new(NoopEventSink),
            server::ServerConfig {
                password: "audio-test".into(),
                port: 0,
                monitor_index: 0,
                region: None,
                fps: 30,
                bitrate_kbps: 8000,
                hardware_encode: false,
                audio: server_audio,
                view_only: true,
                synthetic: true,
                clipboard: vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(
                    |_, _| {},
                )),
            },
        )
        .await
        .unwrap();
        for transport in [Transport::Hybrid, Transport::Tcp] {
            for requested in [false, true] {
                let expected = server_audio && requested;
                let address = format!("127.0.0.1:{}", server.port).parse().unwrap();
                let mut tcp = vnt_net::session::connect(address).await.unwrap();
                tcp.send(&ControlMessage::Hello(pb::Hello {
                    hardware_codecs: Vec::new(),
                    color_depth: 24,
                    desktop_effects: Some(Default::default()),
                    capabilities: vnt_proto::platform_capabilities(),
                    fec_version: 0,
                    resolution_mode: 0,
                    target_width: 0,
                    target_height: 0,
                    proto_ver: vnt_proto::PROTO_VER,
                    transport: transport.to_proto() as i32,
                    codecs: vec![pb::Codec::H264 as i32],
                    client: None,
                    fps: 30,
                    bitrate_kbps: 8000,
                    audio_enabled: requested,
                }))
                .await
                .unwrap();
                vnt_net::auth::respond(&mut tcp, "audio-test")
                    .await
                    .unwrap();
                let Some(ControlMessage::Welcome(welcome)) = tcp.recv().await.unwrap() else {
                    panic!("expected Welcome")
                };
                assert!(welcome.view_only);
                assert_eq!(welcome.audio_enabled, expected);
                let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(16);
                let mut udp = if transport == Transport::Hybrid {
                    let mut rx = UdpVideoReceiver::bind(address).await.unwrap();
                    rx.set_session(welcome.session_id);
                    rx.set_audio_sender(audio_tx);
                    rx.send_bind(welcome.session_id, &welcome.bind_token)
                        .await
                        .unwrap();
                    Some(rx)
                } else {
                    None
                };
                tcp.send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 42 }))
                    .await
                    .unwrap();
                let mut audio_count = 0;
                let (mut state_ready, mut video_seen, mut pong_seen) = (false, false, false);
                // Per-screen workers initialize capture/encoding after the
                // handshake; allow the same startup budget with audio off.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
                loop {
                    tokio::select! {
                        _=tokio::time::sleep_until(deadline)=>break,
                        message=tcp.recv()=>match message.unwrap().expect("desktop connection ended") {
                            ControlMessage::AudioFrame(frame)=>{
                                assert!(expected && transport==Transport::Tcp);
                                assert!(vnt_proto::audio::valid_frame(&frame));audio_count+=1;
                            }
                            ControlMessage::AudioState(state)=>{assert!(expected);state_ready|=state.available;}
                            ControlMessage::ScreenPacket(packet)=>{if matches!(vnt_proto::screen::unwrap(&packet),Some(ControlMessage::VideoFrame(_))){video_seen=true;}},
                            ControlMessage::Pong(p)=>{assert_eq!(p.timestamp_ms,42);pong_seen=true;}
                            _=>{}
                        },
                        video=async {udp.as_mut().unwrap().recv_frame().await},if udp.is_some()=>{
                            video_seen|=video.unwrap().is_some();
                        }
                        Some(frame)=audio_rx.recv()=>{
                            assert!(expected && transport==Transport::Hybrid);
                            assert!(vnt_proto::audio::valid_frame(&frame));audio_count+=1;
                        }
                    }
                    if expected && audio_count >= 5 && state_ready && video_seen && pong_seen {
                        break;
                    }
                }
                assert!(
                    video_seen && pong_seen,
                    "audio must not block desktop video or control: {transport:?}, server_audio={server_audio}, requested={requested}, video={video_seen}, pong={pong_seen}"
                );
                if expected {
                    assert!(
                        state_ready && audio_count >= 5,
                        "no Opus stream for {transport:?}"
                    );
                } else {
                    assert_eq!(audio_count, 0);
                    assert!(!state_ready);
                }
                tcp.send(&ControlMessage::SessionEnd(pb::SessionEnd {
                    reason: "audio case complete".into(),
                }))
                .await
                .unwrap();
                drop(tcp);
                drop(udp);
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !server.active_sessions().is_empty() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
        }
        server.stop().await;
    }
}

async fn accept_managed(
    listener: tokio::net::TcpListener,
) -> (
    vnt_net::session::SessionChannel,
    tokio_util::task::AbortOnDropHandle<()>,
) {
    let router = vnt_net::session::SessionListener::default();
    let (stream, peer) = listener.accept().await.unwrap();
    let channel = router.accept(stream, peer).await.unwrap().unwrap();
    let task = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                next=listener.accept()=>{let (stream,peer)=next.unwrap();let router=router.clone();tasks.spawn(async move {let _=router.accept(stream,peer).await;});},
                Some(_)=tasks.join_next(), if !tasks.is_empty()=>{},
            }
        }
    });
    (channel, tokio_util::task::AbortOnDropHandle::new(task))
}
