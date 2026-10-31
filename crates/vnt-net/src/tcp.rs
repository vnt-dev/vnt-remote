//! TCP 控制通道：异步收发 [`ControlMessage`]。

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::traffic::TrafficCounters;
use vnt_proto::control::{decode_message, parse_header, ControlMessage, HEADER_LEN};

/// 可续读的帧读取器。
///
/// 关键性质：`recv_msg()` 被取消（如外层 `timeout`）时，**已读进度保留在
/// 结构体内**，下次调用从断点续读。早期实现把 header/body 放在局部变量，
/// 超时取消后这些字节永久丢失，TCP 流边界被破坏（报"未知消息类型 0"）。
struct Framer<R> {
    reader: R,
    traffic: TrafficCounters,
    hdr: [u8; HEADER_LEN],
    hdr_filled: usize,
    body: Vec<u8>,
    body_filled: usize,
}

impl<R: tokio::io::AsyncRead + Unpin> Framer<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            traffic: TrafficCounters::default(),
            hdr: [0u8; HEADER_LEN],
            hdr_filled: 0,
            body: Vec::new(),
            body_filled: 0,
        }
    }

    async fn recv_msg(&mut self) -> io::Result<Option<ControlMessage>> {
        // 1) 读满帧头（支持断点续读）
        while self.hdr_filled < HEADER_LEN {
            let n = self.reader.read(&mut self.hdr[self.hdr_filled..]).await?;
            self.traffic.received(n);
            if n == 0 {
                if self.hdr_filled == 0 {
                    return Ok(None); // 干净关闭（帧边界上）
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "帧头未读满即 EOF",
                ));
            }
            self.hdr_filled += n;
        }
        let (ty, len) = parse_header(&self.hdr)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        // 2) 读满 body（同样可续读）
        if self.body.len() != len {
            // 每条消息按自身长度精确分配：读完即随 decode 释放，不在消息
            // 之间保留历史帧的大容量（视频走 TCP 时一帧可达数 MB，保留即
            // 常驻）。同尺寸的反复分配/释放对分配器是稳态的。
            self.body = vec![0u8; len];
            self.body_filled = 0;
        }
        while self.body_filled < len {
            let n = self.reader.read(&mut self.body[self.body_filled..]).await?;
            self.traffic.received(n);
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "帧体未读满即 EOF",
                ));
            }
            self.body_filled += n;
        }

        // 3) 完成：解码并复位进度
        let msg = decode_message(ty, &self.body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.hdr_filled = 0;
        self.body_filled = 0;
        Ok(Some(msg))
    }
}

/// A framed byte stream for IPC, or a message-oriented managed remote session.
/// The session backend does not allocate a fake socket or multiplex bulk data
/// through a single application byte stream.
pub struct FramedChannel<S = TcpStream> {
    backend: ChannelBackend<S>,
}
enum ChannelBackend<S> {
    Stream(Framer<S>),
    Session(crate::session::Endpoint),
}
pub type ControlChannel<S = TcpStream> = FramedChannel<S>;

