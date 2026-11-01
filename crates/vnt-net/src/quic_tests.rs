use super::*;
use vnt_proto::{control::ControlMessage as Msg, pb};

struct Harness {
    address: SocketAddr,
    password: Arc<Mutex<String>>,
    native: Datagram,
    accepted: mpsc::Receiver<Accepted>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn harness(password: &str) -> Harness {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let password = Arc::new(Mutex::new(password.to_string()));
    let value = password.clone();
    let initial = password.lock().unwrap().clone();
    let (mut listener, native) = Listener::new(
        socket,
        initial,
        Arc::new(move || Ok(value.lock().unwrap().clone())),
        Arc::new(|id, token, peer| id == 7 && token == [1; 16] && peer.ip().is_loopback()),
    )
    .await
    .unwrap();
    let (tx, accepted) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        while let Some(value) = listener.accept().await {
            if tx.send(value).await.is_err() {
                break;
            }
        }
    });
    Harness {
        address,
        password,
        native,
        accepted,
        task,
    }
}
async fn next(channel: &mut SessionChannel, predicate: impl Fn(&Msg) -> bool) -> Msg {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = channel.recv().await.unwrap().expect("session closed");
            if predicate(&value) {
                return value;
            }
        }
    })
    .await
    .expect("message timeout")
}
async fn pair(h: &mut Harness, id: u32) -> (SessionChannel, Datagram, Accepted) {
    let password = h.password.lock().unwrap().clone();
    let (mut client, media) = connect(h.address, password.clone()).await.unwrap();
    client
        .send(&Msg::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            transport: 4,
            ..Default::default()
        }))
        .await
        .unwrap();
    let mut server = tokio::time::timeout(Duration::from_secs(5), h.accepted.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        server.channel.recv().await.unwrap(),
        Some(Msg::Hello(_))
    ));
    let task = tokio::spawn(async move {
        let (nonce, proof) = crate::auth::challenge(&mut server.channel).await.unwrap();
        assert!(vnt_proto::auth::verify_remote(&password, &nonce, &proof));
        server
            .channel
            .send(&Msg::Welcome(pb::Welcome {
                system: vnt_proto::system::local() as i32,
                session_id: id,
                transport: 4,
                ..Default::default()
            }))
            .await
            .unwrap();
        server
    });
    let password = h.password.lock().unwrap().clone();
    crate::auth::respond(&mut client, &password).await.unwrap();
    let mut server = task.await.unwrap();
    next(&mut client, |m| matches!(m, Msg::Welcome(_))).await;
    next(
        &mut client,
        |m| matches!(m, Msg::TransportState(s) if s.kind == 1 && s.available),
    )
    .await;
    next(
        &mut server.channel,
        |m| matches!(m, Msg::TransportState(s) if s.kind == 1 && s.available),
    )
    .await;
    (client, media, server)
}

