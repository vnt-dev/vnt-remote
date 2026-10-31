//! Run explicitly against a disposable upgraded vnts, never a user's saved server.
use futures_util::StreamExt;
use std::{sync::Arc, time::Duration};
use vnt_host::link::{LinkConfig, ServerLink};
use vnt_proto::Transport;
use vnt_remote_lib::{
    client::{self, ConnectParams},
    server, NoopEventSink,
};

async fn online(link: &ServerLink, node: &str) {
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let status = link.snapshot().await;
            assert!(
                status.error.is_none(),
                "{}",
                status.error.unwrap_or_default()
            );
            if status.devices.iter().any(|d| d.id == node) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("nodes did not register and synchronize");
}

async fn online_named(link: &ServerLink, name: &str) -> String {
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let status = link.snapshot().await;
            assert!(status.error.is_none(), "{:?}", status.error);
            if let Some(device) = status.devices.iter().find(|d| d.name == name) {
                return device.id.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("nodes did not register and synchronize")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VNT_TEST_SERVER pointing to disposable upgraded vnts"]
async fn vnts_auto_group_device_sync_passwordless_password_and_fec_video() {
    let addresses = std::env::var("VNT_TEST_SERVER").expect("VNT_TEST_SERVER is required");
    let group = format!("test-{}", std::process::id());
    let a = Arc::new(ServerLink::default());
    let b = Arc::new(ServerLink::default());
    for (link, id) in [(&a, "123456789"), (&b, "123456790")] {
        link.configure(LinkConfig {
            enabled: true,
            mode: "manual".into(),
            servers: addresses.split(',').map(String::from).collect(),
            network_code: group.clone(),
            connection_id: id.into(),
            device_name: if id == "123456789" {
                "controller"
            } else {
                "host"
            }
            .into(),
            encryption_password: "integration-encryption-key".into(),
            subscription: String::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    let host_id = online_named(&a, "host").await;
    online_named(&b, "controller").await;
    for access in ["", "required-password"] {
        let clipboard = vnt_host::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
        let host = Arc::new(
            server::start(
                Arc::new(NoopEventSink),
                server::ServerConfig {
                    password: access.into(),
                    port: 0,
                    monitor_index: 0,
                    region: None,
                    fps: 30,
                    bitrate_kbps: 8000,
                    hardware_encode: false,
                    audio: false,
                    view_only: true,
                    clipboard: clipboard.clone(),
                    synthetic: true,
                },
            )
            .await
            .unwrap(),
        );
        let accepted = host.clone();
        let link = b.clone();
        let task = tokio::spawn(async move {
            loop {
                if let Some((channel, peer, media)) = link.next_incoming().await {
                    accepted.accept_overlay(channel, peer, media);
                }
            }
        });
        let params = |password: &str| ConnectParams {
            clipboard_enabled: true,
            server_config_id: None,
            server_node_id: Some(host_id.clone()),
            server_network_code: Some(group.clone()),
            color_depth: 24,
            show_wallpaper: true,
            drag_full_windows: false,
            fec_enabled: true,
            resolution_mode: "none".into(),
            target_width: 0,
            target_height: 0,
            transport: Transport::Vnt,
            host: "0.0.0.0".into(),
            port: 29875,
            password: password.into(),
            codecs: vec!["h264".into()],
            hardware_codecs: vec![],
            fps: 30,
            bitrate_kbps: 8000,
            audio_enabled: false,
            audio_device_id: None,
        };
        if !access.is_empty() {
            let prepared = vnt_net::overlay::prepare(a.peer(&host_id).await.unwrap())
                .await
                .unwrap();
            assert!(client::connect_prepared(
                Arc::new(NoopEventSink),
                params("wrong"),
                clipboard.clone(),
                Some(prepared)
            )
            .await
            .is_err());
        }
        let prepared = vnt_net::overlay::prepare(a.peer(&host_id).await.unwrap())
            .await
            .unwrap();
        let mut request = params(access);
        request.host = prepared.peer.ip().to_string();
        let (client, info) = client::connect_prepared(
            Arc::new(NoopEventSink),
            request,
            clipboard.clone(),
            Some(prepared),
        )
        .await
        .unwrap();
        assert_eq!(info.transport, Transport::Vnt);
        assert!(info.fec_enabled && info.view_only);
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/?t={}",
            info.ws_port, info.token
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                match ws.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes)))
                        if bytes.len() > 30 =>
                    {
                        break
                    }
                    Some(Err(error)) => panic!("video bridge failed: {error}"),
                    None => panic!("video bridge closed"),
                    _ => {}
                }
            }
        })
        .await
        .expect("VNT FEC video did not reach the controller");
        client.disconnect();
        drop(ws);
        task.abort();
        let _ = task.await;
        match Arc::try_unwrap(host) {
            Ok(host) => host.stop().await,
            Err(_) => panic!("host dispatcher retained server"),
        }
    }
    b.stop().await;
    a.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VNT_TEST_SUBSCRIPTIONS and a disposable vnts config update"]
