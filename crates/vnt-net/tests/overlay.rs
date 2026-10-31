//! No external server, virtual NIC or privileged desktop is needed for this test.
use std::{
    net::{Ipv4Addr, TcpListener, UdpSocket},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use vnt_core::{
    context::config::Config, log_manager::InstanceLog, network_info::RuntimeChangeManager,
};
use vnt_net::{
    datagram::{Datagram, OverlayDatagrams},
    overlay::{self, Peer},
    tcp::ControlChannel,
};
use vnt_proto::{control::ControlMessage as Msg, pb};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VNT_TEST_SERVER pointing to disposable upgraded vnts"]
async fn server_relay_carries_encrypted_streams_and_rejects_a_different_key() {
    let server = std::env::var("VNT_TEST_SERVER").expect("disposable VNT_TEST_SERVER is required");
    let group = format!("relay-{}", std::process::id());
    let mut managers = Vec::new();
    for (id, key) in [("a", "shared"), ("b", "shared"), ("wrong-key", "different")] {
        let manager = RuntimeChangeManager::new(
            Config {
                server_addr: vec![server.parse().unwrap()],
                network_code: group.clone(),
                device_id: id.into(),
                connection_id: Some(
                    match id {
                        "a" => "001",
                        "b" => "0001",
                        _ => "0",
                    }
                    .parse()
                    .unwrap(),
                ),
                password: Some(key.into()),
                no_punch: true,
                ..Default::default()
            },
            None,
            Arc::new(InstanceLog::new("relay-test")),
        )
        .await
        .unwrap();
        manager.wait_network_ready().await.unwrap();
        managers.push(manager);
    }
    let a = managers[0].api().unwrap();
    let b = managers[1].api().unwrap();
    let a_ip = a.network().unwrap().ip;
    let b_ip = b.network().unwrap().ip;
    let a_endpoint = a.application_endpoint().unwrap();
    let b_endpoint = b.application_endpoint().unwrap();
    let (client, host) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(a_endpoint.connect(b_ip), b_endpoint.accept())
    })
    .await
    .expect("relay application handshake timed out");
    let client = client.unwrap();
    let host = host.unwrap().unwrap();
    let (mut send, _) = client.open_bi().await.unwrap();
    send.write_all(b"encrypted-over-server-relay")
        .await
        .unwrap();
    let (_, mut recv) = host.accept_bi().await.unwrap();
    let mut received = vec![0; 27];
    recv.read_exact(&mut received).await.unwrap();
    assert_eq!(received, b"encrypted-over-server-relay");
    host.send_datagram(bytes::Bytes::from_static(b"relay-media"))
        .unwrap();
    assert_eq!(
        client.read_datagram().await.unwrap().as_ref(),
        b"relay-media"
    );
    assert!(!a.is_direct(&b_ip) && !b.is_direct(&a_ip));
    let wrong = managers[2].api().unwrap().application_endpoint().unwrap();
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(3), wrong.connect(b_ip)).await {
        assert!(
            result.is_err(),
            "a different encryption key established an application connection"
        );
    }
    for manager in &mut managers {
        manager.stop().await;
    }
}