#[test]
fn deterministic_password_keys_and_public_key_pinning() {
    let (key, public) = identity("test password").unwrap();
    assert_eq!(public, identity("test password").unwrap().1);
    assert_ne!(public, identity("other password").unwrap().1);
    assert_eq!(identity("").unwrap().1, identity(" \t\n").unwrap().1);
    assert_eq!(
        identity("").unwrap().1,
        identity(DEFAULT_PASSWORD).unwrap().1
    );
    let certificate = rcgen::CertificateParams::new(vec![NAME.into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let verify = |pin: PinnedKey| {
        pin.verify_server_cert(
            certificate.der(),
            &[],
            &ServerName::try_from(NAME).unwrap(),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(0)),
        )
    };
    assert!(verify(PinnedKey(public)).is_ok());
    assert!(verify(PinnedKey(vec![0; 32])).is_err());
}

#[tokio::test]
async fn native_udp_quic_and_multiple_same_ip_sessions_share_the_port() {
    let mut h = harness("test").await;
    let tcp = tokio::net::TcpListener::bind(h.address).await.unwrap();
    let (_client_a, media_a, server_a) = pair(&mut h, 1).await;
    let (_client_b, media_b, server_b) = pair(&mut h, 2).await;
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let invalid = vnt_proto::control::encode_udp_bind_frame(7, &[2; 16]);
    udp.send_to(&invalid, h.address).await.unwrap();
    udp.send_to(b"invalid QUIC packet", h.address)
        .await
        .unwrap();
    let bind = vnt_proto::control::encode_udp_bind_frame(7, &[1; 16]);
    udp.send_to(&bind, h.address).await.unwrap();
    let mut buffer = [0; 65536];
    let (len, peer) = tokio::time::timeout(Duration::from_secs(2), h.native.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer, udp.local_addr().unwrap());
    assert_eq!(&buffer[..len], bind);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), h.native.recv_from(&mut buffer))
            .await
            .is_err()
    );
    h.native.send_to(b"native response", peer).await.unwrap();
    let (len, _) = udp.recv_from(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..len], b"native response");
    for (media, server, byte) in [(&media_a, &server_a, 11), (&media_b, &server_b, 22)] {
        // Oversized audio/FEC envelopes also survive without a reliable media stream.
        let packet = vec![byte; 2000];
        server.media.send_to(&packet, server.peer).await.unwrap();
        let (len, from) =
            tokio::time::timeout(Duration::from_secs(2), media.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(from, h.address);
        assert_eq!(&buffer[..len], packet);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), media_a.recv_from(&mut buffer))
            .await
            .is_err()
    );
    drop(tcp);
}

#[tokio::test]
async fn file_backpressure_does_not_block_control_and_quic_connection_recovers_media() {
    let mut h = harness("test").await;
    let (client, media, mut server) = pair(&mut h, 7).await;
    let mut files = server.channel.fork_writer().unwrap();
    let blocked = tokio::spawn(async move {
        for id in 0..1024 {
            files
                .send(&Msg::ClipboardReply(pb::ClipboardReply {
                    offer_id: 1,
                    operation_id: 1,
                    request_id: id + 1,
                    data: vec![3; 32768],
                    ..Default::default()
                }))
                .await
                .unwrap();
        }
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !blocked.is_finished(),
        "bulk file lane must be backpressured"
    );
    server
        .channel
        .send(&Msg::Ping(pb::Ping { timestamp_ms: 77 }))
        .await
        .unwrap();
    // Drain only the control lane; bulk traffic remains backpressured.
    let (reader, mut writer) = client.into_split();
    let [mut control, _files, _media] = reader.into_message_lanes().ok().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(control.recv().await, Some(Msg::Ping(p)) if p.timestamp_ms == 77) {
                break;
            }
        }
    })
    .await
    .unwrap();
    blocked.abort();
    let _ = blocked.await;
    let client_control = tokio::spawn(async move {
        while let Some(message) = control.recv().await {
            if let Msg::TransportState(state) = message {
                if state.kind == 0 && state.synchronizing {
                    writer
                        .send(&Msg::SessionPacket(pb::SessionPacket {
                            op: pb::SessionOperation::SessionSyncAck as i32,
                            session_id: 7,
                            generation: state.generation,
                            state_revision: state.state_revision,
                            ..Default::default()
                        }))
                        .await
                        .unwrap();
                }
            }
        }
    });
    let Datagram::Quic(client_media) = &media else {
        unreachable!()
    };
    let old_id = client_media.connection.borrow().stable_id();
    client_media
        .connection
        .borrow()
        .close(0u32.into(), b"test reconnect");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client_media.connection.borrow().stable_id() != old_id { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        loop { if matches!(server.channel.recv().await.unwrap(), Some(Msg::TransportState(s)) if s.kind == 0 && s.available) { break; } }
    }).await.unwrap();
    server
        .media
        .send_to(b"new connection media", server.peer)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let (len, _) = tokio::time::timeout(Duration::from_secs(3), media.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..len], b"new connection media");
    client_control.abort();
}

