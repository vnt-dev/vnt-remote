use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::net::TcpListener;

#[tokio::test(start_paused = true)]
async fn blocked_media_resumes_before_the_periodic_tick() {
    let now = Instant::now();
    let (events, events_rx) = mpsc::channel(4);
    let (app_tx, _app_rx) = mpsc::channel(4);
    let (control, control_rx) = mpsc::channel(4);
    let (files, files_rx) = mpsc::channel(4);
    let (media, media_rx) = mpsc::channel(4);
    let (writer, mut receiver) = mpsc::channel(1);
    let frame = |pts_ms| {
        Msg::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms,
            data: vec![1],
        })
    };
    writer.try_send(frame(1)).unwrap();
    let mut supervisor = Supervisor {
        server: None,
        address: None,
        id: 1,
        secret: [0; 32],
        record: None,
        tcp_media: Arc::new(AtomicBool::new(true)),
        traffic: TrafficCounters::default(),
        events,
        app: std::array::from_fn(|_| app_tx.clone()),
        delivered_generations: std::array::from_fn(|_| Arc::new(AtomicU64::new(0))),
        links: [
            None,
            None,
            Some(Link {
                tx: writer,
                cancel: CancellationToken::new(),
                generation: 1,
            }),
        ],
        generations: [0, 0, 1],
        lost: [None; 3],
        next_retry: [now; 3],
        attempts: [0; 3],
        retrying: [false; 3],
        pending: [None, None, Some(frame(2))],
        requests: Default::default(),
        awaiting: Default::default(),
        snapshots: Default::default(),
        snapshot_revision: 0,
        sync_revision: 0,
        ready_image: None,
        media_config: Default::default(),
        screen_snapshots: Default::default(),
        screen_waiting: Default::default(),
        open_screens: None,
        media_needs_keyframe: false,
        tasks: JoinSet::new(),
        last_control_rx: now,
        last_heartbeat: now,
        last_app_status: now,
    };
    let task = tokio::spawn(async move {
        supervisor
            .run(events_rx, [control_rx, files_rx, media_rx])
            .await;
    });
    // Keep runnable work so Tokio cannot automatically advance the paused clock.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(receiver.try_recv().unwrap(), Msg::VideoFrame(f) if f.pts_ms == 1));
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(receiver.try_recv().unwrap(), Msg::VideoFrame(f) if f.pts_ms == 2));
    assert_eq!(
        Instant::now(),
        now,
        "must not wait for the 20 ms retry tick"
    );
    task.abort();
    drop((control, files, media));
}

