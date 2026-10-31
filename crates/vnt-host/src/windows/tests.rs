use super::{
    management::{read_json, write_json},
    native,
};
use vnt_net::tcp::ControlChannel;
use vnt_proto::{control::ControlMessage, pb};
#[tokio::test]
async fn named_pipe_authentication_uses_kernel_process_ids_and_bounded_frames() {
    let name = format!(r"\\.\pipe\VntRemote.Test.{}", native::random().unwrap());
    let server = native::pipe(&name, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;BU)", true).unwrap();
    assert!(native::pipe(&name, "D:P(A;;GA;;;SY)", true).is_err());
    let client = tokio::spawn(async move {
        let client = native::connect(&name).await.unwrap();
        assert_eq!(native::server_pid(&client).unwrap(), std::process::id());
        let mut ch = ControlChannel::new(client);
        ch.send(&ControlMessage::SendSas).await.unwrap();
        matches!(ch.recv().await.unwrap(),Some(ControlMessage::DesktopState(s)) if !s.ready && s.generation==7)
    });
    server.connect().await.unwrap();
    assert_eq!(native::client_pid(&server).unwrap(), std::process::id());
    let mut ch = ControlChannel::new(server);
    assert!(matches!(
        ch.recv().await.unwrap(),
        Some(ControlMessage::SendSas)
    ));
    ch.send(&ControlMessage::DesktopState(pb::DesktopState {
        min_pts_ms: 0,
        ready: false,
        desktop: "Winlogon".into(),
        reason: "切换中".into(),
        generation: 7,
    }))
    .await
    .unwrap();
    assert!(client.await.unwrap());
}
#[tokio::test]
async fn bootstrap_rejects_oversized_payload_before_allocation() {
    use tokio::io::AsyncWriteExt;
    let (mut a, mut b) = tokio::io::duplex(16);
    a.write_u32_le(65537).await.unwrap();
    assert!(read_json::<_, serde_json::Value>(&mut b).await.is_err());
    assert!(write_json(&mut a, &"x".repeat(65536)).await.is_err());
}
#[test]
fn native_system_paths_do_not_use_the_callers_search_path() {
    let path = native::system_executable("sc.exe").unwrap();
    assert!(path.is_absolute());
    assert!(path.exists());
    assert_eq!(path.file_name().unwrap(), "sc.exe");
}

