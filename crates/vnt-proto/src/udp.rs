//! UDP 视频通道：帧分片与重组。
//!
//! 重组策略：允许少量帧同时在途（跨帧乱序容忍窗口）。后一帧首包提前
//! 到达时，旧帧分片仍能被收齐并按序交付，不会因"新帧已到"被整帧放弃；
//! 旧帧长时间没有新分片（严格安静上限）或窗口被挤满时才判丢失。
//!
//! 分片包格式（定长 15 字节头 + payload，统一大端）：
//! ```text
//! session_id  u32
//! frame_seq   u16   递增（回绕）
//! frag_idx    u16   0..frag_count-1
//! frag_count  u16   总分片数（u16：1400B/片可支撑约 91MB 大帧）
//! flags       u8    bit0 = keyframe
//! pts_ms      u32   会话相对时间戳
//! ```

/// UDP 分片头长度。
pub const FRAG_HEADER_LEN: usize = 15;

/// 单包 payload 建议值（局域网 MTU 1500 - IP/UDP 头 - 分片头）。
pub const DEFAULT_MTU: usize = 1400;

/// 单帧分片数上限（u16 头）。
pub const MAX_FRAGS: usize = u16::MAX as usize;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Hard cap on an encoded frame, independent of fragment count.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub const FLAG_KEYFRAME: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragHeader {
    pub session_id: u32,
    pub frame_seq: u16,
    pub frag_idx: u16,
    pub frag_count: u16,
    pub flags: u8,
    pub pts_ms: u32,
}

impl FragHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn encode_to(&self, out: &mut [u8]) {
        out[..4].copy_from_slice(&self.session_id.to_be_bytes());
        out[4..6].copy_from_slice(&self.frame_seq.to_be_bytes());
        out[6..8].copy_from_slice(&self.frag_idx.to_be_bytes());
        out[8..10].copy_from_slice(&self.frag_count.to_be_bytes());
        out[10] = self.flags;
        out[11..15].copy_from_slice(&self.pts_ms.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<(FragHeader, &[u8])> {
        if buf.len() < FRAG_HEADER_LEN {
            return None;
        }
        let h = FragHeader {
            session_id: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            frame_seq: u16::from_be_bytes([buf[4], buf[5]]),
            frag_idx: u16::from_be_bytes([buf[6], buf[7]]),
            frag_count: u16::from_be_bytes([buf[8], buf[9]]),
            flags: buf[10],
            pts_ms: u32::from_be_bytes([buf[11], buf[12], buf[13], buf[14]]),
        };
        if h.frag_count == 0 || h.frag_idx >= h.frag_count {
            return None;
        }
        Some((h, &buf[FRAG_HEADER_LEN..]))
    }
}

/// 把一个完整视频帧切成若干 UDP 包。
///
/// 每个追加的 Vec 已含分片头。超过帧大小或分片数量上限时返回错误，
/// 并保持 `out` 不变；不得静默截断视频帧。
pub fn fragment_frame(
    session_id: u32,
    frame_seq: u16,
    keyframe: bool,
    pts_ms: u32,
    frame: &[u8],
    mtu: usize,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), &'static str> {
    for (header, payload) in frame_fragments(session_id, frame_seq, keyframe, pts_ms, frame, mtu)? {
        let mut encoded = [0; FRAG_HEADER_LEN];
        header.encode_to(&mut encoded);
        let mut pkt = Vec::with_capacity(FRAG_HEADER_LEN + payload.len());
        pkt.extend_from_slice(&encoded);
        pkt.extend_from_slice(payload);
        out.push(pkt);
    }
    Ok(())
}

/// Borrow payloads from the encoded frame; the network sender reuses one packet buffer.
pub fn frame_fragments(
    session_id: u32,
    frame_seq: u16,
    keyframe: bool,
    pts_ms: u32,
    frame: &[u8],
    mtu: usize,
) -> Result<impl Iterator<Item = (FragHeader, &[u8])>, &'static str> {
    let mtu = mtu.clamp(64, DEFAULT_MTU);
    let count = frame.len().div_ceil(mtu).max(1);
    if frame.len() > MAX_FRAME_BYTES || count > MAX_FRAGS {
        return Err("frame exceeds UDP fragmentation limits");
    }
    Ok((0..count).map(move |idx| {
        let start = idx * mtu;
        let end = ((idx + 1) * mtu).min(frame.len());
        (
            FragHeader {
                session_id,
                frame_seq,
                frag_idx: idx as u16,
                frag_count: count as u16,
                flags: if keyframe { FLAG_KEYFRAME } else { 0 },
                pts_ms,
            },
            &frame[start..end],
        )
    }))
}

/// 重组输出的一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    pub keyframe: bool,
    pub pts_ms: u32,
    pub frame_seq: u16,
    pub data: Vec<u8>,
}