#[tokio::test]
async fn password_rotation_clear_and_wrong_password_are_used_by_new_handshakes() {
    let mut h = harness("old password").await;
    assert!(connect(h.address, "wrong password".into()).await.is_err());
    let (old_client, old_media, old_server) = pair(&mut h, 1).await;
    *h.password.lock().unwrap() = "new password".into();
    assert!(connect(h.address, "old password".into()).await.is_err());
    let (_new_client, _new_media, _new_server) = pair(&mut h, 2).await;
    old_server
        .media
        .send_to(b"existing session", old_server.peer)
        .await
        .unwrap();
    let mut buffer = [0; 128];
    assert!(
        tokio::time::timeout(Duration::from_secs(2), old_media.recv_from(&mut buffer))
            .await
            .unwrap()
            .is_ok()
    );
    *h.password.lock().unwrap() = String::new();
    assert!(connect(h.address, "new password".into()).await.is_err());
    let (_empty_client, _empty_media, _empty_server) = pair(&mut h, 3).await;
    drop(old_client);
}

#[tokio::test]
async fn dropping_listener_releases_the_udp_port() {
    let h = harness("").await;
    let address = h.address;
    h.task.abort();
    // Await abort so Quinn's driver can release its socket reference.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(h);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(socket) = tokio::net::UdpSocket::bind(address).await {
                drop(socket);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn quic_hello_cannot_be_sent_over_tcp_or_change_its_physical_protocol() {
    let mut h = harness("test").await;
    let (mut client, _media) = connect(h.address, "test".into()).await.unwrap();
    client
        .send(&Msg::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            transport: 1,
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), h.accepted.recv())
            .await
            .is_err()
    );
    let tcp = tokio::net::TcpListener::bind(h.address).await.unwrap();
    let socket = tokio::net::TcpStream::connect(h.address).await.unwrap();
    let (stream, peer) = tcp.accept().await.unwrap();
    let mut client = crate::tcp::ControlChannel::new(socket);
    client
        .send(&Msg::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            transport: 4,
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(SessionListener::default()
        .accept(stream, peer)
        .await
        .is_err());
}

#[tokio::test]
async fn datagram_support_is_required() {
    let mut config = server_config("test").unwrap();
    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(None);
    config.transport_config(Arc::new(transport));
    let endpoint = Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let server = endpoint.clone();
    let task = tokio::spawn(async move {
        if let Some(incoming) = server.accept().await {
            if let Ok(connection) = incoming.await {
                connection.closed().await;
            }
        }
    });
    let error = connect(endpoint.local_addr().unwrap(), "test".into())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("DATAGRAM"), "{error}");
    endpoint.close(0u32.into(), b"test complete");
    task.await.unwrap();
}

#[derive(Debug)]
struct ForgedKey;
impl rustls::sign::SigningKey for ForgedKey {
    fn choose_scheme(&self, schemes: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        schemes
            .contains(&SignatureScheme::ED25519)
            .then(|| Box::new(ForgedKey) as Box<dyn rustls::sign::Signer>)
    }
    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::ED25519
    }
}
impl rustls::sign::Signer for ForgedKey {
    fn sign(&self, _: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        Ok(vec![0; 64])
    }
    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}
