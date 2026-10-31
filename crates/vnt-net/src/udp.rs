//! UDP 视频通道。
//!
//! - 服务端 [`UdpVideoSender`]：完整帧 → 分片 → 定向发送（目标地址来自 UdpBind）
//! - 客户端 [`UdpVideoReceiver`]：绑定本地端口 → 发送 UdpBind → 重组收帧

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::datagram::Datagram as UdpSocket;
use crate::traffic::TrafficCounters;

use vnt_proto::control::encode_udp_bind_frame;
use vnt_proto::udp::{
    frame_fragments, ReassembledFrame, Reassembler, ReassemblyStats, DEFAULT_MTU, FRAG_HEADER_LEN,
};

/// 单次 recv 缓冲：协议内数据报不会超过 `FRAG_HEADER_LEN + DEFAULT_MTU`
/// （发送侧按 DEFAULT_MTU 分片，音频/绑定帧更小）。不按 64KB 巨帧预留：
/// 超限的杂包被截断后解析失败即忽略，不影响协议路径。
const MAX_PACKET: usize = FRAG_HEADER_LEN + DEFAULT_MTU + 8;
// Routed video follows the base header with u32 stream_id + u32 generation.
// Bit one belongs to audio; bit two identifies this eight-byte extension.
const FLAG_SCREEN: u8 = 0x04;

/// 内核 UDP socket 缓冲（收/发同值）：一整帧上限 + 全部分片协议头的余量。
/// 即使启用限时重传，缓冲仍至少要容得下一次完整的
/// 最大帧突发；接收任务持续 drain，缓冲只需覆盖调度抖动，无需再翻倍。
const VIDEO_SOCKET_BUFFER: usize =
    vnt_proto::udp::MAX_FRAME_BYTES + vnt_proto::udp::MAX_FRAGS * FRAG_HEADER_LEN;

/// 收包等待上限：到点就推进一次重组器的超时判定。稳态下包源源不断，
/// 这个超时几乎不会触发（只在画面静止/丢包后的空档生效），因此不增加
/// 常规路径的开销，却保证完整帧不会压在已判失的旧帧后面发不出去。
const RECV_POLL: Duration = vnt_proto::recovery::POLL;

const BIND_KEEPALIVE: Duration = Duration::from_secs(10);

/// Configure the shared server socket as well as standalone video senders.
pub fn configure_video_send_socket(socket: &UdpSocket) -> io::Result<()> {
    socket.configure_send_buffer(VIDEO_SOCKET_BUFFER)
}

/// Bounded packet bursts, with a byte-rate deadline carried across frame boundaries.
/// This is a peak-rate limiter; link feedback still controls the encoder bitrate.
pub struct UdpFrameSender {
    _timer: crate::timing::TimerResolution,
    packet: Vec<u8>,
    repair: Option<crate::repair::VideoRepair>,
    fec: Option<crate::fec::Sender>,
    next_batch: tokio::time::Instant,
    batch_bytes: usize,
}

/// Allocate only when a frame starts sending. A partial/failed send consumes a
/// number, but frames discarded locally before sending never look like loss.
#[derive(Default)]
pub struct VideoSequence(u16);

impl VideoSequence {
    pub fn take(&mut self) -> u16 {
        let seq = self.0;
        self.0 = self.0.wrapping_add(1);
        seq
    }
}

// Four ms of wire bytes, capped at 128 KiB. A fixed 16 KiB cap lets the
// scheduler's 1-2 ms wakeup overhead dominate and throttles high-bitrate video.
const MAX_SEND_BURST_BYTES: usize = 128 * 1024;

fn send_burst_budget(peak_kbps: u32) -> usize {
    MAX_SEND_BURST_BYTES.min((peak_kbps.max(100) as usize / 2).max(127))
}

impl Default for UdpFrameSender {
    fn default() -> Self {
        Self {
            _timer: crate::timing::TimerResolution::acquire(),
            fec: None,
            repair: None,
            packet: Vec::with_capacity(FRAG_HEADER_LEN + DEFAULT_MTU),
            next_batch: tokio::time::Instant::now(),
            batch_bytes: 0,
        }
    }
}

impl UdpFrameSender {
    pub fn set_repair(&mut self, repair: Option<crate::repair::VideoRepair>) {
        self.repair = repair;
    }