/// 重组过程中的丢包统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    /// 因后续帧先到而被放弃的未完成帧数。
    pub lost_frames: u32,
    /// 迟到的旧分片个数。
    pub late_frags: u32,
    /// Partial frames retired after no fragment made progress.
    pub timed_out_frames: u32,
}

/// 同时在途重组的帧数上限。跨帧乱序（后一帧首包提前）时，旧帧分片仍能
/// 收齐并按序交付；窗口被挤满时从最旧的开始放弃。
pub const REORDER_WINDOW: usize = 3;

/// 最旧未完成帧的"安静"上限：超过这么久没有新分片到达即判丢失。
/// 它同时是两个旋钮的上限——乱序容忍窗口，以及丢包时最晚的换帧时间。
pub const FRAME_QUIET: Duration = Duration::from_millis(20);

/// 一帧的重组状态。
#[derive(Debug)]
struct Slot {
    seq: u16,
    frag_count: u16,
    present: Vec<bool>,
    data: Vec<u8>,
    stride: Option<usize>,
    // The final fragment can precede every full-sized fragment.
    tail: Option<Vec<u8>>,
    received: u32,
    keyframe: bool,
    pts_ms: u32,
    bytes: usize,
    /// 最近一个分片到达时刻（安静判定基准：持续有片到达就不算卡住）
    last_frag: Instant,
}

impl Slot {
    fn is_complete(&self) -> bool {
        self.received == self.frag_count as u32
    }

    fn into_frame(self) -> ReassembledFrame {
        let mut data = self.data;
        data.truncate(self.bytes);
        ReassembledFrame {
            keyframe: self.keyframe,
            pts_ms: self.pts_ms,
            frame_seq: self.seq,
            data,
        }
    }
}

/// 单流重组器：容忍跨帧乱序与丢片；按帧序号顺序交付。
///
/// 交付顺序即编码顺序（P 帧参考前一帧），因此只有当**所有更旧的帧**都已
/// 交付或判失后，较新的完整帧才会交出。乱序到达的旧帧分片会在窗口内开槽
/// 并插到正确位置，不会因为"更新的帧已经开始"被丢弃。
#[derive(Debug)]
pub struct Reassembler {
    session_id: u32,
    /// 在途帧，按 seq 升序（乱序迟到会插在中间，故可能不连续）
    slots: VecDeque<Slot>,
    /// 序号跳过、窗口内尚未判失的帧（最多 REORDER_WINDOW - 1 条）
    missing: VecDeque<(u16, Instant)>,
    /// 已见的最大帧序号
    max_seq: Option<u16>,
    /// 下一个期望见到的**新帧**序号
    next_expected: Option<u16>,
    /// 下一个待交付序号：比它更旧的片已无秩序价值
    next_deliver: Option<u16>,
    quiet: Duration,
    /// open_slot 中判失、尚未计入 stats 的帧数（由 drain 统一结算）
    pending_lost: u32,
    /// 已凑齐、待调用方取走的帧（一次 push 可能凑齐多帧）
    ready: VecDeque<ReassembledFrame>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self {
            session_id: 0,
            slots: VecDeque::new(),
            missing: VecDeque::new(),
            max_seq: None,
            next_expected: None,
            next_deliver: None,
            quiet: FRAME_QUIET,
            pending_lost: 0,
            ready: VecDeque::new(),
        }
    }
}

impl Reassembler {
    pub fn new(session_id: u32) -> Self {
        Self {
            session_id,
            quiet: FRAME_QUIET,
            ..Default::default()
        }
    }