#[tokio::test]
async fn multiscreen_recovery_replays_only_open_routes_and_gates_each_reference_chain() {
    use vnt_proto::screen;
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, true).await;
    let info = |id| pb::ScreenInfo {
        stream_id: id,
        name: format!("Screen {id}"),
        width: 64,
        height: 48,
        fps: 30,
        codec: pb::Codec::H264 as i32,
        available: true,
        generation: 1,
        ..Default::default()
    };
    server
        .send(&Msg::ScreensState(pb::ScreensState {
            revision: 1,
            active_stream_id: 1,
            screens: vec![info(1), info(2)],
        }))
        .await
        .unwrap();
    next(&mut client, |m| matches!(m, Msg::ScreensState(_))).await;
    for id in [1, 2] {
        server
            .send(&screen::wrap(
                id,
                1,
                Msg::StreamReset(pb::StreamReset {
                    width: 64,
                    height: 48,
                    fps: 30,
                    decoder_config: vec![],
                }),
            ))
            .await
            .unwrap();
    }
    let mut configs = std::collections::HashSet::new();
    while configs.len() < 2 {
        if let Msg::ScreenPacket(p) = next(&mut client, screen::is_config).await {
            configs.insert(p.stream_id);
        }
    }
    server
        .send(&Msg::ScreensState(pb::ScreensState {
            revision: 2,
            active_stream_id: 2,
            screens: vec![info(2)],
        }))
        .await
        .unwrap();
    next(
        &mut client,
        |m| matches!(m,Msg::ScreensState(s) if s.screens.len()==1),
    )
    .await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    record
        .sender
        .send(Event::Down {
            kind: MEDIA,
            generation: 1,
        })
        .await
        .unwrap();
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==2 && !s.available),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(4), async {
        let (mut config, mut online) = (false, false);
        while !config || !online {
            match client.recv().await.unwrap().unwrap() {
                Msg::ScreenPacket(p)
                    if screen::kind(&p) == Some(vnt_proto::control::MessageType::StreamReset) =>
                {
                    assert_eq!(p.stream_id, 2);
                    config = true;
                }
                Msg::TransportState(s) if s.kind == 2 && s.available && s.generation == 2 => {
                    online = true
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let frame = |id, keyframe, pts_ms| {
        screen::wrap(
            id,
            1,
            Msg::VideoFrame(pb::VideoFrame {
                dependencies: None,
                keyframe,
                pts_ms,
                data: vec![1; 100],
            }),
        )
    };
    server.send(&frame(1, true, 100)).await.unwrap();
    server.send(&frame(2, false, 110)).await.unwrap();
    server.send(&frame(2, true, 120)).await.unwrap();
    let Msg::ScreenPacket(packet) = next(&mut client, screen::is_video).await else {
        panic!("expected routed video")
    };
    assert_eq!(packet.stream_id, 2);
    assert!(matches!(screen::unwrap(&packet),Some(Msg::VideoFrame(f)) if f.pts_ms==120));
    record
        .sender
        .send(Event::Down {
            kind: CONTROL,
            generation: 0,
        })
        .await
        .unwrap();
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==0 && !s.available),
    )
    .await;
    let Msg::ScreensState(state) = next(&mut client, |m| matches!(m, Msg::ScreensState(_))).await
    else {
        panic!("expected screens state")
    };
    assert_eq!(state.screens.len(), 1);
    assert_eq!(state.active_stream_id, 2);
    next(
        &mut client,
        |m| matches!(m,Msg::TransportState(s) if s.kind==0 && s.available),
    )
    .await;
    assert!(h.applications.try_recv().is_err());
}

#[tokio::test]
async fn full_video_mailbox_preserves_references_without_blocking_control() {
    let (control_tx, mut control_rx) = mpsc::channel(1);
    let (file_tx, _files) = mpsc::channel(1);
    let (media_tx, mut media_rx) = mpsc::channel(1);
    let (events, _events_rx) = mpsc::channel(1);
    let producer = Outgoing {
        local: None,
        lanes: [control_tx, file_tx, media_tx],
        tcp_media: Arc::new(AtomicBool::new(true)),
        events,
    };
    let frame = |pts_ms| {
        Msg::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: pts_ms == 0,
            pts_ms,
            data: vec![1; 100],
        })
    };
    producer.send(frame(0)).await.unwrap();
    let next = producer.clone();
    let pending = tokio::spawn(async move { next.send(frame(33)).await });
    tokio::task::yield_now().await;
    assert!(
        !pending.is_finished(),
        "encoded video must wait instead of silently dropping"
    );
    producer
        .send(Msg::Ping(pb::Ping { timestamp_ms: 7 }))
        .await
        .unwrap();
    assert!(matches!(control_rx.recv().await, Some(Msg::Ping(_))));
    assert!(matches!(media_rx.recv().await, Some(Msg::VideoFrame(f)) if f.pts_ms == 0));
    pending.await.unwrap().unwrap();
    assert!(matches!(media_rx.recv().await, Some(Msg::VideoFrame(f)) if f.pts_ms == 33));
}

#[tokio::test]
async fn queued_rejection_reaches_peer_when_application_drops_immediately() {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, peer) = tcp.accept().await.unwrap();
        let mut channel = SessionListener::default()
            .accept(stream, peer)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(channel.recv().await.unwrap(), Some(Msg::Hello(_))));
        channel
            .send(&Msg::Denied(pb::Denied {
                reason: "password changed".into(),
            }))
            .await
            .unwrap();
        // The application must not need a sleep to flush the supervisor.
    });
    let mut client = connect(address).await.unwrap();
    client
        .send(&Msg::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: vnt_proto::PROTO_VER,
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(
        matches!(next(&mut client, |m| matches!(m, Msg::Denied(_))).await,
        Msg::Denied(reason) if reason.reason == "password changed")
    );
    server.await.unwrap();
}