impl FramedChannel<TcpStream> {
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Self::new(stream))
    }
    pub fn peer_addr(&self) -> SocketAddr {
        match &self.backend {
            ChannelBackend::Stream(f) => f
                .reader
                .peer_addr()
                .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()),
            ChannelBackend::Session(_) => {
                unreachable!("managed session has no single socket address")
            }
        }
    }
}
impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> FramedChannel<S> {
    pub fn new(stream: S) -> Self {
        Self {
            backend: ChannelBackend::Stream(Framer::new(stream)),
        }
    }
    /// Independent producer used by the Broker's file forwarding task.
    pub fn fork_writer(&self) -> Option<TcpWriter<S>> {
        match &self.backend {
            ChannelBackend::Session(endpoint) => Some(TcpWriter {
                backend: WriteBackend::Session(endpoint.outgoing.clone()),
                traffic: endpoint.traffic.clone(),
            }),
            _ => None,
        }
    }
    pub(crate) fn use_traffic(&mut self, traffic: TrafficCounters) {
        if let ChannelBackend::Stream(framer) = &mut self.backend {
            let (sent, received) = framer.traffic.snapshot();
            traffic.sent(sent as usize);
            traffic.received(received as usize);
            framer.traffic = traffic;
        }
    }
    pub async fn send_owned(&mut self, message: ControlMessage) -> io::Result<()> {
        match &mut self.backend {
            ChannelBackend::Session(endpoint) => endpoint.send(message).await,
            ChannelBackend::Stream(f) => send_counted(&mut f.reader, &f.traffic, &message).await,
        }
    }
    pub async fn send(&mut self, message: &ControlMessage) -> io::Result<()> {
        match &mut self.backend {
            ChannelBackend::Stream(f) => send_counted(&mut f.reader, &f.traffic, message).await,
            ChannelBackend::Session(endpoint) => endpoint.send(message.clone()).await,
        }
    }
    pub fn traffic(&self) -> TrafficCounters {
        match &self.backend {
            ChannelBackend::Stream(f) => f.traffic.clone(),
            ChannelBackend::Session(endpoint) => endpoint.traffic.clone(),
        }
    }
    pub async fn recv(&mut self) -> io::Result<Option<ControlMessage>> {
        match &mut self.backend {
            ChannelBackend::Stream(f) => f.recv_msg().await,
            ChannelBackend::Session(endpoint) => Ok(endpoint.incoming.recv().await),
        }
    }
    pub fn into_split(
        self,
    ) -> (
        TcpReader<tokio::io::ReadHalf<S>>,
        TcpWriter<tokio::io::WriteHalf<S>>,
    ) {
        match self.backend {
            ChannelBackend::Session(endpoint) => (
                TcpReader {
                    backend: ReadBackend::Session(endpoint.incoming),
                },
                TcpWriter {
                    backend: WriteBackend::Session(endpoint.outgoing),
                    traffic: endpoint.traffic,
                },
            ),
            ChannelBackend::Stream(Framer {
                reader,
                traffic,
                hdr,
                hdr_filled,
                body,
                body_filled,
            }) => {
                let (read, write) = tokio::io::split(reader);
                (
                    TcpReader {
                        backend: ReadBackend::Stream(Framer {
                            reader: read,
                            traffic: traffic.clone(),
                            hdr,
                            hdr_filled,
                            body,
                            body_filled,
                        }),
                    },
                    TcpWriter {
                        backend: WriteBackend::Stream(write),
                        traffic,
                    },
                )
            }
        }
    }
}
pub struct TcpReader<R = tokio::io::ReadHalf<TcpStream>> {
    backend: ReadBackend<R>,
}
enum ReadBackend<R> {
    Stream(Framer<R>),
    Session(crate::session::SessionReceiver),
}
impl<R: tokio::io::AsyncRead + Unpin> TcpReader<R> {
    pub fn into_message_lanes(self) -> Result<[crate::session::MessageLane; 3], Self> {
        match self.backend {
            ReadBackend::Session(receiver) => Ok(receiver.into_lanes()),
            backend => Err(Self { backend }),
        }
    }
    pub async fn recv(&mut self) -> io::Result<Option<ControlMessage>> {
        match &mut self.backend {
            ReadBackend::Stream(f) => f.recv_msg().await,
            ReadBackend::Session(receiver) => Ok(receiver.recv().await),
        }
    }
}
pub struct TcpWriter<W = tokio::io::WriteHalf<TcpStream>> {
    backend: WriteBackend<W>,
    traffic: TrafficCounters,
}
enum WriteBackend<W> {
    Stream(W),
    Session(crate::session::Outgoing),
}
impl<W: tokio::io::AsyncWrite + Unpin> TcpWriter<W> {
    pub fn traffic(&self) -> TrafficCounters {
        self.traffic.clone()
    }
    /// Fork only a managed message producer, never a physical stream writer.
    pub fn fork(&self) -> Option<Self> {
        match &self.backend {
            WriteBackend::Session(sender) => Some(Self {
                backend: WriteBackend::Session(sender.clone()),
                traffic: self.traffic.clone(),
            }),
            _ => None,
        }
    }
    pub async fn send_owned(&mut self, message: ControlMessage) -> io::Result<()> {
        match &mut self.backend {
            WriteBackend::Session(sender) => sender.send(message).await,
            WriteBackend::Stream(stream) => send_counted(stream, &self.traffic, &message).await,
        }
    }
    pub async fn send(&mut self, message: &ControlMessage) -> io::Result<()> {
        match &mut self.backend {
            WriteBackend::Stream(stream) => send_counted(stream, &self.traffic, message).await,
            WriteBackend::Session(sender) => sender.send(message.clone()).await,
        }
    }
}