    pub async fn send_video_frame(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        session_id: u32,
        frame_seq: u16,
        frame: &vnt_proto::pb::VideoFrame,
        peak_kbps: u32,
    ) -> io::Result<usize> {
        self.send_routed_frame(
            socket,
            target,
            session_id,
            frame_seq,
            frame.keyframe,
            frame.pts_ms,
            &frame.data,
            DEFAULT_MTU,
            peak_kbps,
            None,
            frame.dependencies.as_ref(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn send_encoded_frame(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        session_id: u32,
        frame_seq: u16,
        keyframe: bool,
        pts_ms: u32,
        data: &[u8],
        dependencies: Option<&vnt_proto::pb::VideoDependencies>,
        peak_kbps: u32,
    ) -> io::Result<usize> {
        self.send_routed_frame(
            socket,
            target,
            session_id,
            frame_seq,
            keyframe,
            pts_ms,
            data,
            DEFAULT_MTU,
            peak_kbps,
            None,
            dependencies,
        )
        .await
    }
    pub fn set_fec(&mut self, fec: Option<crate::fec::Sender>) {
        self.fec = fec;
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn send_frame(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        session_id: u32,
        frame_seq: u16,
        keyframe: bool,
        pts_ms: u32,
        data: &[u8],
        mtu: usize,
        peak_kbps: u32,
    ) -> io::Result<usize> {
        self.send_routed_frame(
            socket, target, session_id, frame_seq, keyframe, pts_ms, data, mtu, peak_kbps, None,
            None,
        )
        .await
    }
    pub async fn send_screen_frame(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        session_id: u32,
        frame_seq: u16,
        packet: &vnt_proto::pb::ScreenPacket,
        peak_kbps: u32,
    ) -> io::Result<usize> {
        let Some(vnt_proto::control::ControlMessage::VideoFrame(frame)) =
            vnt_proto::screen::unwrap(packet)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid screen frame",
            ));
        };
        self.send_routed_frame(
            socket,
            target,
            session_id,
            frame_seq,
            frame.keyframe,
            frame.pts_ms,
            &frame.data,
            DEFAULT_MTU - 8,
            peak_kbps,
            Some((packet.stream_id, packet.generation)),
            frame.dependencies.as_ref(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_routed_frame(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        session_id: u32,
        frame_seq: u16,
        keyframe: bool,
        pts_ms: u32,
        data: &[u8],
        mtu: usize,
        peak_kbps: u32,
        route: Option<(u32, u32)>,
        metadata: Option<&vnt_proto::pb::VideoDependencies>,
    ) -> io::Result<usize> {
        // Keep inter-burst gaps below the receiver's 20 ms quiet deadline, even
        // at low adaptive bitrates. Shrink fragments when one MTU takes >4 ms.
        let peak_kbps = peak_kbps.max(100);
        let wire_budget = (peak_kbps as usize / 2).max(127); // four ms of link bytes
        let minimum_mtu = data.len().div_ceil(vnt_proto::udp::MAX_FRAGS).max(64);
        let mtu = mtu
            .min(socket.packet_limit().saturating_sub(FRAG_HEADER_LEN + 8))
            .clamp(64, DEFAULT_MTU)
            .min(
                wire_budget
                    .saturating_sub(FRAG_HEADER_LEN + 48)
                    .max(minimum_mtu),
            );
        let burst_bytes = send_burst_budget(peak_kbps);
        let mtu = if self.fec.is_some() {
            mtu.saturating_sub(crate::fec::OVERHEAD).max(minimum_mtu)
        } else {
            mtu
        };
        let metadata_bytes = if let Some(d) = metadata {
            if !vnt_proto::recovery::valid_dependencies(d) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid video dependencies",
                ));
            }
            let mut encoded = Vec::new();
            vnt_proto::recovery::encode_dependencies(d, &mut encoded);
            encoded
        } else {
            Vec::new()
        };
        let mtu = mtu.saturating_sub(metadata_bytes.len()).max(minimum_mtu);
        let fragments = frame_fragments(session_id, frame_seq, keyframe, pts_ms, data, mtu)
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidInput, reason))?;
        let key = crate::fec::Key {
            stream: route.map_or(0, |r| r.0),
            generation: route.map_or(0, |r| r.1),
            audio: false,
        };
        let mut count = 0;
        let mut block = if self.fec.is_some() {
            Vec::with_capacity(32)
        } else {
            Vec::new()
        };
        let mut fragments = fragments.peekable();
        while let Some((header, payload)) = fragments.next() {
            let mut encoded = [0; FRAG_HEADER_LEN];
            header.encode_to(&mut encoded);
            if route.is_some() {
                encoded[10] |= FLAG_SCREEN;
            }
            if metadata.is_some() {
                encoded[10] |= vnt_proto::recovery::FLAG_DEPENDENCIES;
            }
            self.packet.clear();
            self.packet.extend_from_slice(&encoded);
            if let Some((id, generation)) = route {
                self.packet.extend_from_slice(&id.to_be_bytes());
                self.packet.extend_from_slice(&generation.to_be_bytes());
            }
            self.packet.extend_from_slice(&metadata_bytes);
            self.packet.extend_from_slice(payload);
            if let (Some(repair), Some(d)) = (&self.repair, metadata) {
                repair.cache(
                    vnt_proto::recovery::Route {
                        stream: key.stream,
                        generation: key.generation,
                    },
                    d,
                    header,
                    &self.packet,
                    target,
                    peak_kbps,
                );
            }
            if let Some(fec) = self.fec.clone() {
                block.push(self.packet.clone());
                if block.len() == 32 || fragments.peek().is_none() {
                    let packets = fec.video(
                        session_id,
                        key,
                        std::mem::replace(&mut block, Vec::with_capacity(32)),
                    );
                    for packet in packets {
                        self.packet = packet;
                        self.send_current(socket, target, peak_kbps, burst_bytes)
                            .await?;
                        count += 1;
                    }
                }
            } else {
                self.send_current(socket, target, peak_kbps, burst_bytes)
                    .await?;
                count += 1;
            }
        }
        Ok(count)
    }
    async fn send_current(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        peak_kbps: u32,
        burst_bytes: usize,
    ) -> io::Result<()> {
        let now = tokio::time::Instant::now();
        let credit =
            Duration::from_secs_f64(burst_bytes as f64 * 8.0 / (peak_kbps as f64 * 1000.0));
        self.next_batch = self.next_batch.max(now - credit);
        let wire_bytes = self.packet.len() + 48;
        if self.batch_bytes + wire_bytes > burst_bytes {
            if let Some(repair) = &self.repair {
                repair.pace(self.batch_bytes, peak_kbps).await;
            }
            tokio::time::sleep_until(self.next_batch).await;
            self.batch_bytes = 0;
        }
        match tokio::time::timeout(
            Duration::from_millis(100),
            socket.send_to(&self.packet, target),
        )
        .await
        {
            Ok(Ok(_)) => {}
            result => {
                if let Some(fec) = &self.fec {
                    fec.pressure();
                }
                return match result {
                    Ok(Err(e)) => Err(e),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "UDP socket remained blocked for 100 ms",
                    )),
                };
            }
        }
        self.next_batch +=
            Duration::from_secs_f64(wire_bytes as f64 * 8.0 / (peak_kbps as f64 * 1000.0));
        self.batch_bytes += wire_bytes;
        Ok(())
    }
}