async fn next_server(channel: &mut SessionChannel, predicate: impl Fn(&Msg) -> bool) -> Msg {
    next_inner(channel, predicate, false).await
}
async fn next(channel: &mut SessionChannel, predicate: impl Fn(&Msg) -> bool) -> Msg {
    next_inner(channel, predicate, true).await
}
async fn next_inner(
    channel: &mut SessionChannel,
    predicate: impl Fn(&Msg) -> bool,
    client: bool,
) -> Msg {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let msg = channel.recv().await.unwrap().expect("session ended");
            if let Msg::TransportState(state) = &msg {
                if client && state.kind == 0 && state.synchronizing {
                    let mut ack = packet(SYNC_ACK, CONTROL, 123);
                    ack.generation = state.generation;
                    ack.state_revision = state.state_revision;
                    channel.send(&Msg::SessionPacket(ack)).await.unwrap();
                }
            }
            if predicate(&msg) {
                return msg;
            }
        }
    })
    .await
    .expect("message timeout")
}
struct Harness {
    address: SocketAddr,
    listener: SessionListener,
    applications: mpsc::Receiver<SessionChannel>,
    task: tokio::task::JoinHandle<()>,
    accepts: Arc<AtomicUsize>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn harness() -> Harness {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let listener = SessionListener::default();
    let router = listener.clone();
    let accepts = Arc::new(AtomicUsize::new(0));
    let count = accepts.clone();
    let (tx, applications) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                received=tcp.accept()=>{let (stream,peer)=received.unwrap(); count.fetch_add(1,Ordering::Relaxed); let router=router.clone();let tx=tx.clone();connections.spawn(async move { if let Ok(Some(app))=router.accept(stream,peer).await {let _=tx.send(app).await;} });},
                Some(_)=connections.join_next(), if !connections.is_empty()=>{},
            }
        }
    });
    Harness {
        address,
        listener,
        applications,
        task,
        accepts,
    }
}
async fn pair(h: &mut Harness, tcp: bool) -> (SessionChannel, SessionChannel) {
    let mut client = connect(h.address).await.unwrap();
    client
        .send(&Msg::Hello(pb::Hello {
            system: vnt_proto::system::local() as i32,
            color_depth: 24,
            desktop_effects: Some(Default::default()),
            capabilities: vnt_proto::platform_capabilities(),
            proto_ver: 1,
            transport: if tcp { 2 } else { 1 },
            ..Default::default()
        }))
        .await
        .unwrap();
    let mut server = h.applications.recv().await.unwrap();
    assert!(matches!(server.recv().await.unwrap(), Some(Msg::Hello(_))));
    let challenge = tokio::spawn(async move {
        crate::auth::challenge(&mut server).await.unwrap();
        server
    });
    crate::auth::respond(&mut client, "test-password")
        .await
        .unwrap();
    let mut server = challenge.await.unwrap();
    server
        .send(&Msg::Welcome(pb::Welcome {
            system: vnt_proto::system::local() as i32,
            host_edition: pb::HostEdition::Portable as i32,
            proto_ver: 1,
            session_id: 123,
            transport: if tcp { 2 } else { 1 },
            ..Default::default()
        }))
        .await
        .unwrap();
    let Msg::Welcome(w) = next(&mut client, |m| matches!(m, Msg::Welcome(_))).await else {
        unreachable!()
    };
    assert_eq!(w.session_token.len(), 32);
    next(
        &mut client,
        |m| matches!(m,Msg::TransportState(s) if s.kind==1 && s.available),
    )
    .await;
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==1 && s.available),
    )
    .await;
    if tcp {
        next(
            &mut client,
            |m| matches!(m,Msg::TransportState(s) if s.kind==2 && s.available),
        )
        .await;
        next_server(
            &mut server,
            |m| matches!(m,Msg::TransportState(s) if s.kind==2 && s.available),
        )
        .await;
    }
    (client, server)
}
#[tokio::test]
async fn tcp_session_uses_three_sockets_and_routes_files_media_and_control() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, true).await;
    assert_eq!(h.accepts.load(Ordering::Relaxed), 3);
    server
        .send(&Msg::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms: 1,
            data: vec![1; 100],
        }))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut client, |m| matches!(m, Msg::VideoFrame(_))).await,
        Msg::VideoFrame(_)
    ));
    server
        .send(&Msg::ClipboardReply(pb::ClipboardReply {
            offer_id: 1,
            operation_id: 1,
            request_id: 1,
            data: vec![3; 32],
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut client, |m| matches!(m, Msg::ClipboardReply(_))).await,
        Msg::ClipboardReply(_)
    ));
    client
        .send(&Msg::Ping(pb::Ping { timestamp_ms: 7 }))
        .await
        .unwrap();
    assert!(
        matches!(next_server(&mut server,|m|matches!(m,Msg::Ping(_))).await,Msg::Ping(p) if p.timestamp_ms==7)
    );
}
#[tokio::test]
async fn control_recovery_preserves_session_token_and_healthy_file_socket() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, false).await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    let secret = record.secret;
    for generation in 0..2 {
        record
            .sender
            .send(Event::Down {
                kind: CONTROL,
                generation,
            })
            .await
            .unwrap();
        next_server(
            &mut server,
            |m| matches!(m,Msg::TransportState(s) if s.kind==0 && !s.available),
        )
        .await;
        server
            .send(&Msg::ClipboardReply(pb::ClipboardReply {
                offer_id: 1,
                operation_id: 1,
                request_id: generation as u32 + 1,
                ..Default::default()
            }))
            .await
            .unwrap();
        next(&mut client, |m| matches!(m, Msg::ClipboardReply(_))).await;
        next(
            &mut client,
            |m| matches!(m,Msg::TransportState(s) if s.kind==0 && s.available),
        )
        .await;
        next_server(
            &mut server,
            |m| matches!(m,Msg::TransportState(s) if s.kind==0 && s.available),
        )
        .await;
        assert_eq!(
            h.listener.records.lock().unwrap().get(&123).unwrap().secret,
            secret
        );
        assert_eq!(h.accepts.load(Ordering::Relaxed), 3 + generation as usize);
    }
    assert!(
        h.applications.try_recv().is_err(),
        "recovery must not allocate another application session"
    );
}
#[tokio::test]
async fn file_failure_reconnects_only_file_channel() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, false).await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    record
        .sender
        .send(Event::Down {
            kind: FILES,
            generation: 1,
        })
        .await
        .unwrap();
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==1 && !s.available),
    )
    .await;
    next(
        &mut client,
        |m| matches!(m,Msg::TransportState(s) if s.kind==1 && s.available && s.generation==2),
    )
    .await;
    assert_eq!(h.accepts.load(Ordering::Relaxed), 3);
}
#[tokio::test]
async fn image_chunks_wait_for_receiver_ready_on_the_control_lane() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, false).await;
    server
        .send(&Msg::ClipboardImage(pb::ClipboardImage {
            image_id: 9,
            width: 1,
            height: 1,
            total_bytes: 4,
        }))
        .await
        .unwrap();
    server
        .send(&Msg::ClipboardImageChunk(pb::ClipboardImageChunk {
            image_id: 9,
            offset: 0,
            data: vec![1; 4],
        }))
        .await
        .unwrap();
    next(&mut client, |m| matches!(m, Msg::ClipboardImage(_))).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(80), client.recv())
            .await
            .is_err()
    );
    client
        .send(&Msg::ClipboardImageReady(pb::ClipboardImageReady {
            image_id: 9,
            generation: 0,
        }))
        .await
        .unwrap();
    next(&mut client, |m| matches!(m, Msg::ClipboardImageChunk(_))).await;
}
#[tokio::test]
async fn missing_session_is_denied_without_a_password_challenge() {
    let h = harness().await;
    let mut wire = FramedChannel::connect(h.address).await.unwrap();
    let mut resume = packet(RESUME, CONTROL, 999);
    resume.attempt = vec![1; 16];
    wire.send(&Msg::SessionPacket(resume)).await.unwrap();
    assert!(matches!(receive(&mut wire).await.unwrap(), Msg::Denied(_)));
}
#[test]
fn message_whitelist_separates_lanes_and_rejects_local_events() {
    let frame = Msg::VideoFrame(pb::VideoFrame {
        dependencies: None,
        keyframe: true,
        pts_ms: 0,
        data: vec![1],
    });
    assert!(allowed(&frame, MEDIA, true));
    assert!(!allowed(&frame, CONTROL, true));
    assert!(!allowed(&frame, MEDIA, false));
    assert!(!allowed(&Msg::ResetInput(pb::ResetInput {}), CONTROL, true));
    assert!(!allowed(
        &Msg::TransportState(pb::TransportState::default()),
        CONTROL,
        true
    ));
}