async fn private_group_subscriptions_receive_and_apply_a_live_update() {
    let subscriptions: Vec<String> =
        serde_json::from_str(&std::env::var("VNT_TEST_SUBSCRIPTIONS").unwrap()).unwrap();
    let a = Arc::new(ServerLink::default());
    let b = Arc::new(ServerLink::default());
    for (link, subscription) in [(&a, &subscriptions[0]), (&b, &subscriptions[1])] {
        link.configure(LinkConfig {
            enabled: true,
            mode: "subscription".into(),
            subscription: subscription.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    online(&a, "host").await;
    online(&b, "controller").await;
    let prepared = vnt_net::overlay::prepare(a.peer("host").await.unwrap())
        .await
        .unwrap();
    drop(prepared);
    std::fs::write("../target/integration-vnts/subscription-ready", b"ready").unwrap();
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            if a.snapshot()
                .await
                .devices
                .iter()
                .any(|d| d.id == "host" && d.name == "host-updated")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("subscription update was not applied and announced");
    assert_eq!(b.snapshot().await.network_code, "subscription-test");
    b.stop().await;
    a.stop().await;
}

/// Both groups deliberately reuse the same controller and host node IDs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VNT_TEST_SERVER pointing to disposable upgraded vnts"]
async fn vnts_multiple_profiles_keep_control_and_media_on_the_selected_source() {
    use vnt_host::link::{LinkTarget, ServerLinks};
    use vnt_proto::{control::ControlMessage as Msg, pb};
    let addresses = std::env::var("VNT_TEST_SERVER").expect("VNT_TEST_SERVER is required");
    let controller = ServerLinks::default();
    let hosts = [
        Arc::new(ServerLink::default()),
        Arc::new(ServerLink::default()),
    ];
    let mut configs = Vec::new();
    for (index, host) in hosts.iter().enumerate() {
        let config = LinkConfig {
            id: format!("profile-{index}"),
            name: format!("Profile {index}"),
            enabled: true,
            mode: "manual".into(),
            servers: addresses.split(',').map(String::from).collect(),
            network_code: format!("multi-{}-{index}", std::process::id()),
            connection_id: "123456789".into(),
            device_name: "Controller".into(),
            ..Default::default()
        };
        host.configure(LinkConfig {
            connection_id: "123456790".into(),
            device_name: format!("Host {index}"),
            ..config.clone()
        })
        .await
        .unwrap();
        configs.push(config);
    }
    controller.configure(configs.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let statuses = controller.snapshot().await;
            assert!(statuses.iter().all(|s| s.status.error.is_none()));
            if statuses.len() == 2
                && statuses
                    .iter()
                    .all(|s| s.status.devices.iter().any(|d| d.name.starts_with("Host ")))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("both profiles did not synchronize");
    let mut sessions = Vec::new();
    for (index, config) in configs.iter().enumerate() {
        let target = LinkTarget {
            config_id: config.id.clone(),
            network_code: Some(config.network_code.clone()),
            node: controller.snapshot().await[index]
                .status
                .devices
                .iter()
                .find(|d| d.name == format!("Host {index}"))
                .unwrap()
                .id
                .clone(),
        };
        let link = controller.select(&target).await.unwrap();
        let mut prepared = vnt_net::overlay::prepare(link.peer(&target.node).await.unwrap())
            .await
            .unwrap();
        prepared
            .channel
            .send(&Msg::Hello(pb::Hello {
                color_depth: 24,
                desktop_effects: Some(Default::default()),
                proto_ver: vnt_proto::PROTO_VER,
                transport: 3,
                ..Default::default()
            }))
            .await
            .unwrap();
        let (mut host_channel, address, media) =
            tokio::time::timeout(Duration::from_secs(5), hosts[index].next_incoming())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            host_channel.recv().await.unwrap(),
            Some(Msg::Hello(_))
        ));
        let (challenge, response) = tokio::join!(
            vnt_net::auth::challenge(&mut host_channel),
            vnt_net::auth::respond(&mut prepared.channel, "")
        );
        response.unwrap();
        let (nonce, proof) = challenge.unwrap();
        assert!(vnt_proto::auth::verify_remote("", &nonce, &proof));
        host_channel
            .send(&Msg::Welcome(pb::Welcome {
                session_id: index as u32 + 1,
                transport: 3,
                ..Default::default()
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(
                prepared.channel.recv().await.unwrap(),
                Some(Msg::Welcome(_))
            ) {}
        })
        .await
        .unwrap();
        sessions.push((prepared, host_channel, address, media));
    }
    // Stop the first profile; the second established control and media lanes survive.
    configs[0].enabled = false;
    controller.configure(configs).await.unwrap();
    let (prepared, host, address, media) = &mut sessions[1];
    host.send(&Msg::Ping(pb::Ping { timestamp_ms: 222 }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Msg::Ping(ping)) = prepared.channel.recv().await.unwrap() {
                assert_eq!(ping.timestamp_ms, 222);
                break;
            }
        }
    })
    .await
    .unwrap();
    let payload = b"media-from-second-profile";
    media.send_to(payload, *address).await.unwrap();
    let mut buffer = [0; 128];
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(5),
        prepared.media.recv_from(&mut buffer),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&buffer[..len], payload);
    controller.stop().await;
    for host in hosts {
        host.stop().await;
    }
}