/// Send original audio immediately; parity is bounded to one datagram per block.
pub async fn send_audio(
    socket: &UdpSocket,
    target: SocketAddr,
    session: u32,
    frame: &vnt_proto::pb::AudioFrame,
    fec: Option<&crate::fec::Sender>,
) -> io::Result<()> {
    let Some(packet) = vnt_proto::audio::encode_datagram(session, frame) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid audio frame",
        ));
    };
    let packets = if let Some(fec) = fec {
        fec.audio(session, frame.generation, packet)
    } else {
        vec![packet]
    };
    for packet in packets {
        socket.send_to(&packet, target).await?;
    }
    Ok(())
}

pub struct UdpVideoSender {
    socket: UdpSocket,
    target: SocketAddr,
    mtu: usize,
    frame_seq: VideoSequence,
    sender: UdpFrameSender,
    peak_kbps: u32,
}

impl UdpVideoSender {
    pub fn new(socket: UdpSocket, target: SocketAddr, mtu: usize) -> Self {
        if let Err(error) = configure_video_send_socket(&socket) {
            log::warn!("UDP send buffer: {error}");
        }
        Self {
            socket,
            target,
            mtu: mtu.clamp(64, DEFAULT_MTU),
            frame_seq: VideoSequence::default(),
            sender: UdpFrameSender::default(),
            peak_kbps: 40_000,
        }
    }

    pub fn target(&self) -> SocketAddr {
        self.target
    }

    /// Set the peak wire rate (minimum 100 kbps, matching the video configuration).
    pub fn set_peak_kbps(&mut self, kbps: u32) {
        self.peak_kbps = kbps.max(100);
    }

    /// Send a complete frame using a reusable packet buffer and bounded bursts.
    pub async fn send_frame(
        &mut self,
        session_id: u32,
        keyframe: bool,
        pts_ms: u32,
        data: &[u8],
    ) -> io::Result<usize> {
        let seq = self.frame_seq.take();
        self.sender
            .send_frame(
                &self.socket,
                self.target,
                session_id,
                seq,
                keyframe,
                pts_ms,
                data,
                self.mtu,
                self.peak_kbps,
            )
            .await
    }
}

/// 客户端视频接收器。
pub struct UdpVideoReceiver {
    _timer: crate::timing::TimerResolution,
    recovery: vnt_proto::recovery::Receiver,
    recovery_tx: Option<tokio::sync::mpsc::Sender<vnt_proto::control::ControlMessage>>,
    fec: Option<crate::fec::Decoder>,
    fec_ready: std::collections::VecDeque<Vec<u8>>,
    socket: Arc<UdpSocket>,
    traffic: TrafficCounters,
    reassembler: Reassembler,
    screens: std::collections::BTreeMap<u32, (u32, Reassembler, ReassemblyStats)>,
    pub stats: ReassemblyStats,
    allowed_screens: Option<std::collections::HashSet<u32>>,
    server_addr: SocketAddr,
    buf: Vec<u8>,
    audio_tx: Option<tokio::sync::mpsc::Sender<vnt_proto::pb::AudioFrame>>,
}