fn ports() -> [u16; 2] {
    let mut sockets = Vec::new();
    while sockets.len() < 2 {
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        if let Ok(tcp) = TcpListener::bind(udp.local_addr().unwrap()) {
            sockets.push((udp, tcp));
        }
    }
    [
        sockets[0].0.local_addr().unwrap().port(),
        sockets[1].0.local_addr().unwrap().port(),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encrypted_overlay_has_independent_control_files_media_and_closes_with_core() {
    let ports = ports();
    let mut managers = Vec::new();
    for i in 0..2 {
        let config = Config {
            network_code: "application-test".into(),
            device_id: format!("node-{i}"),
            device_name: format!("test-{i}"),
            connection_id: Some(format!("00{}", i + 1).parse().unwrap()),
            password: Some("shared-encryption-key".into()),
            no_punch: true,
            tunnel_addr: vec![(Ipv4Addr::LOCALHOST, ports[i]).into()],
            peer_address: vec![format!("tcp://127.0.0.1:{}", ports[1 - i]).parse().unwrap()],
            ..Default::default()
        };
        let manager =
            RuntimeChangeManager::new(config, None, Arc::new(InstanceLog::new("app-test")))
                .await
                .unwrap();
        manager.wait_network_ready().await.unwrap();
        managers.push(manager);
    }
    let a = managers[0].api().unwrap();
    let b = managers[1].api().unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !a.is_direct(
            &"002"
                .parse::<vnt_core::connection_id::ConnectionId>()
                .unwrap()
                .transport_key(),
        ) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let host_media = Arc::new(OverlayDatagrams::default());
    host_media.set_local("0.0.3.234:29875".parse().unwrap());
    let client_media = Arc::new(OverlayDatagrams::default());
    client_media.set_local("0.0.3.233:29875".parse().unwrap());
    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    overlay::listen(
        b.application_endpoint().unwrap(),
        host_media.clone(),
        cancel.clone(),
        tx,
    );
    let (reverse_tx, mut reverse_rx) = tokio::sync::mpsc::channel(4);
    overlay::listen(
        a.application_endpoint().unwrap(),
        client_media.clone(),
        cancel.clone(),
        reverse_tx,
    );
    let prepared = overlay::prepare(Peer {
        endpoint: a.application_endpoint().unwrap(),
        ip: "002"
            .parse::<vnt_core::connection_id::ConnectionId>()
            .unwrap()
            .transport_key(),
        datagrams: client_media.clone(),
        cancel: cancel.clone(),
        dialing: Default::default(),
    })
    .await
    .unwrap();
    let mut client = prepared.channel;
    client
        .send(&Msg::Hello(pb::Hello {
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            proto_ver: vnt_proto::PROTO_VER,
            transport: 3,
            ..Default::default()
        }))
        .await
        .unwrap();
    let (mut host, _) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(host.recv().await.unwrap(), Some(Msg::Hello(_))));
    let (challenge, response) = tokio::join!(
        vnt_net::auth::challenge(&mut host),
        vnt_net::auth::respond(&mut client, "")
    );
    response.unwrap();
    let (nonce, proof) = challenge.unwrap();
    assert!(vnt_proto::auth::verify_remote("", &nonce, &proof));
    host.send(&Msg::Welcome(pb::Welcome {
        session_id: 19,
        transport: 3,
        ..Default::default()
    }))
    .await
    .unwrap();
    while !matches!(client.recv().await.unwrap(), Some(Msg::Welcome(_))) {}
    host.send(&Msg::ClipboardReply(pb::ClipboardReply {
        offer_id: 1,
        operation_id: 1,
        request_id: 1,
        data: vec![42; 16000],
        ..Default::default()
    }))
    .await
    .unwrap();
    host.send(&Msg::Ping(pb::Ping { timestamp_ms: 123 }))
        .await
        .unwrap();
    let media = Datagram::overlay_peer(host_media.clone(), "0.0.3.233:29875".parse().unwrap());
    let data = vec![37; 1415];
    media
        .send_to(&data, "0.0.3.233:29875".parse().unwrap())
        .await
        .unwrap();
    let mut buffer = vec![0; 1500];
    let (len, peer) = tokio::time::timeout(
        Duration::from_secs(3),
        prepared.media.recv_from(&mut buffer),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(peer.ip().to_string(), "0.0.3.234");
    assert_eq!(&buffer[..len], data);
    let mut file = false;
    let mut control = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !file || !control {
            match client.recv().await.unwrap() {
                Some(Msg::ClipboardReply(chunk)) => {
                    assert_eq!(chunk.data, vec![42; 16000]);
                    file = true;
                }
                Some(Msg::Ping(p)) => {
                    assert_eq!(p.timestamp_ms, 123);
                    control = true;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    // A node can be both controller and host. Streams opened on an inbound-only
    // connection must not be stranded on a peer without an accept loop.
    let mut reverse = overlay::prepare(Peer {
        endpoint: b.application_endpoint().unwrap(),
        ip: "001"
            .parse::<vnt_core::connection_id::ConnectionId>()
            .unwrap()
            .transport_key(),
        datagrams: host_media.clone(),
        cancel: cancel.clone(),
        dialing: Default::default(),
    })
    .await
    .unwrap();
    reverse
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
    let (mut accepted, _) = tokio::time::timeout(Duration::from_secs(3), reverse_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        accepted.recv().await.unwrap(),
        Some(Msg::Hello(_))
    ));
    let endpoint = b.application_endpoint().unwrap();
    managers[1].stop().await;
    assert!(endpoint
        .connect(
            "001"
                .parse::<vnt_core::connection_id::ConnectionId>()
                .unwrap()
                .transport_key()
        )
        .await
        .is_err());
    cancel.cancel();
    managers[0].stop().await;
    // The generic channel still accepts the native stream interface.
    let (stream, _) = tokio::io::duplex(64);
    let _ = ControlChannel::new(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_rejects_another_nodes_encryption_password() {
    let ports = ports();
    let mut managers = Vec::new();
    for i in 0..2 {
        let config = Config {
            network_code: "wrong-key-test".into(),
            device_id: format!("node-{i}"),
            connection_id: Some(format!("00{}", i + 1).parse().unwrap()),
            password: Some(
                if i == 0 {
                    "expected-key"
                } else {
                    "different-key"
                }
                .into(),
            ),
            no_punch: true,
            tunnel_addr: vec![(Ipv4Addr::LOCALHOST, ports[i]).into()],
            peer_address: vec![format!("tcp://127.0.0.1:{}", ports[1 - i]).parse().unwrap()],
            ..Default::default()
        };
        let manager =
            RuntimeChangeManager::new(config, None, Arc::new(InstanceLog::new("wrong-key-test")))
                .await
                .unwrap();
        manager.wait_network_ready().await.unwrap();
        managers.push(manager);
    }
    let endpoint = managers[0].api().unwrap().application_endpoint().unwrap();
    let peer = managers[1].api().unwrap().application_endpoint().unwrap();
    let (client, host) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(2),
            endpoint.connect("0.0.3.234".parse().unwrap())
        ),
        tokio::time::timeout(Duration::from_secs(2), peer.accept()),
    );
    assert!(
        !matches!(client, Ok(Ok(_))),
        "different passwords established an application connection"
    );
    assert!(
        !matches!(host, Ok(Ok(Some(_)))),
        "different passwords accepted an application connection"
    );
    for manager in &mut managers {
        manager.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires disposable VNT_TEST_SUBSCRIPTION_A/B and VNT_TEST_API/TOKEN"]
async fn subscriptions_relay_and_rebuild_when_the_connection_id_changes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut managers = Vec::new();
    for key in ["VNT_TEST_SUBSCRIPTION_A", "VNT_TEST_SUBSCRIPTION_B"] {
        let subscription =
            vnt_core::managed_config::Subscription::parse(&std::env::var(key).unwrap()).unwrap();
        let manager = tokio::time::timeout(
            Duration::from_secs(15),
            RuntimeChangeManager::new(
                Config::default(),
                Some(subscription),
                Arc::new(InstanceLog::new("subscription-relay")),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        manager.wait_network_ready().await.unwrap();
        managers.push(manager);
    }
    let key = |id: &str| {
        id.parse::<vnt_core::connection_id::ConnectionId>()
            .unwrap()
            .transport_key()
    };
    assert_eq!(managers[0].api().unwrap().network().unwrap().ip, key("001"));
    assert_eq!(
        managers[1].api().unwrap().network().unwrap().ip,
        key("0001")
    );
    async fn exchange(a: &RuntimeChangeManager, b: &RuntimeChangeManager) {
        let a = a.api().unwrap().application_endpoint().unwrap();
        let b_api = b.api().unwrap();
        let b = b_api.application_endpoint().unwrap();
        let (client, host) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(a.connect(b_api.network().unwrap().ip), b.accept())
        })
        .await
        .unwrap();
        let client = client.unwrap();
        let host = host.unwrap().unwrap();
        client
            .send_datagram(bytes::Bytes::from_static(b"subscription-relay"))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), host.read_datagram())
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            b"subscription-relay"
        );
        a.close();
        b.close();
    }
    exchange(&managers[0], &managers[1]).await;
    let api = std::env::var("VNT_TEST_API").unwrap();
    let token = std::env::var("VNT_TEST_TOKEN").unwrap();
    let body = r#"{"connection_id":"0"}"#;
    let mut http = tokio::net::TcpStream::connect(&api).await.unwrap();
    let request = format!(
        "PUT /api/networks/remote-sub/devices/sub-a/client-config HTTP/1.1\r\nHost: {api}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    http.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    http.read_to_string(&mut response).await.unwrap();
    assert!(
        response.contains("\"code\":200"),
        "configuration update was rejected"
    );
    let change = tokio::time::timeout(Duration::from_secs(15), managers[0].changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(change.config.connection_id.unwrap().to_string(), "0");
    assert_eq!(
        managers[0].apply_change(&change).await.unwrap(),
        vnt_core::network_info::ChangeOutcome::Rebuilt
    );
    assert_eq!(managers[0].api().unwrap().network().unwrap().ip, key("0"));
    // Rebuild creates a fresh application endpoint; use a fresh peer endpoint as well.
    managers[1].rebuild().await.unwrap();
    exchange(&managers[0], &managers[1]).await;
    for manager in &mut managers {
        manager.stop().await;
    }
}
