//! Desktop controller adapters. Protocol/session logic is shared with Android.
use crate::EventSink;
use std::sync::Arc;
use tokio::sync::mpsc;
pub(crate) use vnt_client::screens;
pub use vnt_client::{
    send_input, send_input_events, send_screen_input_events, validate_fec, validate_welcome,
    ClientSessionInfo, ConnectParams, InputArg, ScreenInputTarget,
};
use vnt_proto::{control::ControlMessage, pb};
pub struct ClientHandle(vnt_client::ClientHandle);
impl std::ops::Deref for ClientHandle {
    type Target = vnt_client::ClientHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl ClientHandle {
    #[cfg(windows)]
    pub fn capture_keyboard(
        &self,
        hwnd: windows::Win32::Foundation::HWND,
        area: crate::commands::KeyboardCaptureArea,
        target: Option<ScreenInputTarget>,
    ) -> Result<crate::keyboard::KeyboardCapture, String> {
        let fail = self.0.failure_callback();
        crate::keyboard::KeyboardCapture::start(
            hwnd,
            area,
            self.control_tx.clone(),
            self.control_flag(),
            self.screens.clone(),
            target,
            move || {
                fail();
            },
        )
    }

    #[cfg(target_os = "macos")]
    pub fn capture_keyboard(
        &self,
        hwnd: usize,
        area: crate::commands::KeyboardCaptureArea,
        target: Option<ScreenInputTarget>,
    ) -> Result<crate::keyboard::KeyboardCapture, String> {
        let fail = self.0.failure_callback();
        crate::keyboard::KeyboardCapture::start(
            hwnd,
            area,
            self.control_tx.clone(),
            self.control_flag(),
            self.screens.clone(),
            target,
            move || {
                fail();
            },
        )
    }

    pub fn disconnect(self) {
        self.0.disconnect();
    }
}
pub async fn connect_split(
    sink: Arc<dyn EventSink>,
    params: ConnectParams,
    clipboard: Arc<crate::clipboard::ClipboardService>,
) -> Result<(ClientHandle, ClientSessionInfo), String> {
    connect_prepared(sink, params, clipboard, None).await
}
pub async fn connect_prepared(
    sink: Arc<dyn EventSink>,
    params: ConnectParams,
    clipboard: Arc<crate::clipboard::ClipboardService>,
    prepared: Option<vnt_net::overlay::Prepared>,
) -> Result<(ClientHandle, ClientSessionInfo), String> {
    let outputs = vnt_client::platform::Outputs {
        client_name: "vnt-remote webview".into(),
        media: None,
        audio: Arc::new(DesktopAudio),
    };
    let (handle, info) = vnt_client::connect_with(
        sink,
        params,
        Arc::new(DesktopClipboard(clipboard)),
        outputs,
        prepared,
    )
    .await?;
    Ok((ClientHandle(handle), info))
}
struct DesktopClipboard(Arc<crate::clipboard::ClipboardService>);
struct Registration(crate::clipboard::ClipboardRegistration);
impl vnt_client::clipboard::ClipboardRegistration for Registration {
    fn key(&self) -> u64 {
        self.0.key()
    }
}
impl vnt_client::clipboard::ClipboardService for DesktopClipboard {
    fn capabilities(&self) -> u32 {
        vnt_proto::platform_capabilities()
    }
    fn is_enabled(&self) -> bool {
        self.0.is_enabled()
    }
    fn session_enabled(&self, key: u64) -> bool {
        self.0.session_enabled(key)
    }
    fn set_session_enabled(&self, key: u64, enabled: bool) -> bool {
        self.0.set_session_enabled(key, enabled)
    }
    fn transport_changed(&self, key: u64, available: bool) {
        self.0.transport_changed(key, available)
    }
    fn control_changed(&self, key: u64, available: bool) {
        self.0.control_changed(key, available)
    }
    fn register(
        self: Arc<Self>,
        spec: vnt_client::clipboard::SessionSpec,
    ) -> Box<dyn vnt_client::clipboard::ClipboardRegistration> {
        Box::new(Registration(self.0.register(
            crate::clipboard::SessionSpec {
                capabilities: spec.capabilities,
                session_id: spec.session_id,
                file_tx: spec.file_tx,
                control_tx: spec.control_tx,
                in_rx: spec.in_rx,
                session_cancel: spec.session_cancel,
                sink: spec.sink,
                enabled: spec.enabled,
            },
        )))
    }
    fn dispatch<'a>(
        &'a self,
        key: u64,
        tx: mpsc::Sender<ControlMessage>,
        msg: ControlMessage,
    ) -> vnt_client::clipboard::Dispatch<'a> {
        Box::pin(async move { crate::clipboard::dispatch_inbound(&self.0, key, &tx, msg).await })
    }
}
struct DesktopAudio;
struct Playback(vnt_audio::Playback);
struct Sender(vnt_audio::PlaybackSender);
impl vnt_client::platform::AudioFactory for DesktopAudio {
    fn start(
        &self,
        device: Option<String>,
    ) -> Result<Box<dyn vnt_client::platform::AudioPlayback>, String> {
        Ok(Box::new(Playback(vnt_audio::Playback::start_on_device(
            device,
        )?)))
    }
}
impl vnt_client::platform::AudioPlayback for Playback {
    fn sender(&self) -> Arc<dyn vnt_client::platform::AudioSender> {
        Arc::new(Sender(self.0.sender()))
    }
}
impl vnt_client::platform::AudioSender for Sender {
    fn set_state(&self, state: pb::AudioState) {
        self.0.set_state(state)
    }
    fn send(&self, frame: pb::AudioFrame) {
        self.0.send(frame)
    }
    fn status(&self) -> vnt_client::platform::PlaybackStatus {
        let s = self.0.status();
        vnt_client::platform::PlaybackStatus {
            available: s.available,
            reason: s.reason,
            buffer_ms: s.buffer_ms,
        }
    }
}