    /// 测试用：自定义安静上限（避免用例里真睡）。
    #[cfg(test)]
    fn with_quiet(session_id: u32, quiet: Duration) -> Self {
        Self {
            session_id,
            quiet,
            ..Default::default()
        }
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// `a` 比 `b` 新多少（u16 回绕语义）；`a` 不更新则 `None`。
    fn ahead(a: u16, b: u16) -> Option<u16> {
        let d = a.wrapping_sub(b);
        (d > 0 && d < 0x8000).then_some(d)
    }

    /// 喂入一个 UDP 包（含头）。属于其他会话的包直接忽略。
    ///
    /// 可能返回 `None`：本包只是某个在途帧的一片，或更旧的完整帧还没凑齐
    /// （按序交付）。无包到达时用 [`Reassembler::poll_expiry`] 推进超时判定。
    pub fn push(&mut self, packet: &[u8], stats: &mut ReassemblyStats) -> Option<ReassembledFrame> {
        let (h, payload) = FragHeader::decode(packet)?;
        if h.session_id != self.session_id {
            return None;
        }
        if h.flags & !FLAG_KEYFRAME != 0 {
            return None;
        }
        if payload.len() > DEFAULT_MTU || packet.len() > FRAG_HEADER_LEN + DEFAULT_MTU {
            return None;
        }
        // 已交付过更晚的帧：此片不可能再按序交付
        if let Some(nd) = self.next_deliver {
            if Self::ahead(nd, h.frame_seq).is_some() {
                stats.late_frags += 1;
                return None;
            }
        }
        // 太旧（窗口已滑过）：迟到片
        if let Some(max) = self.max_seq {
            if let Some(age) = Self::ahead(max, h.frame_seq) {
                if age >= REORDER_WINDOW as u16 {
                    stats.late_frags += 1;
                    return None;
                }
            }
        }

        if let Some(slot) = self.slots.iter_mut().find(|s| s.seq == h.frame_seq) {
            fill_frag(slot, &h, payload);
        } else {
            self.open_slot(&h, payload);
        }
        self.drain(Instant::now(), stats);
        self.ready.pop_front()
    }

    /// 推进超时/超容判定并取走已凑齐的帧。接收端在收包等待超时时应调用，
    /// 否则完整帧可能一直压在已判失的旧帧后面发不出去。
    pub fn poll_expiry(&mut self, stats: &mut ReassemblyStats) -> Option<ReassembledFrame> {
        self.drain(Instant::now(), stats);
        self.ready.pop_front()
    }

    /// 为新帧（或窗口内迟到的首片）开槽，并记录序号跳过的缺失帧。
    fn open_slot(&mut self, h: &FragHeader, payload: &[u8]) {
        let seq = h.frame_seq;
        let mut slot = Slot {
            seq,
            frag_count: h.frag_count,
            present: vec![false; h.frag_count as usize],
            data: Vec::new(),
            stride: None,
            tail: None,
            received: 0,
            keyframe: h.is_keyframe(),
            pts_ms: h.pts_ms,
            bytes: 0,
            last_frag: Instant::now(),
        };
        if !fill_frag(&mut slot, h, payload) {
            return;
        }
        // Only forward arrivals move the horizon. Retire missing entries here:
        // drain may be blocked by an older partial frame, and large repeated
        // jumps can otherwise accumulate stale entries across sequence wraps.
        let newer = match self.next_expected {
            None => true,
            Some(e) => seq == e || Self::ahead(seq, e).is_some(),
        };
        if newer {
            self.missing.retain(|&(missing, _)| {
                if seq.wrapping_sub(missing) >= REORDER_WINDOW as u16 {
                    self.pending_lost += 1;
                    false
                } else {
                    true
                }
            });
        }
        // 序号跳变：跳过且未见分片的帧先记入 missing。落在窗口外的部分已
        // 不可能到达，立即判失；窗口内的等它乱序迟到，或滑出窗口时再判失。
        if let Some(expected) = self.next_expected {
            if let Some(gap) = Self::ahead(seq, expected) {
                let keep = (REORDER_WINDOW - 1) as u16;
                let immediate = gap.saturating_sub(keep);
                self.pending_lost += immediate as u32;
                for i in immediate..gap {
                    let s = expected.wrapping_add(i);
                    // expected follows the newest arrival, so this new gap
                    // cannot overlap an existing missing sequence.
                    self.missing.push_back((s, Instant::now()));
                }
            }
        }
        // 只有"不旧于"期望序号时才推进（乱序迟到的旧帧不推进）
        if newer {
            self.next_expected = Some(seq.wrapping_add(1));
        }
        self.max_seq = Some(match self.max_seq {
            Some(m) if Self::ahead(m, seq).is_some() => m,
            _ => seq,
        });
        // 迟到的首片意味着该帧不再是"缺失"
        self.missing.retain(|&(s, _)| s != seq);

        // 按序号插入正确位置（乱序到达时插在中间）
        let pos = self
            .slots
            .iter()
            .position(|s| Self::ahead(s.seq, seq).is_some())
            .unwrap_or(self.slots.len());
        self.slots.insert(pos, slot);
    }

    /// 状态机：交付凑齐的最旧帧；放弃安静超时/挤占窗口的未凑齐帧。
    fn drain(&mut self, now: Instant, stats: &mut ReassemblyStats) {
        stats.lost_frames += self.pending_lost;
        self.pending_lost = 0;
        while let Some(front) = self.slots.front() {
            // A whole frame can arrive behind the next complete delta frame.
            // Keep that gap open for the same bounded reorder interval as a
            // partial frame. A complete keyframe can safely skip the gap.
            while let Some(&(seq, since)) = self.missing.front() {
                if Self::ahead(front.seq, seq).is_none() {
                    break;
                }
                let outside_window = self
                    .max_seq
                    .and_then(|max| Self::ahead(max, seq))
                    .is_some_and(|age| age >= REORDER_WINDOW as u16);
                if !(outside_window || front.is_complete() && front.keyframe)
                    && now.duration_since(since) < self.quiet
                {
                    return;
                }
                self.missing.pop_front();
                stats.lost_frames += 1;
                self.next_deliver = Some(seq.wrapping_add(1));
            }
            if front.is_complete() {
                let slot = self.slots.pop_front().expect("front exists");
                let seq = slot.seq;
                self.ready.push_back(slot.into_frame());
                self.next_deliver = Some(seq.wrapping_add(1));
                // 已越过该帧：更早的缺失帧不可能再按序交付，判失
                while let Some(&(m, _)) = self.missing.front() {
                    if Self::ahead(seq, m).is_some() {
                        self.missing.pop_front();
                        stats.lost_frames += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            let expired = now.duration_since(front.last_frag) >= self.quiet;
            let overflow = self.slots.len() > REORDER_WINDOW;
            if !expired && !overflow {
                break; // 最旧帧未凑齐但仍在等：保持顺序，等它的分片
            }
            let retired = self.slots.pop_front().expect("front exists");
            if expired {
                stats.timed_out_frames = stats.timed_out_frames.wrapping_add(1);
            }
            // 判失与成功交付一样推进水位，迟到分片不得重新开启已退休的帧。
            self.next_deliver = Some(retired.seq.wrapping_add(1));
            stats.lost_frames += 1;
        }
    }
}

/// 填入一个分片；元数据不一致或重复则忽略。
fn fill_frag(slot: &mut Slot, h: &FragHeader, payload: &[u8]) -> bool {
    let idx = h.frag_idx as usize;
    if h.frag_count != slot.frag_count
        || h.pts_ms != slot.pts_ms
        || h.is_keyframe() != slot.keyframe
    {
        return false;
    }
    if slot.bytes.saturating_add(payload.len()) > MAX_FRAME_BYTES {
        return false;
    }
    if idx >= slot.present.len() || slot.present[idx] {
        return false;
    }
    let last = idx + 1 == slot.frag_count as usize;
    if slot.frag_count == 1 {
        slot.data.extend_from_slice(payload);
    } else if let Some(stride) = slot.stride {
        if (!last && payload.len() != stride) || (last && payload.len() > stride) {
            return false;
        }
        let start = idx * stride;
        if start + payload.len() > MAX_FRAME_BYTES {
            return false;
        }
        slot.data[start..start + payload.len()].copy_from_slice(payload);
    } else if last {
        slot.tail = Some(payload.to_vec());
    } else {
        let stride = payload.len();
        let base = stride * (slot.frag_count as usize - 1);
        if stride == 0
            || base >= MAX_FRAME_BYTES
            || slot
                .tail
                .as_ref()
                .is_some_and(|tail| tail.len() > stride || base + tail.len() > MAX_FRAME_BYTES)
        {
            return false;
        }
        // Allocate once, copy incoming fragments directly to their final offsets.
        slot.data.resize((base + stride).min(MAX_FRAME_BYTES), 0);
        slot.data[idx * stride..(idx + 1) * stride].copy_from_slice(payload);
        if let Some(tail) = slot.tail.take() {
            slot.data[base..base + tail.len()].copy_from_slice(&tail);
        }
        slot.stride = Some(stride);
    }
    slot.present[idx] = true;
    slot.bytes += payload.len();
    slot.received += 1;
    slot.last_frag = Instant::now();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_reassembly_reuses_its_frame_allocation_with_tail_first_and_duplicates() {
        for mtu in [64, 1000, DEFAULT_MTU] {
            let data: Vec<u8> = (0..(mtu * 4 + 17)).map(|i| (i % 251) as u8).collect();
            let fragments = packets(0, true, &data, mtu);
            let mut r = Reassembler::new(7);
            let mut stats = ReassemblyStats::default();
            r.push(fragments.last().unwrap(), &mut stats);
            r.push(&fragments[1], &mut stats);
            let address = r.slots[0].data.as_ptr();
            r.push(&fragments[1], &mut stats); // duplicate must not complete early
            r.push(&fragments[3], &mut stats);
            r.push(&fragments[0], &mut stats);
            let frame = r.push(&fragments[2], &mut stats).unwrap();
            assert_eq!(
                frame.data.as_ptr(),
                address,
                "delivery must reuse the assembly allocation"
            );
            assert_eq!(frame.data, data);
            assert_eq!(stats.lost_frames, 0);
        }
    }

    #[test]
    fn maximum_size_frame_with_a_partial_tail_stays_inside_the_allocation_limit() {
        let data = vec![5; MAX_FRAME_BYTES];
        let fragments = packets(0, true, &data, DEFAULT_MTU);
        let mut r = Reassembler::with_quiet(7, Duration::from_secs(60));
        let mut stats = ReassemblyStats::default();
        for packet in fragments.iter().rev().take(fragments.len() - 1) {
            assert!(r.push(packet, &mut stats).is_none());
        }
        assert!(r.slots[0].data.len() <= MAX_FRAME_BYTES);
        assert_eq!(r.push(&fragments[0], &mut stats).unwrap().data, data);
    }

    #[test]
    fn invalid_first_fragment_does_not_advance_or_retire_the_reorder_window() {
        let mut r = Reassembler::with_quiet(7, Duration::from_secs(60));
        let mut stats = ReassemblyStats::default();
        let first = packets(0, true, &[42; 250], 100);
        assert!(r.push(&first[0], &mut stats).is_none());
        let header = FragHeader {
            session_id: 7,
            frame_seq: 100,
            frag_idx: 0,
            frag_count: u16::MAX,
            flags: 0,
            pts_ms: 1,
        };
        let mut invalid = vec![0; FRAG_HEADER_LEN + DEFAULT_MTU];
        header.encode_to(&mut invalid);
        assert!(r.push(&invalid, &mut stats).is_none());
        assert_eq!(r.max_seq, Some(0));
        assert_eq!(r.next_expected, Some(1));
        assert_eq!(stats.lost_frames, 0);
        assert_eq!(r.slots.len(), 1);
        r.push(&first[1], &mut stats);
        assert_eq!(r.push(&first[2], &mut stats).unwrap().data, vec![42; 250]);
    }

    #[test]
    fn malformed_fragment_lengths_do_not_poison_valid_reassembly() {
        let data = vec![42; 250];
        let fragments = packets(0, true, &data, 100);
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        r.push(&fragments[0], &mut stats);
        let mut short = fragments[1].clone();
        short.pop();
        assert!(r.push(&short, &mut stats).is_none());
        let mut oversized_tail = fragments[2].clone();
        oversized_tail.resize(FRAG_HEADER_LEN + 101, 0);
        assert!(r.push(&oversized_tail, &mut stats).is_none());
        r.push(&fragments[1], &mut stats);
        assert_eq!(r.push(&fragments[2], &mut stats).unwrap().data, data);
    }

    #[test]
    fn sequence_jump_flood_keeps_missing_state_within_the_reorder_window() {
        let mut r = Reassembler::with_quiet(7, Duration::from_secs(60));
        let mut stats = ReassemblyStats::default();
        let mut seq = 0u16;
        for _ in 0..10_000 {
            let frame = packets(seq, false, &[7; 128], 64);
            r.push(&frame[0], &mut stats);
            assert!(
                r.missing.len() < REORDER_WINDOW,
                "missing state grew to {} entries at sequence {seq}",
                r.missing.len()
            );
            seq = seq.wrapping_add(12_345);
        }
    }

    #[test]
    fn missing_frames_retire_once_while_partial_frames_block_delivery() {
        for base in [0u16, u16::MAX - 2] {
            let mut r = Reassembler::with_quiet(7, Duration::from_secs(60));
            let mut stats = ReassemblyStats::default();
            let seq = |offset| base.wrapping_add(offset);
            let first = packets(seq(0), true, &[0], 64);
            let second = packets(seq(2), false, &[2; 128], 64);
            let third = packets(seq(3), false, &[3], 64);
            let fourth = packets(seq(4), false, &[4; 128], 64);
            assert!(r.push(&first[0], &mut stats).is_some());
            assert!(r.push(&second[0], &mut stats).is_none());
            assert_eq!(stats.lost_frames, 0);
            assert!(r.push(&fourth[0], &mut stats).is_none());
            assert_eq!(stats.lost_frames, 1);
            assert_eq!(
                r.missing.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
                vec![seq(3)]
            );
            assert!(r.push(&third[0], &mut stats).is_none());
            assert_eq!(r.push(&second[1], &mut stats).unwrap().frame_seq, seq(2));
            assert_eq!(r.poll_expiry(&mut stats).unwrap().frame_seq, seq(3));
            assert_eq!(r.push(&fourth[1], &mut stats).unwrap().frame_seq, seq(4));
            assert!(r.missing.is_empty());
            assert_eq!(stats.lost_frames, 1);
        }
    }

    #[test]
    fn default_reassembler_keeps_incomplete_frames_until_the_reorder_deadline() {
        let mut receiver = Reassembler::default();
        let mut stats = ReassemblyStats::default();
        let mut fragments = Vec::new();
        fragment_frame(0, 0, true, 0, &[7; 128], 64, &mut fragments).unwrap();
        assert!(receiver.push(&fragments[0], &mut stats).is_none());
        assert_eq!(
            receiver.push(&fragments[1], &mut stats).unwrap().data,
            vec![7; 128]
        );
        assert_eq!(stats.lost_frames, 0);
    }

    #[test]
    fn whole_frame_reorder_preserves_delta_reference_order_including_wraparound() {
        for base in [0u16, u16::MAX - 1] {
            let mut r = Reassembler::new(7);
            let mut stats = ReassemblyStats::default();
            let first = packets(base, true, &[0], 64);
            let middle = packets(base.wrapping_add(1), false, &[1], 64);
            let last = packets(base.wrapping_add(2), false, &[2], 64);
            assert!(r.push(&first[0], &mut stats).is_some());
            assert!(r.push(&last[0], &mut stats).is_none());
            assert_eq!(r.push(&middle[0], &mut stats).unwrap().data, vec![1]);
            assert_eq!(r.poll_expiry(&mut stats).unwrap().data, vec![2]);
            assert_eq!(stats.lost_frames, 0);
        }
    }

    #[test]
    fn wholly_missing_frame_expires_without_stranding_the_next_delta() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        r.push(&packets(0, true, &[0], 64)[0], &mut stats);
        assert!(r
            .push(&packets(2, false, &[2], 64)[0], &mut stats)
            .is_none());
        r.drain(Instant::now() + FRAME_QUIET, &mut stats);
        assert_eq!(r.ready.pop_front().unwrap().data, vec![2]);
        assert_eq!(stats.lost_frames, 1);
        assert!(r
            .push(&packets(1, false, &[1], 64)[0], &mut stats)
            .is_none());
        assert_eq!(stats.lost_frames, 1);
    }

    #[test]
    fn fragmentation_rejects_unrepresentable_frames_without_partial_output() {
        let mut out = vec![vec![42]];
        assert!(fragment_frame(7, 0, true, 0, &vec![0; MAX_FRAGS * 64 + 1], 64, &mut out).is_err());
        assert!(fragment_frame(
            7,
            0,
            true,
            0,
            &vec![0; MAX_FRAME_BYTES + 1],
            DEFAULT_MTU,
            &mut out
        )
        .is_err());
        assert_eq!(out, vec![vec![42]]);
        let packets = packets(0, true, &[1; 3000], usize::MAX);
        assert!(packets
            .iter()
            .all(|p| p.len() <= FRAG_HEADER_LEN + DEFAULT_MTU));
    }

    #[test]
    fn expired_frame_cannot_be_reopened_by_late_fragments() {
        for seq in [0, u16::MAX] {
            let mut r = Reassembler::new(7);
            let mut stats = ReassemblyStats::default();
            let frame = packets(seq, false, &[7; 300], 100);
            r.push(&frame[0], &mut stats);
            r.drain(Instant::now() + FRAME_QUIET, &mut stats);
            assert_eq!(stats.lost_frames, 1);
            for packet in &frame {
                assert!(r.push(packet, &mut stats).is_none());
            }
            assert!(r.slots.is_empty(), "a retired frame must stay retired");
            assert_eq!(stats.late_frags, 3);
            let next = packets(seq.wrapping_add(1), true, &[9], 100);
            assert_eq!(r.push(&next[0], &mut stats).unwrap().data, vec![9]);
            assert_eq!(stats.lost_frames, 1, "loss must only be counted once");
        }
    }

    #[test]
    fn new_fragment_refreshes_quiet_deadline() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let frame = packets(0, true, &[7; 300], 100);
        r.push(&frame[0], &mut stats);
        // Simulate an old first packet without relying on scheduler sleeps.
        r.slots[0].last_frag = Instant::now() - Duration::from_secs(1);
        assert!(r.push(&frame[1], &mut stats).is_none());
        assert_eq!(stats.lost_frames, 0, "fresh fragments keep a frame alive");
        assert_eq!(r.push(&frame[2], &mut stats).unwrap().data, vec![7; 300]);
    }

    #[test]
    fn duplicate_and_inconsistent_fragments_do_not_refresh_deadline() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let frame = packets(0, true, &[7; 300], 100);
        r.push(&frame[0], &mut stats);
        let old = r.slots[0].last_frag;
        r.push(&frame[0], &mut stats);
        let mut invalid = frame[1].clone();
        invalid[14] ^= 1;
        r.push(&invalid, &mut stats);
        assert_eq!(r.slots[0].last_frag, old);
        r.drain(old + FRAME_QUIET, &mut stats);
        assert_eq!(stats.lost_frames, 1);
    }

    #[test]
    fn inconsistent_fragment_metadata_is_ignored() {
        let mut fragments = packets(1, true, &[3; 200], 100);
        let mut reassembler = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        assert!(reassembler.push(&fragments[0], &mut stats).is_none());
        let original = fragments[1].clone();
        fragments[1][14] ^= 1; // Different timestamp in the same frame.
        assert!(reassembler.push(&fragments[1], &mut stats).is_none());
        assert_eq!(
            reassembler.push(&original, &mut stats).unwrap().data.len(),
            200
        );
    }

    fn packets(frame_seq: u16, keyframe: bool, data: &[u8], mtu: usize) -> Vec<Vec<u8>> {
        let mut pkts = Vec::new();
        fragment_frame(7, frame_seq, keyframe, 1234, data, mtu, &mut pkts).unwrap();
        pkts
    }

    #[test]
    fn fragment_size() {
        let pkts = packets(1, true, &vec![0xAB; 3000], 1400);
        assert_eq!(pkts.len(), 3);
        for p in &pkts {
            assert!(p.len() <= 1400 + FRAG_HEADER_LEN);
        }
        // 空帧也产出 1 个包
        let one = packets(2, false, &[], 1400);
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn large_frame_over_255_frags() {
        // 回归：frag_count 曾为 u8，>255 片（>357,000B）时溢出——
        // 357,001B 产生 0 个包（count=256 as u8 = 0），358,401B 被截断成 1 片
        for size in [357_001usize, 358_401, 500_000] {
            let mut pkts = Vec::new();
            fragment_frame(7, 5, true, 42, &vec![0x5A; size], DEFAULT_MTU, &mut pkts).unwrap();
            let expect = size.div_ceil(DEFAULT_MTU);
            assert_eq!(pkts.len(), expect, "{size}B 应产生 {expect} 片");
            assert!(expect > 255, "用例应覆盖 >255 片");
            // 分片头可解析且序号连续
            for (i, p) in pkts.iter().enumerate() {
                let (h, _) = FragHeader::decode(p).expect("分片头可解析");
                assert_eq!(h.frag_count as usize, expect);
                assert_eq!(h.frag_idx as usize, i);
                assert!(h.is_keyframe());
            }
            // 完整重组
            let mut r = Reassembler::new(7);
            let mut stats = ReassemblyStats::default();
            let mut out = None;
            for p in &pkts {
                if let Some(f) = r.push(p, &mut stats) {
                    out = Some(f);
                }
            }
            let f = out.expect("大帧应完整重组");
            assert_eq!(f.data.len(), size);
            assert!(f.data.iter().all(|&b| b == 0x5A));
        }
    }

    #[test]
    fn reassemble_in_order() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let pkts = packets(0, true, &vec![7u8; 3000], 1000);
        for (i, p) in pkts.iter().enumerate() {
            let out = r.push(p, &mut stats);
            if i + 1 == pkts.len() {
                let f = out.expect("最后一片应完成整帧");
                assert!(f.keyframe);
                assert_eq!(f.data.len(), 3000);
                assert!(f.data.iter().all(|&b| b == 7));
            } else {
                assert!(out.is_none());
            }
        }
        assert_eq!(stats.lost_frames, 0);
    }

    #[test]
    fn reassemble_out_of_order() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let pkts = packets(0, true, &vec![5u8; 2500], 1000);
        let out = r.push(&pkts[2], &mut stats);
        assert!(out.is_none());
        let out = r.push(&pkts[0], &mut stats);
        assert!(out.is_none());
        let out = r.push(&pkts[1], &mut stats).expect("乱序到齐应完成");
        assert_eq!(out.data.len(), 2500);
    }