#[tokio::test]
async fn control_recovery_expires_after_thirty_seconds_and_revokes_token() {
    let mut h = harness().await;
    let (client, mut server) = pair(&mut h, false).await;
    drop(client);
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==0 && !s.available),
    )
    .await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(29)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(h.listener.records.lock().unwrap().contains_key(&123));
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert!(!h.listener.records.lock().unwrap().contains_key(&123));
    let end = next_server(&mut server, |m| matches!(m, Msg::SessionEnd(_))).await;
    assert!(matches!(end,Msg::SessionEnd(e) if e.reason.contains("30")));
}
#[tokio::test]
async fn expired_or_wrong_role_binding_does_not_disrupt_the_existing_session() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, false).await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    for (kind, expired, wrong) in [
        (FILES, true, false),
        (FILES, false, true),
        (MEDIA, false, false),
    ] {
        record.grants.lock().unwrap().insert(
            FILES,
            Grant {
                token: [7; 32],
                generation: 2,
                expires: Instant::now()
                    + if expired {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(10)
                    },
            },
        );
        let mut wire = FramedChannel::connect(h.address).await.unwrap();
        let mut bind = packet(BIND, kind, 123);
        bind.generation = 2;
        bind.credential = vec![if wrong { 8 } else { 7 }; 32];
        wire.send(&Msg::SessionPacket(bind)).await.unwrap();
        assert!(matches!(wire.recv().await, Ok(None) | Err(_)));
    }
    client
        .send(&Msg::Ping(pb::Ping { timestamp_ms: 99 }))
        .await
        .unwrap();
    next_server(
        &mut server,
        |m| matches!(m,Msg::Ping(p) if p.timestamp_ms==99),
    )
    .await;
}
#[tokio::test]
async fn saturated_file_receive_queue_does_not_block_control_or_media() {
    let mut h = harness().await;
    let (mut client, server) = pair(&mut h, true).await;
    let (_input, mut output) = server.into_split();
    let mut files = output.fork().unwrap();
    let flood = tokio::spawn(async move {
        for request_id in 1..300 {
            files
                .send(&Msg::ClipboardReply(pb::ClipboardReply {
                    offer_id: 1,
                    operation_id: 1,
                    request_id,
                    data: vec![0; 32 * 1024],
                    ..Default::default()
                }))
                .await
                .unwrap();
        }
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    output
        .send(&Msg::Pong(pb::Pong { timestamp_ms: 17 }))
        .await
        .unwrap();
    output
        .send(&Msg::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms: 1,
            data: vec![1; 100],
        }))
        .await
        .unwrap();
    // Do not read the file lane while waiting for control and media.
    next(
        &mut client,
        |m| matches!(m,Msg::Pong(p) if p.timestamp_ms==17),
    )
    .await;
    next(&mut client, |m| matches!(m, Msg::VideoFrame(_))).await;
    flood.abort();
}