#[tokio::test]
async fn shared_agent_streams_two_pipe_sessions_and_cancels_only_the_departing_session() {
    use std::{sync::Arc, time::Duration};
    let clipboard = crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
    let host = Arc::new(crate::server::AgentHost::new(crate::server::ServerConfig {
        password: "agent-test".into(),
        port: 0,
        monitor_index: 0,
        region: None,
        fps: 30,
        bitrate_kbps: 8000,
        hardware_encode: false,
        audio: false,
        view_only: true,
        clipboard,
        synthetic: true,
    }));
    let mut clients = Vec::new();
    let mut tasks = Vec::new();
    for depth in [15, 32] {
        let (a, b) = tokio::io::duplex(1024 * 1024);
        let host = host.clone();
        tasks.push(tokio::spawn(async move { host.serve(a).await }));
        let mut client = ControlChannel::new(b);
        client
            .send(&ControlMessage::Hello(pb::Hello {
                desktop_effects: Some(Default::default()),
                color_depth: depth,
                capabilities: vnt_proto::platform_capabilities(),
                proto_ver: vnt_proto::PROTO_VER,
                codecs: vec![pb::Codec::H264 as i32],
                fps: 30,
                bitrate_kbps: 8000,
                transport: vnt_proto::Transport::Tcp.to_proto() as i32,
                ..Default::default()
            }))
            .await
            .unwrap();
        vnt_net::auth::respond(&mut client, "agent-test")
            .await
            .unwrap();
        assert!(
            matches!(client.recv().await.unwrap(),Some(ControlMessage::Welcome(w)) if w.view_only && w.color_depth == depth)
        );
        clients.push(client);
    }
    let closed = clients.remove(0);
    drop(closed);
    tokio::time::timeout(Duration::from_secs(3), tasks.remove(0))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut remaining = clients.remove(0);
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ControlMessage::VideoFrame(v)) = remaining.recv().await.unwrap() {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(!frame.data.is_empty());
    drop(remaining);
    tokio::time::timeout(Duration::from_secs(3), tasks.remove(0))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// Runs on the same single-thread runtime as --user-agent, with actual framed
// named-pipe traffic. Publishers are isolated so no user's clipboard is changed.
#[tokio::test]
async fn user_agent_pipe_syncs_text_and_lazy_files_in_both_directions() {
    use crate::clipboard::{dispatch_inbound, ClipboardService, SessionSpec};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use vnt_clip::native::FileProvider;
    type Published = Arc<Mutex<Option<Arc<dyn FileProvider>>>>;
    fn peer() -> (Arc<ClipboardService>, Published, Arc<Mutex<Vec<String>>>) {
        let published: Published = Default::default();
        let output = published.clone();
        let text: Arc<Mutex<Vec<String>>> = Default::default();
        let writes = text.clone();
        let service = ClipboardService::with_publishers(
            Arc::new(move |content, _| match content {
                vnt_clip::ClipboardContent::Text(text) => writes.lock().unwrap().push(text),
                vnt_clip::ClipboardContent::Image(image) => writes
                    .lock()
                    .unwrap()
                    .push(format!("image:{}x{}", image.width, image.height)),
                _ => {}
            }),
            Arc::new(move |_, provider, _| {
                *output.lock().unwrap() = Some(provider);
                Ok(())
            }),
        );
        (service, published, text)
    }
    async fn provider(published: &Published) -> Arc<dyn FileProvider> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(provider) = published.lock().unwrap().clone() {
                    return provider;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    let (controller, received_by_controller, controller_text) = peer();
    let (target, received_by_target, target_text) = peer();
    let name = format!(
        r"\\.\pipe\VntRemote.ClipboardTest.{}",
        native::random().unwrap()
    );
    let pipe = native::pipe(&name, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;BU)", true).unwrap();
    let stop = CancellationToken::new();
    let agent_stop = stop.clone();
    let agent_clipboard = target.clone();
    let agent = tokio::spawn(async move {
        let client = native::connect(&name).await.unwrap();
        super::agent::user_session(
            client,
            super::agent::Bootstrap {
                secret: "pipe-secret".into(),
                config: crate::config::AppConfig {
                    audio: false,
                    ..Default::default()
                },
            },
            agent_stop,
            agent_clipboard,
            vnt_audio::SharedAudio::new(false),
        )
        .await
    });
    pipe.connect().await.unwrap();
    let mut channel = ControlChannel::new(pipe);
    channel
        .send(&ControlMessage::Hello(pb::Hello {
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut channel, "pipe-secret")
        .await
        .unwrap();
    assert!(matches!(
        channel.recv().await.unwrap(),
        Some(ControlMessage::Welcome(_))
    ));
    let (out, mut outgoing) = tokio::sync::mpsc::channel(64);
    let (input, receive) = tokio::sync::mpsc::channel(crate::clipboard::INBOUND_QUEUE);
    let registration = controller.register(SessionSpec {
        capabilities: vnt_proto::platform_capabilities(),
        session_id: 42,
        file_tx: out.clone(),
        control_tx: out,
        in_rx: receive,
        session_cancel: stop.clone(),
        sink: Arc::new(crate::NoopEventSink),
        enabled: true,
    });
    let key = registration.key();
    let source = controller.clone();
    let (mut reader, mut writer) = channel.into_split();
    let reading = tokio::spawn(async move {
        while let Some(message) = reader.recv().await.unwrap() {
            dispatch_inbound(&source, key, &input, message)
                .await
                .unwrap();
        }
    });
    let writing = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            writer.send(&message).await.unwrap();
        }
    });
    controller.on_local_change(vnt_clip::ClipboardContent::Text("to target".into()));
    target.on_local_change(vnt_clip::ClipboardContent::Text("to controller".into()));
    tokio::time::timeout(Duration::from_secs(3), async {
        while target_text.lock().unwrap().is_empty() || controller_text.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*target_text.lock().unwrap(), ["to target"]);
    assert_eq!(*controller_text.lock().unwrap(), ["to controller"]);
    let image = vnt_clip::image::from_rgba(2, 1, &[255, 0, 0, 255, 0, 128, 255, 64]).unwrap();
    for (source, received) in [(&controller, &target_text), (&target, &controller_text)] {
        source.on_local_change(vnt_clip::ClipboardContent::Image(image.clone()));
        tokio::time::timeout(Duration::from_secs(3), async {
            while received.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(received.lock().unwrap()[1], "image:2x1");
    }
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file.bin");
    let bytes = b"service clipboard payload".repeat(4096);
    std::fs::write(&file, &bytes).unwrap();
    for (source, received) in [
        (&controller, &received_by_target),
        (&target, &received_by_controller),
    ] {
        source.offer_paths(std::slice::from_ref(&file)).unwrap();
        let provider = provider(received).await;
        assert!(provider.available());
        let operation = provider.begin().unwrap();
        let expected_len = bytes.len();
        let actual = tokio::task::spawn_blocking(move || {
            let mut all = Vec::new();
            while all.len() < expected_len {
                let chunk = operation.read(0, all.len() as u64, 32768).unwrap();
                all.extend(chunk);
            }
            operation.finish(true, "test paste complete");
            all
        })
        .await
        .unwrap();
        assert!(actual == bytes, "pasted bytes must match source exactly");
    }
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(3), agent)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    reading.abort();
    writing.abort();
    drop(registration);
}

#[tokio::test]
async fn persistent_media_pipe_recovers_epochs_without_rehandshake() {
    use pb::agent_message::Payload;
    use std::{sync::Arc, time::Duration};
    let d = vnt_capture::DesktopCapture::new(0, Some("Default".into()));
    let cfg = crate::server::ServerConfig {
        password: "persistent-test".into(),
        port: 0,
        monitor_index: 0,
        region: None,
        fps: 30,
        bitrate_kbps: 2000,
        hardware_encode: false,
        audio: false,
        view_only: true,
        clipboard: crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        synthetic: true,
    };
    let host = crate::server::AgentHost::with_desktop(cfg, d.clone());
    let name = format!(
        r"\\.\pipe\VntRemote.Test.Persistent.{}",
        native::random().unwrap()
    );
    let a = native::pipe(&name, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;BU)", true).unwrap();
    let b = native::connect(&name).await.unwrap();
    a.connect().await.unwrap();
    assert_eq!(native::server_pid(&b).unwrap(), std::process::id());
    let task = tokio::spawn(async move { host.serve(a).await });
    let mut pipe = ControlChannel::new(b);
    pipe.send(&ControlMessage::Hello(pb::Hello {
        desktop_effects: Some(Default::default()),
        color_depth: 16,
        capabilities: vnt_proto::platform_capabilities(),
        proto_ver: vnt_proto::PROTO_VER,
        codecs: vec![pb::Codec::H264 as i32],
        fps: 30,
        bitrate_kbps: 2000,
        transport: vnt_proto::Transport::Tcp.to_proto() as i32,
        ..Default::default()
    }))
    .await
    .unwrap();
    vnt_net::auth::respond(&mut pipe, "persistent-test")
        .await
        .unwrap();
    assert!(matches!(
        pipe.recv().await.unwrap(),
        Some(ControlMessage::Welcome(w)) if w.color_depth == 16
    ));
    let mut last_pts = 0;
    for name in ["Default", "Winlogon", "Default", "Winlogon", "Default"] {
        d.set_desktop(Some(name.into()));
        let expected = d.snapshot().generation;
        tokio::time::timeout(Duration::from_secs(8), async {
            let mut configured = false;
            loop {
                let message = pipe
                    .recv()
                    .await
                    .unwrap()
                    .expect("pipe must survive switch");
                assert!(!matches!(
                    message,
                    ControlMessage::Welcome(_) | ControlMessage::SessionEnd(_)
                ));
                if let ControlMessage::Agent(message) = message {
                    if message.generation < expected {
                        continue;
                    }
                    assert_eq!(message.generation, d.snapshot().generation);
                    match message.payload.unwrap() {
                        Payload::Config(_) => configured = true,
                        Payload::Video(frame) => {
                            assert!(configured);
                            assert!(frame.keyframe);
                            assert!(frame.pts_ms >= last_pts);
                            last_pts = frame.pts_ms;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        })
        .await
        .unwrap();
        d.set_desktop(None);
        pipe.send(&ControlMessage::Ping(pb::Ping { timestamp_ms: 123 }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2),async{while !matches!(pipe.recv().await.unwrap(),Some(ControlMessage::Pong(p)) if p.timestamp_ms==123){}}).await.unwrap();
    }
    drop(pipe);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_agent_pipe_applies_background_budget_and_restores_encoding() {
    use std::{sync::Arc, time::Duration};
    let host = Arc::new(crate::server::AgentHost::new(crate::server::ServerConfig {
        password: "throttle-agent-test".into(),
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
    }));
    let name = format!(
        r"\\.\pipe\VntRemote.ThrottleTest.{}",
        native::random().unwrap()
    );
    let pipe = native::pipe(&name, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;BU)", true).unwrap();
    let connecting = tokio::spawn(async move { native::connect(&name).await.unwrap() });
    pipe.connect().await.unwrap();
    let mut client = ControlChannel::new(connecting.await.unwrap());
    let serving = tokio::spawn(async move { host.serve(pipe).await });
    client
        .send(&ControlMessage::Hello(pb::Hello {
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            codecs: vec![pb::Codec::H264 as i32],
            fps: 30,
            bitrate_kbps: 8000,
            transport: vnt_proto::Transport::Tcp.to_proto() as i32,
            ..Default::default()
        }))
        .await
        .unwrap();
    vnt_net::auth::respond(&mut client, "throttle-agent-test")
        .await
        .unwrap();
    assert!(matches!(
        client.recv().await.unwrap(),
        Some(ControlMessage::Welcome(_))
    ));
    for (background, fps, kbps) in [(true, 5, 1333), (false, 30, 8000)] {
        client
            .send(&ControlMessage::ScreenThrottle(pb::ScreenThrottle {
                background,
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match client.recv().await.unwrap().unwrap() {
                    ControlMessage::StatsReport(stats) if stats.applied_fps == fps => {
                        assert_eq!(stats.applied_bitrate_kbps, kbps);
                        if !background || stats.fps <= 7 {
                            break;
                        }
                    }
                    ControlMessage::SessionEnd(end) => panic!("{}", end.reason),
                    _ => {}
                }
            }
        })
        .await
        .expect("named-pipe agent must apply visibility changes and keep sending video");
    }
    client
        .send(&ControlMessage::SessionEnd(pb::SessionEnd {
            reason: "done".into(),
        }))
        .await
        .unwrap();
    drop(client);
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