    #[test]
    fn loss_counts_and_next_frame() {
        // 旧帧未凑齐时新帧按序等待（乱序容忍）；安静超时后旧帧判失、新帧交付
        let mut r = Reassembler::with_quiet(7, Duration::from_millis(5));
        let mut stats = ReassemblyStats::default();
        let f0 = packets(0, true, &vec![0u8; 2500], 1000);
        let f1 = packets(1, false, &vec![1u8; 900], 1000);
        r.push(&f0[0], &mut stats); // 收到 f0 第 0 片
        assert!(
            r.push(&f1[0], &mut stats).is_none(),
            "旧帧未决，新帧应按序等待而不是顶替"
        );
        std::thread::sleep(Duration::from_millis(12));
        let out = r.poll_expiry(&mut stats).expect("超时后帧 1 应交付");
        assert_eq!(stats.lost_frames, 1);
        assert_eq!(out.data, vec![1u8; 900]);
        // f0 迟到分片被忽略（已交付过更晚的帧）
        assert!(r.push(&f0[1], &mut stats).is_none());
        assert_eq!(stats.late_frags, 1);
    }

    #[test]
    fn cross_frame_reorder_delivers_both_frames() {
        // 回归（用户复现）：两帧分片全部到达，仅后一帧首包提前——
        // 旧实现一看到新帧就放弃未完成的旧帧，导致两帧只出一帧。
        let mut r = Reassembler::with_quiet(7, Duration::from_secs(30));
        let mut stats = ReassemblyStats::default();
        let f0 = packets(0, true, &vec![0u8; 2500], 1000);
        let f1 = packets(1, false, &vec![1u8; 2500], 1000);
        // 后一帧首包提前
        assert!(r.push(&f1[0], &mut stats).is_none());
        // 旧帧分片随后全部到齐（乱序）
        assert!(r.push(&f0[0], &mut stats).is_none());
        assert!(r.push(&f0[1], &mut stats).is_none());
        let a = r.push(&f0[2], &mut stats).expect("帧 0 应凑齐");
        assert_eq!(a.frame_seq, 0);
        assert_eq!(a.data, vec![0u8; 2500]);
        // 新帧剩余分片到齐后按序交付
        assert!(r.push(&f1[1], &mut stats).is_none());
        let b = r.push(&f1[2], &mut stats).expect("帧 1 应凑齐");
        assert_eq!(b.frame_seq, 1);
        assert_eq!(b.data, vec![1u8; 2500]);
        assert_eq!(stats.lost_frames, 0, "两帧全部到达，不应计丢失");
        assert_eq!(stats.late_frags, 0);
    }