#[tokio::test]
async fn binding_token_is_one_use_and_wrong_session_cannot_consume_it() {
    let mut h = harness().await;
    let (_client, mut server) = pair(&mut h, false).await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    record.grants.lock().unwrap().insert(
        FILES,
        Grant {
            token: [5; 32],
            generation: 2,
            expires: Instant::now() + Duration::from_secs(10),
        },
    );
    let mut wrong = FramedChannel::connect(h.address).await.unwrap();
    let mut bind = packet(BIND, FILES, 124);
    bind.generation = 2;
    bind.credential = vec![5; 32];
    wrong.send(&Msg::SessionPacket(bind.clone())).await.unwrap();
    assert!(matches!(receive(&mut wrong).await.unwrap(), Msg::Denied(_)));
    assert!(record.grants.lock().unwrap().contains_key(&FILES));
    bind.session_id = 123;
    let mut attached = FramedChannel::connect(h.address).await.unwrap();
    attached
        .send(&Msg::SessionPacket(bind.clone()))
        .await
        .unwrap();
    assert!(
        matches!(receive(&mut attached).await.unwrap(),Msg::SessionPacket(p) if p.op==ATTACHED)
    );
    assert!(!record.grants.lock().unwrap().contains_key(&FILES));
    let mut replay = FramedChannel::connect(h.address).await.unwrap();
    replay.send(&Msg::SessionPacket(bind)).await.unwrap();
    assert!(matches!(replay.recv().await, Ok(None) | Err(_)));
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==1 && s.available && s.generation==2),
    )
    .await;
}