#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
mod tests {
    use super::*;
    use vnt_client::address;

    #[test]
    fn fec_confirmation_must_match_the_request_and_transport() {
        for transport in [
            vnt_proto::Transport::Tcp,
            vnt_proto::Transport::Hybrid,
            vnt_proto::Transport::Quic,
            vnt_proto::Transport::Vnt,
        ] {
            for enabled in [false, true] {
                let expected = u32::from(enabled && transport != vnt_proto::Transport::Tcp);
                for confirmed in [0, 1, 2] {
                    assert_eq!(
                        validate_fec(enabled, transport, confirmed).is_ok(),
                        confirmed == expected
                    );
                }
            }
        }
    }

    #[test]
    fn local_audio_device_is_optional_and_cannot_contain_a_truncated_windows_id() {
        let value = serde_json::json!({"colorDepth":24,"showWallpaper":true,"dragFullWindows":false,"fecEnabled":false,"hardwareCodecs":[],
            "transport":"tcp", "host":"127.0.0.1", "port":29875,
            "password":"pw", "codecs":["h264"], "fps":30, "bitrateKbps":8000,
            "clipboardEnabled":true, "audioEnabled":true, "audioDeviceId":null,
            "resolutionMode":"none", "targetWidth":0, "targetHeight":0,
        });
        for field in [
            "clipboardEnabled",
            "audioEnabled",
            "colorDepth",
            "showWallpaper",
            "dragFullWindows",
            "fecEnabled",
            "hardwareCodecs",
        ] {
            let mut incomplete = value.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<ConnectParams>(incomplete).is_err(),
                "{field}"
            );
        }
        let mut params: ConnectParams = serde_json::from_value(value).unwrap();
        assert!(params.audio_enabled && params.audio_device_id.is_none());
        assert!(!params.fec_enabled);
        assert!(params.show_wallpaper);
        assert!(!params.drag_full_windows);
        assert_eq!(params.color_depth, 24);
        for depth in [15, 16, 24, 32] {
            params.color_depth = depth;
            assert!(params.validate().is_ok());
        }
        for depth in [0, 8, 30, u32::MAX] {
            params.color_depth = depth;
            assert!(params.validate().is_err());
        }
        params.color_depth = 24;
        for host in ["192.168.1.10", "example.com", "localhost", "::1"] {
            params.host = host.into();
            assert!(params.validate().is_ok(), "{host}");
        }
        for host in [
            "192.168.1.10:",
            "example.com:",
            "256.1.1.1",
            "https://example.com",
        ] {
            params.host = host.into();
            assert_eq!(params.validate().unwrap_err(), address::ADDRESS_ERROR);
        }
        params.host = "127.0.0.1".into();
        params.port = 0;
        assert_eq!(params.validate().unwrap_err(), address::ADDRESS_ERROR);
        params.port = vnt_proto::DEFAULT_PORT;
        params.audio_device_id = Some("headset-id".into());
        assert!(params.validate().is_ok());
        params.audio_device_id = Some("headset\0other-device".into());
        assert!(params.validate().is_err());
        params.audio_device_id = Some(String::new());
        assert!(params.validate().is_err());
        params.audio_device_id = Some("a".repeat(2049));
        assert!(params.validate().is_err());
    }

    #[tokio::test]
    async fn color_depth_requires_exact_confirmation() {
        for (requested, applied, accepted) in [
            (15, 0, false),
            (16, 0, false),
            (24, 0, false),
            (32, 0, false),
            (15, 15, true),
            (16, 16, true),
            (32, 32, true),
            (15, 16, false),
            (24, 8, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (done, finish) = tokio::sync::oneshot::channel();
            let serving = tokio::spawn(async move {
                let (mut channel, _listener_task) = accept_managed(listener).await;
                let Some(ControlMessage::Hello(hello)) = channel.recv().await.unwrap() else {
                    panic!("missing Hello")
                };
                assert_eq!(hello.color_depth, requested);
                let (nonce, proof) = vnt_net::auth::challenge(&mut channel).await.unwrap();
                assert!(vnt_proto::auth::verify("test", &nonce, &proof));
                channel
                    .send(&ControlMessage::Welcome(pb::Welcome {
                        system: vnt_proto::system::local() as i32,
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
                        color_depth: applied,
                        host_edition: pb::HostEdition::Portable as i32,
                        transport: vnt_proto::Transport::Tcp.to_proto() as i32,
                        proto_ver: vnt_proto::PROTO_VER,
                        session_id: 42,
                        codec: pb::Codec::H264 as i32,
                        width: 64,
                        height: 48,
                        fps: 30,
                        ..Default::default()
                    }))
                    .await
                    .unwrap();
                let _ = finish.await;
            });
            let params: ConnectParams = serde_json::from_value(serde_json::json!({"showWallpaper":true,"dragFullWindows":false,"fecEnabled":false,"hardwareCodecs":[],
                "colorDepth": requested,
                "transport":"tcp", "host":"127.0.0.1", "port":address.port(),
                "password":"test", "codecs":["h264"], "fps":30, "bitrateKbps":8000,
                "clipboardEnabled":true, "audioEnabled":false, "audioDeviceId":null,
                "resolutionMode":"none", "targetWidth":0, "targetHeight":0,
            }))
            .unwrap();
            let result = connect_split(
                Arc::new(crate::NoopEventSink),
                params,
                crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
            )
            .await;
            match result {
                Ok((client, _)) => {
                    assert!(accepted);
                    client.disconnect();
                }
                Err(error) => {
                    assert!(!accepted, "{requested}/{applied}: {error}");
                    assert!(error.contains("颜色深度"));
                }
            }
            let _ = done.send(());
            serving.await.unwrap();
        }
    }

    #[tokio::test]
    async fn feedback_keeps_reporting_without_udp_video() {
        check_feedback_without_video(
            vnt_proto::Transport::Hybrid,
            pb::HostEdition::Portable,
            false,
        )
        .await;
    }
    #[tokio::test]
    async fn feedback_keeps_reporting_without_tcp_video() {
        check_feedback_without_video(vnt_proto::Transport::Tcp, pb::HostEdition::Portable, false)
            .await;
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
    #[tokio::test]
    async fn sas_is_allowed_only_for_installed_controllable_hosts() {
        check_feedback_without_video(vnt_proto::Transport::Tcp, pb::HostEdition::Installed, false)
            .await;
        check_feedback_without_video(vnt_proto::Transport::Tcp, pb::HostEdition::Installed, true)
            .await;
        check_feedback_without_video(vnt_proto::Transport::Tcp, pb::HostEdition::Portable, true)
            .await;
    }
    async fn check_feedback_without_video(
        transport: vnt_proto::Transport,
        edition: pb::HostEdition,
        view_only: bool,
    ) {
        let sas_allowed = edition == pb::HostEdition::Installed && !view_only;
        // Windows reserves different ephemeral ranges for TCP and UDP.
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
        let (listener, _udp) = bound.expect("available TCP/UDP test port");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut channel, _listener_task) = accept_managed(listener).await;
            let Some(ControlMessage::Hello(hello)) = channel.recv().await.unwrap() else {
                panic!()
            };
            let (nonce, proof) = vnt_net::auth::challenge(&mut channel).await.unwrap();
            assert!(vnt_proto::auth::verify("test", &nonce, &proof));
            assert_eq!(hello.proto_ver, vnt_proto::PROTO_VER);
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
                    host_edition: edition as i32,
                    transport: transport.to_proto() as i32,
                    proto_ver: vnt_proto::PROTO_VER,
                    session_id: 42,
                    bind_token: if transport != vnt_proto::Transport::Tcp {
                        vec![7; 16]
                    } else {
                        vec![]
                    },
                    codec: pb::Codec::H264.into(),
                    width: 64,
                    height: 48,
                    fps: 30,
                    view_only,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let deadline = tokio::time::sleep(Duration::from_millis(2300));
            tokio::pin!(deadline);
            let mut reports = 0;
            let mut sas_received = false;
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    message = channel.recv() => match message.unwrap().unwrap() {
                        ControlMessage::Ping(ping) => channel.send(&ControlMessage::Pong(pb::Pong { timestamp_ms: ping.timestamp_ms })).await.unwrap(),
                        ControlMessage::LinkFeedback(report) => {
                            assert!(vnt_proto::validation::valid_link_feedback(&report));
                            assert_eq!(report.received_frames, 0);
                            assert_eq!(report.lost_frames, 0);
                            assert_eq!(report.frame_gap_ms, None);
                            assert!(report.rtt_ms.is_some());
                            reports += 1;
                        }
                        ControlMessage::SendSas => sas_received = true,
                        _ => {}
                    }
                }
            }
            assert!(reports > 0);
            assert_eq!(sas_received, sas_allowed);
            channel
                .send(&ControlMessage::SessionEnd(pb::SessionEnd {
                    reason: "done".into(),
                }))
                .await
                .unwrap();
        });
        let (client, info) = connect_split(
            Arc::new(crate::NoopEventSink),
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
                port: address.port(),
                password: "test".into(),
                codecs: vec!["h264".into()],
                fps: 30,
                bitrate_kbps: 8000,
            },
            crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        )
        .await
        .unwrap();
        assert_eq!(info.fps, 30);
        assert_eq!(
            info.host_edition,
            if edition == pb::HostEdition::Installed {
                "installed"
            } else {
                "portable"
            }
        );
        assert_eq!(client.send_sas().is_ok(), sas_allowed);
        assert!(!client.is_closed());
        server.await.unwrap();
        client.disconnect();
    }

    #[test]
    fn welcome_rejects_invalid_metadata_before_allocating_video_resources() {
        let valid = pb::Welcome {
            system: vnt_proto::system::local() as i32,
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
            color_depth: 24,
            host_edition: pb::HostEdition::Portable as i32,
            transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
            session_id: 1,
            bind_token: vec![1; 16],
            codec: pb::Codec::H264.into(),
            width: 1920,
            height: 1080,
            fps: 60,
            ..Default::default()
        };
        let codecs = vec!["h264".into()];
        for system in [0, -1, 999] {
            let invalid = pb::Welcome { system, ..valid.clone() };
            assert!(validate_welcome(&invalid, &codecs, vnt_proto::Transport::Hybrid).is_err());
        }
        assert!(validate_welcome(&valid, &codecs, vnt_proto::Transport::Hybrid).is_ok());
        let tcp = pb::Welcome {
            system: vnt_proto::system::local() as i32,
            host_edition: pb::HostEdition::Portable as i32,
            transport: vnt_proto::Transport::Tcp.to_proto() as i32,
            bind_token: vec![],
            ..valid.clone()
        };
        assert!(validate_welcome(&tcp, &codecs, vnt_proto::Transport::Tcp).is_ok());
        assert!(validate_welcome(&tcp, &codecs, vnt_proto::Transport::Hybrid).is_err());
        assert!(validate_welcome(&valid, &codecs, vnt_proto::Transport::Tcp).is_err());
        for host_edition in [0, -1, 3] {
            assert!(validate_welcome(
                &pb::Welcome {
                    system: vnt_proto::system::local() as i32,
                    host_edition,
                    ..valid.clone()
                },
                &codecs,
                vnt_proto::Transport::Hybrid
            )
            .is_err());
        }
        for wire_mode in [0, -1, 3] {
            assert!(validate_welcome(
                &pb::Welcome {
                    system: vnt_proto::system::local() as i32,
                    host_edition: pb::HostEdition::Portable as i32,
                    transport: wire_mode,
                    ..valid.clone()
                },
                &codecs,
                vnt_proto::Transport::Tcp
            )
            .is_err());
        }
        for invalid in [
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                screens: vec![],
                ..valid.clone()
            },
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                host_edition: pb::HostEdition::Portable as i32,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                session_id: 0,
                ..valid.clone()
            },
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                host_edition: pb::HostEdition::Portable as i32,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                bind_token: vec![],
                ..valid.clone()
            },
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                host_edition: pb::HostEdition::Portable as i32,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                width: u32::MAX,
                ..valid.clone()
            },
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                host_edition: pb::HostEdition::Portable as i32,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                height: 0,
                ..valid.clone()
            },
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                host_edition: pb::HostEdition::Portable as i32,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                fps: 0,
                ..valid.clone()
            },
            pb::Welcome {
                system: vnt_proto::system::local() as i32,
                host_edition: pb::HostEdition::Portable as i32,
                transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                codec: pb::Codec::Hevc.into(),
                ..valid.clone()
            },
        ] {
            assert!(validate_welcome(&invalid, &codecs, vnt_proto::Transport::Hybrid).is_err());
        }
    }

    #[tokio::test]
    async fn oversized_batch_is_rejected_before_sending_any_key_down() {
        let (tx, mut rx) = mpsc::channel(512);
        let mut events = vec![
            InputArg::Key {
                scancode: 30,
                extended: false,
                down: true
            };
            256
        ];
        events.push(InputArg::Key {
            scancode: 30,
            extended: false,
            down: false,
        });
        assert!(send_input_events(&tx, events).await.is_err());
        assert!(rx.try_recv().is_err());
    }

    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<String>>);

    impl EventSink for RecordingSink {
        fn emit_json(&self, event: &str, payload: String) {
            if event == "client://closed" {
                self.0.lock().unwrap().push(payload);
            }
        }
    }

    #[tokio::test]
    async fn session_end_releases_control_socket_and_bridge_once() {
        check_session_end("127.0.0.1").await;
    }

    #[tokio::test]
    async fn hostname_connection_completes_handshake_and_releases_session() {
        check_session_end("localhost").await;
    }

    async fn check_session_end(host: &str) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut channel, _listener_task) = accept_managed(listener).await;
            let Some(ControlMessage::Hello(hello)) = channel.recv().await.unwrap() else {
                panic!("expected Hello");
            };
            let (nonce, proof) = vnt_net::auth::challenge(&mut channel).await.unwrap();
            assert!(vnt_proto::auth::verify("pw", &nonce, &proof));
            assert_eq!(hello.fps, 30);
            assert_eq!(hello.bitrate_kbps, 8000);
            assert_eq!(hello.codecs, vec![i32::from(pb::Codec::H264)]);
            assert_eq!(hello.hardware_codecs, vec![i32::from(pb::Codec::H264)]);
            channel
                .send(&ControlMessage::Welcome(pb::Welcome {
                    system: vnt_proto::system::local() as i32,
                    color_depth: 24,
                    capabilities: vnt_proto::platform_capabilities(),
                    fec_version: 0,
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
                    resolution_mode: 0,
                    resolution_reason: String::new(),
                    session_token: Vec::new(),
                    audio_enabled: false,
                    transport: vnt_proto::Transport::Hybrid.to_proto() as i32,
                    proto_ver: vnt_proto::PROTO_VER,
                    session_id: 42,
                    bind_token: vec![7; 16],
                    codec: pb::Codec::H264.into(),
                    width: 64,
                    height: 48,
                    fps: 30,
                    hardware: false,
                    decoder_config: vec![],
                    encoder_name: "test".into(),
                    view_only: false,
                }))
                .await
                .unwrap();
            end_rx.await.unwrap();
            channel
                .send(&ControlMessage::SessionEnd(pb::SessionEnd {
                    reason: "server stopped".into(),
                }))
                .await
                .unwrap();
            // Ignore a Ping already queued before SessionEnd, then require EOF.
            tokio::time::timeout(Duration::from_secs(2), async {
                while channel.recv().await.unwrap().is_some() {}
            })
            .await
            .expect("TCP writer remained alive after SessionEnd");
        });
        let sink = Arc::new(RecordingSink::default());
        let (client, info) = connect_split(
            sink.clone(),
            ConnectParams {
                clipboard_enabled: true,
                server_config_id: None,
                server_node_id: None,
                server_network_code: None,
                hardware_codecs: vec!["h264".into(), "av1".into(), "invalid".into()],
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
                host: host.into(),
                port: addr.port(),
                password: "pw".into(),
                codecs: vec!["h264".into()],
                fps: 30,
                bitrate_kbps: 8000,
            },
            crate::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {})),
        )
        .await
        .unwrap();
        assert!(!info.fec_enabled);

        let port = info.ws_port;
        end_tx.send(()).unwrap();
        server.await.unwrap();
        client.disconnect();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bridge port remained open");
        let closed = sink.0.lock().unwrap();
        assert_eq!(closed.len(), 1);
        assert!(closed[0].contains("server stopped"));
    }

    #[tokio::test(start_paused = true)]
    async fn input_congestion_reports_reliable_failure() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send(ControlMessage::RequestKeyframe).await.unwrap();
        assert!(send_input(&tx, InputArg::MouseMove { x: 0.5, y: 0.5 })
            .await
            .is_err());
        assert!(send_input(
            &tx,
            InputArg::Key {
                scancode: 30,
                extended: false,
                down: false
            }
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn congested_move_arrives_before_the_following_click() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(ControlMessage::RequestKeyframe).await.unwrap();
        let send = async {
            send_input(&tx, InputArg::MouseMove { x: 0.75, y: 0.25 })
                .await
                .unwrap();
            send_input(
                &tx,
                InputArg::MouseButton {
                    button: 1,
                    down: true,
                },
            )
            .await
            .unwrap();
        };
        let receive = async {
            tokio::task::yield_now().await;
            assert!(matches!(
                rx.recv().await,
                Some(ControlMessage::RequestKeyframe)
            ));
            let Some(ControlMessage::InputEvent(ev)) = rx.recv().await else {
                panic!("missing move")
            };
            assert!(matches!(ev.ev, Some(pb::input_event::Ev::MouseMove(_))));
            let Some(ControlMessage::InputEvent(ev)) = rx.recv().await else {
                panic!("missing click")
            };
            assert!(matches!(ev.ev, Some(pb::input_event::Ev::MouseButton(_))));
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(send, receive);
        })
        .await
        .unwrap();
    }
}