    #[test]
    fn complete_frame_not_stranded_behind_lost_frame() {
        // 完整帧压在判失的旧帧后面时，必须靠 poll_expiry 放出，
        // 否则静止桌面前的最后一帧（鼠标刚动完那一下）永远发不出去
        let mut r = Reassembler::with_quiet(7, Duration::from_millis(5));
        let mut stats = ReassemblyStats::default();
        let f0 = packets(0, true, &vec![0u8; 2500], 1000);
        let f1 = packets(1, false, &vec![1u8; 900], 1000);
        r.push(&f0[0], &mut stats); // 帧 0 只有一片，永不凑齐
        assert!(r.push(&f1[0], &mut stats).is_none());
        std::thread::sleep(Duration::from_millis(12));
        let f = r.poll_expiry(&mut stats).expect("超时后完整帧不应被压住");
        assert_eq!(f.frame_seq, 1);
        assert_eq!(f.data, vec![1u8; 900]);
        assert_eq!(stats.lost_frames, 1);
    }

    #[test]
    fn seq_wraparound() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let a = packets(0xFFFE, true, &[1], 64);
        let b = packets(0xFFFF, true, &[2], 64);
        let c = packets(0x0000, true, &[3], 64);
        assert!(r.push(&a[0], &mut stats).is_some());
        assert!(r.push(&b[0], &mut stats).is_some());
        assert!(r.push(&c[0], &mut stats).is_some());
        assert_eq!(stats.lost_frames, 0);
    }

    #[test]
    fn whole_frame_loss_counted_by_seq_gap() {
        // 序号从 0 跳到 2 表示帧 1 的所有分片丢失，必须计入 lost_frames。
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let f0 = packets(0, true, &vec![0u8; 500], 200); // 3 片，完整到达
        let f2 = packets(2, true, &vec![2u8; 500], 200); // 帧 1 整体消失
        assert!(r.push(&f0[0], &mut stats).is_none());
        assert!(r.push(&f0[1], &mut stats).is_none());
        assert!(r.push(&f0[2], &mut stats).is_some(), "帧 0 完成");
        assert!(r.push(&f2[0], &mut stats).is_none());
        assert!(r.push(&f2[1], &mut stats).is_none());
        let done = r.push(&f2[2], &mut stats).expect("帧 2 完成");
        assert_eq!(stats.lost_frames, 1, "帧 1 整帧丢失应计入");
        assert_eq!(done.data, vec![2u8; 500]);
    }

    #[test]
    fn gap_wraparound_counted() {
        let mut r = Reassembler::new(7);
        let mut stats = ReassemblyStats::default();
        let a = packets(0xFFFE, true, &[1], 64);
        let c = packets(0x0000, true, &[3], 64); // 0xFFFF 整帧丢失
        assert!(r.push(&a[0], &mut stats).is_some());
        let out = r.push(&c[0], &mut stats).expect("回绕后新帧");
        assert_eq!(stats.lost_frames, 1, "回绕边界整帧丢失应计入");
        assert_eq!(out.data, vec![3u8]);
    }

    #[test]
    fn partial_then_next_counts_once() {
        // 部分到达后超时判失：只计 1 次，不与 gap 重复计数
        let mut r = Reassembler::with_quiet(7, Duration::from_millis(5));
        let mut stats = ReassemblyStats::default();
        let f0 = packets(0, true, &vec![0u8; 2500], 1000); // 3 片
        let f1 = packets(1, false, &vec![1u8; 900], 1000); // 1 片
        r.push(&f0[0], &mut stats);
        r.push(&f1[0], &mut stats); // 按序等待中
        std::thread::sleep(Duration::from_millis(12));
        r.poll_expiry(&mut stats).expect("帧 1 交付");
        assert_eq!(stats.lost_frames, 1);
    }

    #[test]
    fn foreign_session_ignored() {
        let mut r = Reassembler::new(1);
        let mut stats = ReassemblyStats::default();
        let pkts = packets(0, true, &[9], 64);
        assert!(r.push(&pkts[0], &mut stats).is_none());
        assert_eq!(stats, ReassemblyStats::default());
    }
}