impl UdpVideoReceiver {
    pub fn set_recovery_sender(
        &mut self,
        tx: tokio::sync::mpsc::Sender<vnt_proto::control::ControlMessage>,
    ) {
        self.recovery_tx = Some(tx);
    }
    pub fn set_rtt(&mut self, rtt: Option<u32>) {
        self.recovery.set_rtt(rtt);
    }
    pub fn recovery_stats(&self, stream: u32) -> vnt_proto::recovery::Stats {
        self.recovery.stats(stream)
    }
    pub fn recovery_diagnostics(
        &self,
    ) -> Vec<(vnt_proto::recovery::Route, vnt_proto::recovery::Stats)> {
        self.recovery.diagnostics()
    }
    fn recovery_tick(&mut self) {
        let now = std::time::Instant::now();
        self.recovery.advance(now);
        if let Some(tx) = &self.recovery_tx {
            for nack in self.recovery.nacks(now) {
                let _ = tx.try_send(vnt_proto::control::ControlMessage::VideoNack(nack));
            }
            for (route, reason) in self.recovery.idrs(now) {
                log::info!(
                    "UDP reference chain unavailable: stream={} generation={} reason={reason:?}",
                    route.stream,
                    route.generation
                );
                let msg = vnt_proto::control::ControlMessage::RequestKeyframe;
                let msg = if route.stream == 0 {
                    msg
                } else {
                    vnt_proto::screen::wrap(route.stream, route.generation, msg)
                };
                let _ = tx.try_send(msg);
            }
        }
        let stats = self.recovery.stats(0);
        self.stats.lost_frames = self.stats.lost_frames.max(stats.lost_frames as u32);
        self.stats.late_frags = self.stats.late_frags.max(stats.late_fragments as u32);
        self.stats.timed_out_frames = self
            .stats
            .timed_out_frames
            .max(stats.timed_out_frames as u32);
    }
    pub fn set_fec(&mut self, enabled: bool) {
        self.fec = enabled.then(crate::fec::Decoder::default);
        self.fec_ready.clear();
    }
    pub fn fec_reports(&mut self, stream: Option<u32>) -> Vec<vnt_proto::pb::FecFeedback> {
        self.fec
            .as_mut()
            .map_or_else(Vec::new, |f| f.take_reports(stream))
    }

    /// 绑定本地随机端口；`server_addr` 为被控端 UDP 地址（与 TCP 同端口）。
    pub async fn bind(server_addr: SocketAddr) -> io::Result<Self> {
        let local: &str = if server_addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(local).await?;
        // Windows' default UDP receive buffer cannot hold a detailed desktop IDR.
        // One dropped fragment discards the entire frame; requesting another IDR
        // then repeats the same burst and can leave the picture frozen forever.
        // Reserve room for the maximum encoded frame plus per-datagram overhead.
        Self::from_socket(socket, server_addr)
    }
    pub fn from_socket(socket: UdpSocket, server_addr: SocketAddr) -> io::Result<Self> {
        socket.configure_recv_buffer(VIDEO_SOCKET_BUFFER)?;
        Ok(Self {
            _timer: crate::timing::TimerResolution::acquire(),
            recovery: Default::default(),
            recovery_tx: None,
            fec: None,
            fec_ready: Default::default(),
            socket: Arc::new(socket),
            traffic: TrafficCounters::default(),
            reassembler: Reassembler::new(0),
            screens: Default::default(),
            stats: ReassemblyStats::default(),
            allowed_screens: None,
            server_addr,
            buf: vec![0u8; MAX_PACKET],
            audio_tx: None,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn set_traffic(&mut self, traffic: TrafficCounters) {
        self.traffic = traffic;
    }

    /// Audio packets are routed immediately, without entering video reassembly
    /// or updating video loss/watchdog statistics. A slow audio consumer drops.
    pub fn set_audio_sender(&mut self, tx: tokio::sync::mpsc::Sender<vnt_proto::pb::AudioFrame>) {
        self.audio_tx = Some(tx);
    }

    /// Welcome 收到后设定会话 id。
    pub fn set_session(&mut self, session_id: u32) {
        self.screens.clear();
        self.recovery.clear();
        if let Some(fec) = &mut self.fec {
            fec.clear();
        }
        self.fec_ready.clear();
        self.reassembler = Reassembler::new(session_id);
    }

    /// 重试发送 UdpBind（服务端收到即回 TCP UdpBindAck）。
    pub async fn send_bind(&mut self, session_id: u32, bind_token: &[u8]) -> io::Result<()> {
        // Direct QUIC media belongs to the authenticated connection already.
        if matches!(self.socket.as_ref(), UdpSocket::Quic(_)) {
            return Ok(());
        }
        self.socket
            .send_to(
                &encode_udp_bind_frame(session_id, bind_token),
                self.server_addr,
            )
            .await
            .map(|bytes| self.traffic.sent(bytes))
    }

    /// Keep the native UDP mapping alive, independently of video/audio reception.
    /// The caller owns and cancels this future with the remote session.
    pub fn binding_keepalive(
        &self,
        session_id: u32,
        bind_token: Vec<u8>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let socket = self.socket.clone();
        let target = self.server_addr;
        let traffic = self.traffic.clone();
        async move {
            // QUIC and VNT own their transport keepalives. Never send native
            // UDP bindings over either of those media transports.
            if !matches!(socket.as_ref(), UdpSocket::Udp(_)) {
                return;
            }
            let packet = encode_udp_bind_frame(session_id, &bind_token);
            let mut timer = tokio::time::interval_at(
                tokio::time::Instant::now() + BIND_KEEPALIVE,
                BIND_KEEPALIVE,
            );
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = timer.tick() => {}
                }
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    result = tokio::time::timeout(Duration::from_secs(1), socket.send_to(&packet, target)) => result,
                };
                match result {
                    Ok(Ok(bytes)) => traffic.sent(bytes),
                    Ok(Err(error)) => log::debug!("UDP binding keepalive failed: {error}"),
                    Err(_) => log::debug!("UDP binding keepalive timed out"),
                }
            }
        }
    }