#[tokio::test]
async fn failed_control_proof_leaves_existing_control_connection_alive() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, false).await;
    let mut wire = FramedChannel::connect(h.address).await.unwrap();
    let mut request = packet(RESUME, CONTROL, 123);
    request.attempt = vec![1; 16];
    wire.send(&Msg::SessionPacket(request.clone()))
        .await
        .unwrap();
    assert!(matches!(receive(&mut wire).await.unwrap(),Msg::SessionPacket(p) if p.op==CHALLENGE));
    request.op = PROOF;
    request.credential = vec![0; 32];
    wire.send(&Msg::SessionPacket(request)).await.unwrap();
    assert!(matches!(wire.recv().await, Ok(None) | Err(_)));
    client
        .send(&Msg::Ping(pb::Ping { timestamp_ms: 51 }))
        .await
        .unwrap();
    next_server(
        &mut server,
        |m| matches!(m,Msg::Ping(p) if p.timestamp_ms==51),
    )
    .await;
}

#[tokio::test]
async fn media_failure_reconnects_only_media_and_replays_configuration_before_keyframe() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, true).await;
    server
        .send(&Msg::StreamReset(pb::StreamReset {
            width: 64,
            height: 48,
            fps: 30,
            decoder_config: vec![],
        }))
        .await
        .unwrap();
    next(&mut client, |m| matches!(m, Msg::StreamReset(_))).await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    record
        .sender
        .send(Event::Down {
            kind: MEDIA,
            generation: 1,
        })
        .await
        .unwrap();
    next(
        &mut client,
        |m| matches!(m,Msg::TransportState(s) if s.kind==2 && !s.available),
    )
    .await;
    next(
        &mut client,
        |m| matches!(m,Msg::TransportState(s) if s.kind==2 && s.available && s.generation==2),
    )
    .await;
    next(&mut client, |m| matches!(m, Msg::StreamReset(_))).await;
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==2 && s.available && s.generation==2),
    )
    .await;
    server
        .send(&Msg::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms: 50,
            data: vec![1; 100],
        }))
        .await
        .unwrap();
    assert!(
        matches!(next(&mut client,|m|matches!(m,Msg::VideoFrame(_))).await,Msg::VideoFrame(f) if f.keyframe && f.pts_ms==50)
    );
    assert_eq!(h.accepts.load(Ordering::Relaxed), 4);
}
#[tokio::test]
async fn snapshot_changes_during_recovery_require_a_new_acknowledgement() {
    let mut h = harness().await;
    let (mut client, mut server) = pair(&mut h, false).await;
    server
        .send(&Msg::DesktopState(pb::DesktopState {
            ready: true,
            desktop: "default".into(),
            generation: 1,
            ..Default::default()
        }))
        .await
        .unwrap();
    next(&mut client, |m| matches!(m, Msg::DesktopState(_))).await;
    let record = h
        .listener
        .records
        .lock()
        .unwrap()
        .get(&123)
        .unwrap()
        .clone();
    record
        .sender
        .send(Event::Down {
            kind: CONTROL,
            generation: 0,
        })
        .await
        .unwrap();
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==0 && !s.available),
    )
    .await;
    let first = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(Msg::TransportState(s)) = client.recv().await.unwrap() {
                if s.synchronizing {
                    break s;
                }
            }
        }
    })
    .await
    .unwrap();
    server
        .send(&Msg::DesktopState(pb::DesktopState {
            ready: false,
            desktop: "winlogon".into(),
            generation: 2,
            ..Default::default()
        }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut ack = packet(SYNC_ACK, CONTROL, 123);
    ack.generation = first.generation;
    ack.state_revision = first.state_revision;
    client.send(&Msg::SessionPacket(ack.clone())).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(3), async {
        let mut applied = false;
        loop {
            match client.recv().await.unwrap() {
                Some(Msg::DesktopState(s)) if s.generation == 2 => applied = true,
                Some(Msg::TransportState(s)) if s.synchronizing => {
                    assert!(applied);
                    break s;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(second.state_revision > first.state_revision);
    assert!(record.deadline.lock().unwrap().is_some());
    ack.state_revision = second.state_revision;
    client.send(&Msg::SessionPacket(ack)).await.unwrap();
    next(
        &mut client,
        |m| matches!(m,Msg::TransportState(s) if s.kind==0 && s.available),
    )
    .await;
    next_server(
        &mut server,
        |m| matches!(m,Msg::TransportState(s) if s.kind==0 && s.available),
    )
    .await;
    assert!(record.deadline.lock().unwrap().is_none());
}

#[tokio::test]
async fn paste_commands_are_not_queued_for_recovery() {
    let now = Instant::now();
    let (events, _events_rx) = mpsc::channel(4);
    let (app_tx, _app_rx) = mpsc::channel(4);
    let (writer, _receiver) = mpsc::channel(1);
    let frame = |pts_ms| {
        Msg::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms,
            data: vec![1],
        })
    };
    let mut supervisor = Supervisor {
        server: None,
        address: None,
        id: 1,
        secret: [0; 32],
        record: None,
        tcp_media: Arc::new(AtomicBool::new(true)),
        traffic: TrafficCounters::default(),
        events,
        app: std::array::from_fn(|_| app_tx.clone()),
        delivered_generations: std::array::from_fn(|_| Arc::new(AtomicU64::new(0))),
        links: [
            None,
            None,
            Some(Link {
                tx: writer,
                cancel: CancellationToken::new(),
                generation: 1,
            }),
        ],
        generations: [0, 0, 1],
        lost: [None; 3],
        next_retry: [now; 3],
        attempts: [0; 3],
        retrying: [false; 3],
        pending: [None, None, Some(frame(2))],
        requests: Default::default(),
        awaiting: Default::default(),
        snapshots: Default::default(),
        snapshot_revision: 0,
        sync_revision: 0,
        ready_image: None,
        media_config: Default::default(),
        screen_snapshots: Default::default(),
        screen_waiting: Default::default(),
        open_screens: None,
        media_needs_keyframe: false,
        tasks: JoinSet::new(),
        last_control_rx: now,
        last_heartbeat: now,
        last_app_status: now,
    };
    supervisor.lost[CONTROL] = Some(now);
    let paste = Msg::ClipboardPaste(pb::ClipboardPaste {
        request_id: 1,
        text: "never replay".into(),
    });
    assert!(supervisor.outgoing(paste.clone(), CONTROL).await.unwrap());
    assert!(supervisor
        .outgoing(vnt_proto::screen::wrap(1, 1, paste), CONTROL)
        .await
        .unwrap());
    assert!(supervisor.pending[CONTROL].is_none());
    assert!(supervisor.snapshots.is_empty());
}