async fn send_counted<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    traffic: &TrafficCounters,
    msg: &ControlMessage,
) -> io::Result<()> {
    let frame = msg.encode_frame();
    let mut remaining = frame.as_slice();
    while !remaining.is_empty() {
        let n = writer.write(remaining).await?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        // Count each completed write even if a later write is cancelled or fails.
        traffic.sent(n);
        remaining = &remaining[n..];
    }
    match msg {
        ControlMessage::VideoFrame(frame) => traffic.video_sent(frame.data.len()),
        ControlMessage::Agent(message) => {
            if let Some(vnt_proto::pb::agent_message::Payload::Video(frame)) = &message.payload {
                traffic.video_sent(frame.data.len());
            }
        }
        _ => {}
    }
    Ok(())
}

impl FramedChannel<tokio::io::DuplexStream> {
    pub(crate) fn managed(endpoint: crate::session::Endpoint) -> Self {
        Self {
            backend: ChannelBackend::Session(endpoint),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use vnt_proto::control::encode_udp_bind_frame;
    use vnt_proto::pb;

    #[tokio::test]
    async fn varying_messages_allocate_exactly_and_release_between_messages() {
        let messages: Vec<_> = [32_000, 16_000, 24_000, 600_000, 8_000]
            .into_iter()
            .map(|len| {
                ControlMessage::VideoFrame(pb::VideoFrame {
                    dependencies: None,
                    keyframe: true,
                    pts_ms: len as u32,
                    data: vec![3; len],
                })
            })
            .collect();
        let wire: Vec<u8> = messages
            .iter()
            .flat_map(ControlMessage::encode_frame)
            .collect();
        let mut framer = Framer::new(std::io::Cursor::new(wire));
        framer.recv_msg().await.unwrap();
        let first_len = framer.body.len();
        let first = framer.body.as_ptr();
        assert_eq!(framer.body.capacity(), first_len, "按消息长度精确分配");
        framer.recv_msg().await.unwrap();
        // 每条消息独立分配，不复用上一条的存储
        assert_ne!(framer.body.as_ptr(), first);
        assert_eq!(framer.body.capacity(), framer.body.len());
        framer.recv_msg().await.unwrap();
        framer.recv_msg().await.unwrap();
        assert!(framer.body.len() > 500_000);
        assert_eq!(framer.body.capacity(), framer.body.len());
        framer.recv_msg().await.unwrap();
        // 历史大帧的容量不驻留：下一条小消息按自身长度分配
        assert!(framer.body.capacity() < 16_000);
    }

    #[tokio::test]
    async fn only_complete_video_writes_count_as_sent_frames() {
        let (mut tx, _blocked_rx) = tokio::io::duplex(10);
        let traffic = TrafficCounters::default();
        let video = ControlMessage::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms: 0,
            data: vec![1; 100],
        });
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            send_counted(&mut tx, &traffic, &video)
        )
        .await
        .is_err());
        assert_eq!(traffic.video_snapshot(), (0, 0));
        let mut bytes = Vec::new();
        send_counted(&mut bytes, &traffic, &video).await.unwrap();
        assert_eq!(traffic.video_snapshot(), (1, 100));
    }

    #[tokio::test]
    async fn echo_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ch = ControlChannel::new(sock);
            while let Some(message) = ch.recv().await.unwrap() {
                ch.send(&message).await.unwrap();
            }
        });

        let mut client = ControlChannel::connect(addr).await.unwrap();
        let msgs = vec![
            ControlMessage::Ping(vnt_proto::pb::Ping { timestamp_ms: 7 }),
            ControlMessage::RequestKeyframe,
            ControlMessage::InputEvent(pb::InputEvent {
                ev: Some(pb::input_event::Ev::Key(pb::KeyEvent {
                    scancode: 0x1E,
                    extended: false,
                    down: true,
                })),
            }),
        ];
        for m in &msgs {
            client.send(m).await.unwrap();
            let back = client.recv().await.unwrap().unwrap();
            assert_eq!(back.msg_type(), m.msg_type());
        }
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn traffic_counts_files_video_and_control_across_split() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut channel = ControlChannel::new(sock);
            while let Some(message) = channel.recv().await.unwrap() {
                channel.send(&message).await.unwrap();
            }
        });
        let mut channel = ControlChannel::connect(addr).await.unwrap();
        let traffic = channel.traffic();
        let ping = ControlMessage::Ping(pb::Ping { timestamp_ms: 7 });
        channel.send(&ping).await.unwrap();
        channel.recv().await.unwrap().unwrap();
        let mut expected = ping.encode_frame().len() as u64;
        assert_eq!(traffic.snapshot(), (expected, expected));
        let (mut reader, mut writer) = channel.into_split();
        for message in [
            ControlMessage::ClipboardReply(pb::ClipboardReply {
                offer_id: 1,
                operation_id: 2,
                request_id: 3,
                data: vec![42; 32768],
                ..Default::default()
            }),
            ControlMessage::VideoFrame(pb::VideoFrame {
                data: vec![7; 200_000],
                ..Default::default()
            }),
            ControlMessage::RequestKeyframe,
        ] {
            writer.send(&message).await.unwrap();
            reader.recv().await.unwrap().unwrap();
            expected += message.encode_frame().len() as u64;
            assert_eq!(traffic.snapshot(), (expected, expected));
        }
        // Another connection never contributes to this connection's counters.
        assert_eq!(TrafficCounters::default().snapshot(), (0, 0));
        drop(reader);
        drop(writer);
        server.await.unwrap();
    }

    /// 回归：分两段发送的帧（间隔 2.3s）必须完整收到——
    /// 早期 recv 用局部变量缓存已读数据，外层超时取消后数据丢失，
    /// 双方报"未知消息类型 0"并断开。
    #[tokio::test(flavor = "current_thread")]
    async fn split_frame_across_timeout_is_intact() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let frame = ControlMessage::Ping(vnt_proto::pb::Ping { timestamp_ms: 77 }).encode_frame();

        let writer = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // 先发帧头，等 2.3s 再发 body（跨越客户端的 2s 轮询超时）
            sock.write_all(&frame[..5]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2300)).await;
            sock.write_all(&frame[5..]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut ch = ControlChannel::connect(addr).await.unwrap();
        // 外层 2s 超时轮询（模拟客户端存活检查）：第一次必超时，
        // 已读的帧头不得丢失
        let mut got = None;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(2), ch.recv()).await {
                Ok(Ok(Some(m))) => {
                    got = Some(m);
                    break;
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => panic!("recv 错误: {e}"),
                Err(_) => continue, // 超时：续读进度必须保留
            }
        }
        writer.await.unwrap();
        match got.expect("分段帧应完整收到") {
            ControlMessage::Ping(p) => assert_eq!(p.timestamp_ms, 77),
            other => panic!("期望 Ping，得到 {other:?}"),
        }
    }

    /// 回归：into_split 后取消 recv，续读进度必须随读半部转移。
    #[tokio::test(flavor = "current_thread")]
    async fn split_reader_resumes_after_cancel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 用带 body 的消息（Ping）：帧头先到、body 后到，
        // 第一次 recv 必然只读完帧头就被取消
        let frame = ControlMessage::VideoFrame(vnt_proto::pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms: 99,
            data: vec![7; 200_000],
        })
        .encode_frame();

        let writer = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&frame[..123]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            sock.write_all(&frame[123..]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let ch = ControlChannel::connect(addr).await.unwrap();
        let traffic = ch.traffic();
        let (mut reader, _writer) = ch.into_split();
        // 第一次 recv 在帧头到达后、body 未到时被取消
        let _ = tokio::time::timeout(Duration::from_millis(30), reader.recv()).await;
        assert_eq!(traffic.snapshot(), (0, 123));
        // 第二次必须从断点续收到完整消息
        let msg = tokio::time::timeout(Duration::from_secs(2), reader.recv())
            .await
            .expect("续读不应超时")
            .expect("连接错误")
            .expect("应收到消息");
        match msg {
            ControlMessage::VideoFrame(p) => {
                assert_eq!(p.pts_ms, 99);
                assert_eq!(p.data, vec![7; 200_000]);
            }
            other => panic!("期望 Ping，得到 {other:?}"),
        }
        assert_eq!(
            traffic.snapshot().1,
            ControlMessage::VideoFrame(pb::VideoFrame {
                dependencies: None,
                keyframe: true,
                pts_ms: 99,
                data: vec![7; 200_000],
            })
            .encode_frame()
            .len() as u64
        );
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn clean_eof() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut ch = ControlChannel::connect(addr).await.unwrap();
            ch.recv().await.unwrap()
        });
        let (mut a, _) = listener.accept().await.unwrap();
        a.shutdown().await.unwrap();
        assert!(client.await.unwrap().is_none());
    }

    #[test]
    fn udp_bind_frame() {
        let frame = encode_udp_bind_frame(42, &[0xC0; 16]);
        assert_eq!(
            vnt_proto::control::decode_udp_bind_frame(&frame),
            Some((42, vec![0xC0; 16]))
        );
        assert_eq!(vnt_proto::control::decode_udp_bind_frame(&frame[..3]), None);
    }
}