    /// 阻塞收一个完整帧（内部丢帧不返回）。`Ok(None)` 表示无会话/被忽略包且无帧。
    ///
    /// 收包等待有上限：到点推进一次重组器的超时判定，把"完整帧被压在已判失
    /// 旧帧后面"的情况放出来（静止桌面前的最后一帧尤其需要这个）。
    pub fn screen_lost(&self, id: u32) -> u32 {
        self.screens
            .get(&id)
            .map_or(0, |(_, _, stats)| stats.lost_frames)
            .saturating_add(self.recovery.stats(id).lost_frames as u32)
    }
    pub fn retain_screens(&mut self, ids: &[u32]) {
        if let Some(fec) = &mut self.fec {
            fec.retain_screens(ids);
        }

        self.recovery.retain(ids);
        self.allowed_screens = Some(ids.iter().copied().collect());
        self.screens.retain(|id, _| ids.contains(id));
    }
    pub async fn recv_frame(&mut self) -> io::Result<Option<ReassembledFrame>> {
        Ok(self.recv_screen_frame().await?.map(|(_, _, frame)| frame))
    }
    pub async fn recv_screen_frame(&mut self) -> io::Result<Option<(u32, u32, ReassembledFrame)>> {
        loop {
            self.recovery_tick();
            if let Some((route, frame)) = self.recovery.pop() {
                return Ok(Some((route.stream, route.generation, frame)));
            }
            for (id, (generation, reassembler, stats)) in &mut self.screens {
                if let Some(frame) = reassembler.poll_expiry(stats) {
                    return Ok(Some((*id, *generation, frame)));
                }
            }
            // 一次 push 可同时放出多帧：先排空已完成帧，再等待新包。
            // 同时保证无效包持续到达时仍推进已有帧的超时判定。
            if let Some(frame) = self.reassembler.poll_expiry(&mut self.stats) {
                return Ok(Some((0, 0, frame)));
            }
            if let Some(fec) = &mut self.fec {
                fec.expire(std::time::Instant::now());
            }
            let len = if let Some(packet) = self.fec_ready.pop_front() {
                self.buf[..packet.len()].copy_from_slice(&packet);
                packet.len()
            } else {
                let recv =
                    tokio::time::timeout(RECV_POLL, self.socket.recv_from(&mut self.buf)).await;
                let (len, from) = match recv {
                    Ok(r) => r?,
                    Err(_) => continue,
                };
                if from != self.server_addr {
                    continue;
                }
                self.traffic.received(len);
                if len > 10 && self.buf[10] & crate::fec::FLAG != 0 {
                    if let Some(fec) = &mut self.fec {
                        self.fec_ready.extend(fec.push(
                            &self.buf[..len],
                            self.reassembler.session_id(),
                            std::time::Instant::now(),
                        ));
                    }
                    continue;
                }
                if self.fec.is_some()
                    && self
                        .buf
                        .get(10)
                        .is_none_or(|f| f & vnt_proto::recovery::FLAG_REPAIR == 0)
                {
                    continue;
                }
                len
            };
            if self.buf[..len]
                .get(10)
                .is_some_and(|flags| flags & vnt_proto::audio::FLAG_AUDIO != 0)
            {
                if let Some(frame) = vnt_proto::audio::decode_datagram(
                    &self.buf[..len],
                    self.reassembler.session_id(),
                ) {
                    if let Some(tx) = &self.audio_tx {
                        let _ = tx.try_send(frame);
                    }
                }
                continue;
            }
            if len >= FRAG_HEADER_LEN && self.buf[10] & vnt_proto::recovery::FLAG_DEPENDENCIES != 0
            {
                if let Some((h, mut payload)) = vnt_proto::udp::FragHeader::decode(&self.buf[..len])
                {
                    if h.session_id != self.reassembler.session_id()
                        || h.flags
                            & !(vnt_proto::udp::FLAG_KEYFRAME
                                | FLAG_SCREEN
                                | vnt_proto::recovery::FLAG_DEPENDENCIES
                                | vnt_proto::recovery::FLAG_REPAIR)
                            != 0
                    {
                        continue;
                    }
                    let route = if h.flags & FLAG_SCREEN != 0 {
                        if payload.len() < 8 {
                            continue;
                        }
                        let stream = u32::from_be_bytes(payload[..4].try_into().unwrap());
                        let generation = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                        if stream == 0
                            || stream > 128
                            || generation == 0
                            || self
                                .allowed_screens
                                .as_ref()
                                .is_some_and(|ids| !ids.contains(&stream))
                        {
                            continue;
                        }
                        payload = &payload[8..];
                        vnt_proto::recovery::Route { stream, generation }
                    } else {
                        vnt_proto::recovery::Route::default()
                    };
                    if let Some((meta, data)) = vnt_proto::recovery::decode_dependencies(payload) {
                        self.recovery
                            .push(route, h, meta, data, std::time::Instant::now());
                    }
                }
                continue;
            }
            if len >= FRAG_HEADER_LEN + 8 && self.buf[10] & FLAG_SCREEN != 0 {
                let id = u32::from_be_bytes(self.buf[15..19].try_into().unwrap());
                let generation = u32::from_be_bytes(self.buf[19..23].try_into().unwrap());
                if id == 0
                    || id > 128
                    || generation == 0
                    || self
                        .allowed_screens
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(&id))
                {
                    continue;
                }
                let session = self.reassembler.session_id();
                let entry = self.screens.entry(id).or_insert_with(|| {
                    (
                        generation,
                        Reassembler::new(session),
                        ReassemblyStats::default(),
                    )
                });
                if generation < entry.0 {
                    continue;
                }
                if generation > entry.0 {
                    *entry = (
                        generation,
                        Reassembler::new(session),
                        ReassemblyStats::default(),
                    );
                }
                self.buf.copy_within(23..len, 15);
                self.buf[10] &= !FLAG_SCREEN;
                if let Some(frame) = entry.1.push(&self.buf[..len - 8], &mut entry.2) {
                    return Ok(Some((id, generation, frame)));
                }
                continue;
            }
            if let Some(frame) = self.reassembler.push(&self.buf[..len], &mut self.stats) {
                return Ok(Some((0, 0, frame)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vnt_proto::udp::fragment_frame;

    #[tokio::test(start_paused = true)]
    async fn binding_keepalive_uses_the_media_socket_during_streaming_and_idle_and_stops() {
        // Keep simulated time under explicit control while real loopback I/O runs.
        let _clock_guard = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }));
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut receiver = UdpVideoReceiver::bind(server.local_addr().unwrap())
            .await
            .unwrap();
        receiver.set_session(9);
        let port = receiver.local_addr().unwrap().port();
        let traffic = receiver.traffic.clone();
        receiver.send_bind(9, &[7; 16]).await.unwrap();
        let mut buffer = [0; 256];
        let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
        let expected = encode_udp_bind_frame(9, &[7; 16]);
        assert_eq!(&buffer[..len], expected);
        assert_eq!(peer.port(), port);
        let cancel = tokio_util::sync::CancellationToken::new();
        let keepalive = tokio::spawn(receiver.binding_keepalive(9, vec![7; 16], cancel.clone()));
        tokio::task::yield_now().await;
        for sequence in 0..10 {
            let data = vec![sequence as u8; 100];
            let mut packets = Vec::new();
            fragment_frame(
                9,
                sequence,
                true,
                sequence as u32,
                &data,
                1200,
                &mut packets,
            )
            .unwrap();
            for packet in packets {
                server.send_to(&packet, peer).await.unwrap();
            }
            assert_eq!(receiver.recv_frame().await.unwrap().unwrap().data, data);
            assert_eq!(
                server.try_recv_from(&mut buffer).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let (len, from) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(from, peer);
        assert_eq!(&buffer[..len], expected);
        // No incoming frames are needed to trigger the next uplink keepalive.
        tokio::time::advance(BIND_KEEPALIVE).await;
        let (len, from) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(from, peer);
        assert_eq!(&buffer[..len], expected);
        assert_eq!(traffic.snapshot().0, 3 * expected.len() as u64);
        cancel.cancel();
        keepalive.await.unwrap();
        tokio::time::advance(BIND_KEEPALIVE).await;
        assert_eq!(
            server.try_recv_from(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(receiver);
        tokio::net::UdpSocket::bind(("0.0.0.0", port))
            .await
            .expect("cancelled keepalive retained the media socket");
    }

    #[tokio::test]
    async fn routed_frames_keep_generations_and_wrapping_sequences_independent() {
        use vnt_proto::{control::ControlMessage as Msg, pb, screen};
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        let target = SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let mut sender = UdpFrameSender::default();
        let packet = |id, generation, value| {
            let Msg::ScreenPacket(p) = screen::wrap(
                id,
                generation,
                Msg::VideoFrame(pb::VideoFrame {
                    dependencies: None,
                    keyframe: true,
                    pts_ms: value as u32,
                    data: vec![value; 3000],
                }),
            ) else {
                unreachable!()
            };
            p
        };
        for (id, generation, seq, value) in [
            (1, 2, u16::MAX, 11),
            (2, 1, u16::MAX, 22),
            (1, 1, 0, 99),
            (1, 2, 0, 33),
            (2, 1, 0, 44),
        ] {
            sender
                .send_screen_frame(&tx, target, 7, seq, &packet(id, generation, value), 4000)
                .await
                .unwrap();
        }
        for (id, generation, value) in [(1, 2, 11), (2, 1, 22), (1, 2, 33), (2, 1, 44)] {
            let actual = tokio::time::timeout(Duration::from_secs(2), rx.recv_screen_frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!((actual.0, actual.1), (id, generation));
            assert_eq!(actual.2.data, vec![value; 3000]);
        }
        assert_eq!(rx.screen_lost(1), 0);
        assert_eq!(rx.screen_lost(2), 0);
    }

    #[test]
    fn send_sequence_wraps_and_only_allocates_for_attempted_frames() {
        let mut seq = VideoSequence(u16::MAX);
        assert_eq!(seq.take(), u16::MAX); // also consumed by a partial failure
                                          // Locally skipped encoded frames do not touch this counter.
        assert_eq!(seq.take(), 0);
        assert_eq!(seq.take(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn pacing_keeps_absolute_deadlines_and_bounds_credit_after_a_stall() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut sender = UdpFrameSender::default();
        let started = tokio::time::Instant::now();
        sender
            .send_frame(
                &socket,
                target.local_addr().unwrap(),
                7,
                0,
                true,
                0,
                &[0; 14_000],
                DEFAULT_MTU,
                4000,
            )
            .await
            .unwrap();
        // Ten packets at 1463 wire bytes each: exactly 29.26 ms of debt,
        // irrespective of the millisecond rounding of each Tokio sleep.
        assert_eq!(sender.next_batch - started, Duration::from_micros(29_260));
        tokio::time::advance(Duration::from_secs(1)).await;
        let resumed = tokio::time::Instant::now();
        sender
            .send_frame(
                &socket,
                target.local_addr().unwrap(),
                7,
                1,
                false,
                33,
                &[0; 14_000],
                DEFAULT_MTU,
                4000,
            )
            .await
            .unwrap();
        assert!(sender.next_batch >= resumed + Duration::from_millis(25));
        assert!(tokio::time::Instant::now() >= resumed + Duration::from_millis(20));
    }

    #[tokio::test]
    async fn sustained_alternating_frames_with_idr_bursts_keep_full_rate_without_loss() {
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        let target = SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let mut sender = UdpVideoSender::new(tx, target, DEFAULT_MTU);
        sender.set_peak_kbps(16_000);
        let send = async move {
            let mut clock = tokio::time::interval(Duration::from_micros(33_333));
            clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            for seq in 0..90 {
                clock.tick().await;
                let size = if seq % 30 == 0 {
                    128 * 1024
                } else if seq % 2 == 0 {
                    40_000
                } else {
                    8_000
                };
                sender
                    .send_frame(7, seq % 30 == 0, seq * 33, &vec![seq as u8; size])
                    .await
                    .unwrap();
            }
        };
        let receive = async move {
            for seq in 0..90u16 {
                let frame = rx.recv_frame().await.unwrap().unwrap();
                assert_eq!(frame.frame_seq, seq);
                assert!(frame.data.iter().all(|&byte| byte == seq as u8));
            }
            assert_eq!(rx.stats.lost_frames, 0);
        };
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(send, receive);
        })
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(3500));
    }

    #[test]
    fn batch_budget_keeps_encoder_headroom_despite_two_ms_of_wakeup_overhead() {
        // The peak is 2x encoder bitrate. Even with two ms of timer overhead
        // per batch, the pacing schedule must still sustain the encoder rate.
        // A fixed 16 KiB budget fails this at 200-400 Mbps peaks.
        for peak_kbps in [100, 200, 4000, 40_000, 100_000, 200_000, 400_000] {
            let bytes = send_burst_budget(peak_kbps) as f64;
            let serialization = bytes * 8.0 / (peak_kbps as f64 * 1000.0);
            let effective_bps = bytes * 8.0 / (serialization + 0.002);
            assert!(
                effective_bps >= peak_kbps as f64 * 1000.0 / 2.0,
                "pacing throttles encoder rate at peak {peak_kbps} kbps"
            );
        }
    }

    #[tokio::test]
    async fn paced_large_frame_spans_batches_without_reassembly_expiry() {
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        let target = SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let mut sender = UdpVideoSender::new(tx, target, DEFAULT_MTU);
        sender.set_peak_kbps(4000);
        let data = vec![23; 100_000];
        let start = std::time::Instant::now();
        let (sent, received) = tokio::join!(
            sender.send_frame(7, true, 0, &data),
            tokio::time::timeout(Duration::from_secs(3), rx.recv_frame())
        );
        assert!(sent.unwrap() > 1);
        assert_eq!(received.unwrap().unwrap().unwrap().data, data);
        assert!(
            start.elapsed() >= Duration::from_millis(180),
            "frame was sent as an unpaced burst"
        );
        assert_eq!(rx.stats.lost_frames, 0);
    }

    #[tokio::test]
    async fn low_bitrate_shrinks_fragments_to_keep_the_partial_frame_alive() {
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        let target = SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let mut sender = UdpVideoSender::new(tx, target, DEFAULT_MTU);
        sender.set_peak_kbps(200);
        let data = vec![11; 3000];
        let (sent, received) = tokio::join!(
            sender.send_frame(7, true, 0, &data),
            tokio::time::timeout(Duration::from_secs(3), rx.recv_frame())
        );
        assert!(sent.unwrap() > data.len().div_ceil(DEFAULT_MTU));
        assert_eq!(received.unwrap().unwrap().unwrap().data, data);
        assert_eq!(rx.stats.lost_frames, 0);
    }

    #[tokio::test]
    async fn multiplexed_audio_is_counted_but_never_enters_video_reassembly() {
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        let traffic = TrafficCounters::default();
        rx.set_traffic(traffic.clone());
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(6);
        rx.set_audio_sender(audio_tx);
        let target = SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let audio = vnt_proto::pb::AudioFrame {
            generation: 1,
            sequence: 0,
            timestamp_samples: 0,
            data: vec![42; 80],
        };
        let datagram = vnt_proto::audio::encode_datagram(7, &audio).unwrap();
        tx.send_to(&datagram, target).await.unwrap();
        let mut packets = Vec::new();
        fragment_frame(7, 0, true, 0, &[1; 30], 1400, &mut packets).unwrap();
        tx.send_to(&packets[0], target).await.unwrap();
        let video = tokio::time::timeout(Duration::from_secs(1), rx.recv_frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(video.data, vec![1; 30]);
        assert_eq!(audio_rx.try_recv().unwrap(), audio);
        assert_eq!(rx.stats.lost_frames, 0);
        assert_eq!(
            traffic.snapshot().1,
            (datagram.len() + packets[0].len()) as u64
        );
    }

    #[tokio::test]
    async fn already_reassembled_frame_is_delivered_without_waiting_for_network() {
        let mut rx = UdpVideoReceiver::bind("127.0.0.1:29875".parse().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        let mut first = Vec::new();
        let mut second = Vec::new();
        fragment_frame(7, 0, true, 0, &[1; 200], 100, &mut first).unwrap();
        fragment_frame(7, 1, false, 33, &[2], 100, &mut second).unwrap();
        // Completing the older frame releases both frames in one push.
        rx.reassembler.push(&first[0], &mut rx.stats);
        rx.reassembler.push(&second[0], &mut rx.stats);
        assert_eq!(
            rx.reassembler
                .push(&first[1], &mut rx.stats)
                .unwrap()
                .frame_seq,
            0
        );
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let mut receive = std::pin::pin!(rx.recv_frame());
        let Poll::Ready(Ok(Some(frame))) = receive
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("ready frames must not wait for network activity");
        };
        assert_eq!(frame.frame_seq, 1);
        assert_eq!(frame.data, vec![2]);
    }

    #[tokio::test]
    async fn desktop_keyframe_burst_survives_before_receiver_is_scheduled() {
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(9);
        let target = SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let mut sender = UdpVideoSender::new(tx, target, DEFAULT_MTU);
        // A detailed 4K desktop IDR is much larger than a small delta. The receiver
        // may be busy decoding/rendering when this burst reaches the kernel.
        let data = vec![42; 512 * 1024];
        sender.send_frame(9, true, 1000, &data).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv_frame())
            .await
            .expect("desktop keyframe burst was lost")
            .unwrap()
            .unwrap();
        assert_eq!(frame.data, data);
        assert_eq!(rx.stats.lost_frames, 0);
    }

    #[tokio::test]
    async fn send_recv_lossless() {
        let tx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = UdpVideoReceiver::bind(tx_sock.local_addr().unwrap())
            .await
            .unwrap();
        let traffic = TrafficCounters::default();
        rx.set_traffic(traffic.clone());
        let rx_port = rx.local_addr().unwrap().port();
        let rx_addr: SocketAddr = format!("127.0.0.1:{rx_port}").parse().unwrap();
        let mut sender = UdpVideoSender::new(tx_sock, rx_addr, 1200);

        rx.set_session(9);
        rx.send_bind(9, &[7; 16]).await.unwrap();
        assert_eq!(
            traffic.snapshot().0,
            encode_udp_bind_frame(9, &[7; 16]).len() as u64
        );
        let mut expected_received = 0;
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let packets = sender.send_frame(9, true, 111, &data).await.unwrap();
        expected_received += (data.len() + packets * FRAG_HEADER_LEN) as u64;
        let frame = rx.recv_frame().await.unwrap().expect("应收到完整帧");
        assert!(frame.keyframe);
        assert_eq!(frame.pts_ms, 111);
        assert_eq!(frame.data, data);
        assert_eq!(traffic.snapshot().1, expected_received);

        // 多帧
        for k in 1..=5u8 {
            let d = vec![k; 3000];
            let packets = sender
                .send_frame(9, false, 200 + k as u32, &d)
                .await
                .unwrap();
            expected_received += (d.len() + packets * FRAG_HEADER_LEN) as u64;
            let f = rx.recv_frame().await.unwrap().expect("帧丢失");
            assert_eq!(f.data, d);
            assert_eq!(traffic.snapshot().1, expected_received);
        }
        assert_eq!(rx.stats.lost_frames, 0);
    }
}