#[derive(Debug)]
struct ForgedResolver(Arc<rustls::sign::CertifiedKey>);
impl rustls::server::ResolvesServerCert for ForgedResolver {
    fn resolve(
        &self,
        _: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn matching_public_key_with_forged_handshake_signature_is_rejected() {
    let (key, _) = identity("test").unwrap();
    let cert = rcgen::CertificateParams::new(vec![NAME.into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let key = Arc::new(rustls::sign::CertifiedKey::new(
        vec![cert.der().clone()],
        Arc::new(ForgedKey),
    ));
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(ForgedResolver(key)));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
    ));
    let endpoint = Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let server = endpoint.clone();
    let task = tokio::spawn(async move {
        if let Some(incoming) = server.accept().await {
            assert!(incoming.await.is_err());
        }
    });
    assert!(connect(endpoint.local_addr().unwrap(), "test".into())
        .await
        .is_err());
    endpoint.close(0u32.into(), b"test complete");
    task.await.unwrap();
}

#[tokio::test]
async fn fec_recovers_a_missing_video_fragment_over_quic_datagrams() {
    let mut h = harness("test").await;
    let (_client, media, server) = pair(&mut h, 7).await;
    let data = vec![42; 32000];
    let mut packets = Vec::new();
    vnt_proto::udp::fragment_frame(7, 0, true, 123, &data, 1000, &mut packets).unwrap();
    let originals = packets.len();
    let fec = crate::fec::Sender::negotiated(crate::fec::VERSION, true).unwrap();
    let mut packets = fec.video(7, Default::default(), packets);
    assert!(packets.len() > originals, "test requires parity");
    packets.remove(3);
    // Reverse arrival order, duplicate one packet, and retain a missing original.
    server
        .media
        .send_to(packets.last().unwrap(), server.peer)
        .await
        .unwrap();
    for packet in packets.into_iter().rev() {
        server.media.send_to(&packet, server.peer).await.unwrap();
    }
    let mut receiver = crate::udp::UdpVideoReceiver::from_socket(media, h.address).unwrap();
    receiver.set_session(7);
    receiver.set_fec(true);
    let frame = tokio::time::timeout(Duration::from_secs(3), receiver.recv_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, data);
    assert_eq!(frame.pts_ms, 123);
    // Loss telemetry is finalized only after the reorder/FEC deadline.
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(receiver
        .fec_reports(Some(0))
        .iter()
        .any(|report| report.recovered > 0));
}

#[tokio::test]
async fn existing_input_clipboard_and_file_messages_use_reliable_lanes() {
    let mut h = harness("test").await;
    let (mut client, _media, mut server) = pair(&mut h, 7).await;
    for message in [
        Msg::InputEvent(pb::InputEvent {
            ev: Some(pb::input_event::Ev::Key(pb::KeyEvent {
                scancode: 30,
                extended: false,
                down: true,
            })),
        }),
        Msg::InputEvent(pb::InputEvent {
            ev: Some(pb::input_event::Ev::MouseMove(pb::MouseMove {
                x: 0.2,
                y: 0.7,
            })),
        }),
        Msg::ClipboardText(pb::ClipboardText {
            text: "QUIC clipboard".into(),
        }),
        Msg::ClipboardReply(pb::ClipboardReply {
            offer_id: 1,
            operation_id: 1,
            request_id: 9,
            data: vec![7; 32768],
            ..Default::default()
        }),
    ] {
        client.send(&message).await.unwrap();
        let received = next(&mut server.channel, |m| m.msg_type() == message.msg_type()).await;
        assert_eq!(received, message);
    }
    let header = Msg::ClipboardImage(pb::ClipboardImage {
        image_id: 1,
        width: 1,
        height: 1,
        total_bytes: 4,
    });
    client.send(&header).await.unwrap();
    assert_eq!(
        next(&mut server.channel, |m| matches!(m, Msg::ClipboardImage(_))).await,
        header
    );
    server
        .channel
        .send(&Msg::ClipboardImageReady(pb::ClipboardImageReady {
            image_id: 1,
            ..Default::default()
        }))
        .await
        .unwrap();
    let chunk = Msg::ClipboardImageChunk(pb::ClipboardImageChunk {
        image_id: 1,
        data: vec![1, 2, 3, 4],
        ..Default::default()
    });
    client.send(&chunk).await.unwrap();
    assert_eq!(
        next(&mut server.channel, |m| matches!(
            m,
            Msg::ClipboardImageChunk(_)
        ))
        .await,
        chunk
    );
}
