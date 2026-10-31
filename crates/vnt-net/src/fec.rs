//! Negotiated UDP-only systematic FEC. Originals never wait for a coding block.
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use vnt_proto::{
    pb,
    udp::{FragHeader, DEFAULT_MTU, FRAG_HEADER_LEN},
};

pub const VERSION: u32 = 1;
pub const FLAG: u8 = 0x08;
pub const HEADER: usize = 38;
pub const OVERHEAD: usize = HEADER + 2;
pub const MAX_PACKET: usize = FRAG_HEADER_LEN + DEFAULT_MTU;
const DEADLINE: Duration = Duration::from_millis(20);
const MAX_MEMORY: usize = 32 * 1024 * 1024;
const MAX_GROUPS: usize = 1024;
// Emit repair after 10 ms, leaving jitter headroom inside the 20 ms deadline.
const AUDIO_SHARDS: usize = 2;

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub stream: u32,
    pub generation: u32,
    pub audio: bool,
}

struct Adapt {
    ratio: f64,
    loss: f64,
    healthy: Option<Instant>,
    last: Option<Instant>,
    last_decrease: Option<Instant>,
    rtts: VecDeque<(Instant, u32)>,
}
impl Default for Adapt {
    fn default() -> Self {
        Self {
            ratio: 0.05,
            loss: 0.0,
            healthy: None,
            last: None,
            last_decrease: None,
            rtts: VecDeque::new(),
        }
    }
}
impl Adapt {
    fn update(&mut self, f: &pb::FecFeedback, report: &pb::LinkFeedback, now: Instant) {
        if f.expected == 0 || f.received > f.expected || f.recovered > f.expected - f.received {
            return;
        }
        if self
            .last
            .is_some_and(|at| now.duration_since(at) < Duration::from_millis(500))
        {
            return;
        }
        if self
            .last
            .is_some_and(|at| now.duration_since(at) > Duration::from_secs(3))
        {
            self.healthy = None;
        }
        self.last = Some(now);
        let p = (f.expected - f.received) as f64 / f.expected as f64;
        self.loss = 0.75 * self.loss + 0.25 * p;
        while self
            .rtts
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(60))
        {
            self.rtts.pop_front();
        }
        if let Some(rtt) = report.rtt_ms {
            self.rtts.push_back((now, rtt));
        }
        let base = self.rtts.iter().map(|(_, v)| *v).min().unwrap_or(0);
        let congested = report
            .rtt_ms
            .is_some_and(|rtt| rtt.saturating_sub(base) > 50.max(base / 2))
            || report.frame_gap_ms.is_some_and(|gap| gap >= 1000);
        let target = (1.5 * self.loss / (1.0 - self.loss).max(0.001) + 0.02).clamp(0.0, 0.30);
        if p > 0.001 || f.residual > 0 || congested {
            self.healthy = None;
            if !congested {
                self.ratio = self.ratio.max(target);
            }
        } else {
            let since = *self.healthy.get_or_insert(now);
            if now.duration_since(since) >= Duration::from_secs(5)
                && self
                    .last_decrease
                    .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
            {
                self.ratio = (self.ratio - 0.02).max(0.0);
                self.last_decrease = Some(now);
            }
        }
    }
}

#[derive(Default)]
struct Lane {
    adapt: Adapt,
    sequence: u64,
    credit: f64,
    audio: Option<AudioBlock>,
}
struct AudioBlock {
    start: Instant,
    base: u64,
    parity: usize,
    ratio: f64,
    data: Vec<Vec<u8>>,
}
#[derive(Default)]
struct State {
    lanes: HashMap<Key, Lane>,
    feedback_sequence: HashMap<(u32, u32), u64>,
    pressure: Option<Instant>,
}

/// A session owns one handle, shared by its audio and independently paced screens.
#[derive(Clone, Default)]
pub struct Sender(Arc<Mutex<State>>);
impl Sender {
    pub fn negotiated(request: u32, udp: bool) -> Option<Self> {
        (udp && request == VERSION).then(Self::default)
    }
    pub fn feedback(&self, report: &pb::LinkFeedback) {
        self.feedback_at(report, Instant::now());
    }
    fn feedback_at(&self, report: &pb::LinkFeedback, now: Instant) {
        if !vnt_proto::validation::valid_link_feedback(report) {
            return;
        }
        let mut state = self.0.lock().unwrap();
        let scope = report
            .fec
            .iter()
            .find(|f| !f.audio)
            .or_else(|| report.fec.first())
            .map(|f| (f.stream_id, f.generation));
        let Some(scope) = scope else {
            return;
        };
        state
            .feedback_sequence
            .retain(|(id, g), _| *id != scope.0 || *g == scope.1);
        let seq = state.feedback_sequence.entry(scope).or_default();
        if report.sequence <= *seq {
            return;
        }
        *seq = report.sequence;
        let pressure = state
            .pressure
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1));
        for f in &report.fec {
            let key = Key {
                stream: f.stream_id,
                generation: f.generation,
                audio: f.audio,
            };
            if let Some(lane) = state.lanes.get_mut(&key) {
                let previous = lane.adapt.ratio;
                lane.adapt.update(f, report, now);
                if pressure {
                    lane.adapt.ratio = lane.adapt.ratio.min(previous);
                    lane.adapt.healthy = None;
                }
            }
        }
    }
    pub fn pressure(&self) {
        self.0.lock().unwrap().pressure = Some(Instant::now());
    }
    pub fn ratio(&self, key: Key) -> f64 {
        self.0
            .lock()
            .unwrap()
            .lanes
            .get(&key)
            .map_or(0.05, |l| l.adapt.ratio)
    }
    fn lane(state: &mut State, key: Key) -> &mut Lane {
        state.lanes.retain(|k, _| {
            k.audio != key.audio || k.stream != key.stream || k.generation == key.generation
        });
        state.lanes.entry(key).or_default()
    }
    pub fn video(&self, session: u32, key: Key, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut state = self.0.lock().unwrap();
        let lane = Self::lane(&mut state, key);
        let mut out = Vec::new();
        for block in packets.chunks(32) {
            let data: Vec<_> = block.iter().map(|p| shard(p)).collect();
            let size = data.iter().map(Vec::len).max().unwrap_or(0);
            lane.credit += block.iter().map(Vec::len).sum::<usize>() as f64 * lane.adapt.ratio;
            let m = ((lane.credit / (size + HEADER) as f64).floor() as usize).min(10);
            lane.credit -= m as f64 * (size + HEADER) as f64;
            let base = lane.sequence;
            lane.sequence = lane.sequence.wrapping_add(data.len() as u64);
            for (i, d) in data.iter().enumerate() {
                out.push(wrap(
                    session,
                    key,
                    base,
                    i,
                    data.len(),
                    m,
                    lane.adapt.ratio,
                    d,
                ));
            }
            out.extend(parity(session, key, base, &data, m, lane.adapt.ratio));
        }
        out
    }
    pub fn audio(&self, session: u32, generation: u32, packet: Vec<u8>) -> Vec<Vec<u8>> {
        self.audio_at(session, generation, packet, Instant::now())
    }
    fn audio_at(
        &self,
        session: u32,
        generation: u32,
        packet: Vec<u8>,
        now: Instant,
    ) -> Vec<Vec<u8>> {
        let key = Key {
            stream: 0,
            generation,
            audio: true,
        };
        let mut state = self.0.lock().unwrap();
        let lane = Self::lane(&mut state, key);
        if lane
            .audio
            .as_ref()
            .is_some_and(|b| now.duration_since(b.start) > DEADLINE)
        {
            if let Some(b) = lane.audio.take() {
                if b.parity > 0 {
                    lane.credit += MAX_PACKET as f64;
                }
            }
        }
        lane.credit += packet.len() as f64 * lane.adapt.ratio;
        if lane.audio.is_none() {
            // Reserve an entire maximum-sized parity datagram: even variable Opus
            // packet sizes cannot spend more than the cumulative byte allowance.
            let m = usize::from(lane.credit >= MAX_PACKET as f64);
            if m > 0 {
                lane.credit -= MAX_PACKET as f64;
            }
            let base = lane.sequence;
            lane.audio = Some(AudioBlock {
                start: now,
                base,
                parity: m,
                ratio: lane.adapt.ratio,
                data: Vec::new(),
            });
        }
        let b = lane.audio.as_mut().unwrap();
        let d = shard(&packet);
        let mut out = vec![wrap(
            session,
            key,
            b.base,
            b.data.len(),
            AUDIO_SHARDS,
            b.parity,
            b.ratio,
            &d,
        )];
        b.data.push(d);
        lane.sequence = lane.sequence.wrapping_add(1);
        if b.data.len() == AUDIO_SHARDS {
            let repair = parity(session, key, b.base, &b.data, b.parity, b.ratio);
            if b.parity > 0 {
                lane.credit +=
                    MAX_PACKET as f64 - repair.iter().map(Vec::len).sum::<usize>() as f64;
            }
            out.extend(repair);
            lane.audio = None;
        }
        out
    }
}

fn shard(packet: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(packet.len() + 2);
    d.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    d.extend_from_slice(packet);
    d
}
#[allow(clippy::too_many_arguments)]
fn wrap(
    session: u32,
    key: Key,
    base: u64,
    index: usize,
    k: usize,
    m: usize,
    ratio: f64,
    data: &[u8],
) -> Vec<u8> {
    let mut p = vec![0; HEADER];
    FragHeader {
        session_id: session,
        frame_seq: 0,
        frag_idx: 0,
        frag_count: 1,
        flags: FLAG,
        pts_ms: 0,
    }
    .encode_to(&mut p);
    p[15] = VERSION as u8;
    p[16] = u8::from(key.audio);
    p[17..21].copy_from_slice(&key.stream.to_be_bytes());
    p[21..25].copy_from_slice(&key.generation.to_be_bytes());
    p[25..33].copy_from_slice(&base.to_be_bytes());
    p[33] = index as u8;
    p[34] = k as u8;
    p[35] = m as u8;
    p[36..38].copy_from_slice(&((ratio * 1000.0).round() as u16).to_be_bytes());
    p.extend_from_slice(data);
    p
}
fn codec(k: usize, m: usize) -> Arc<ReedSolomon> {
    type CodecCache = Mutex<HashMap<(usize, usize), Arc<ReedSolomon>>>;
    static CODECS: OnceLock<CodecCache> = OnceLock::new();
    CODECS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry((k, m))
        .or_insert_with(|| Arc::new(ReedSolomon::new(k, m).unwrap()))
        .clone()
}
fn parity(
    session: u32,
    key: Key,
    base: u64,
    data: &[Vec<u8>],
    m: usize,
    ratio: f64,
) -> Vec<Vec<u8>> {
    if m == 0 {
        return Vec::new();
    }
    let size = data.iter().map(Vec::len).max().unwrap();
    let mut shards = data.to_vec();
    for d in &mut shards {
        d.resize(size, 0);
    }
    for _ in 0..m {
        shards.push(vec![0; size]);
    }
    codec(data.len(), m).encode(&mut shards).unwrap();
    (0..m)
        .map(|i| {
            wrap(
                session,
                key,
                base,
                data.len() + i,
                data.len(),
                m,
                ratio,
                &shards[data.len() + i],
            )
        })
        .collect()
}

struct Group {
    key: Key,
    base: u64,
    k: usize,
    m: usize,
    ratio: u32,
    shards: Vec<Option<Vec<u8>>>,
    originals: Vec<bool>,
    delivered: Vec<bool>,
    created: Instant,
    last: Instant,
    parity_bytes: u64,
}
impl Group {
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(if self.key.audio {
            self.created
        } else {
            self.last
        }) >= DEADLINE
            || now.duration_since(self.created) >= Duration::from_secs(1)
    }
}
#[derive(Default)]
pub struct Decoder {
    groups: HashMap<(Key, u64), Group>,
    retired: HashMap<Key, u64>,
    reports: HashMap<Key, pb::FecFeedback>,
    memory: usize,
    last_expiry: Option<Instant>,
    audio_delivered: VecDeque<(u32, u64)>,
}
impl Decoder {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn retain_screens(&mut self, ids: &[u32]) {
        self.groups
            .retain(|(k, _), _| k.audio || k.stream == 0 || ids.contains(&k.stream));
        self.retired
            .retain(|k, _| k.audio || k.stream == 0 || ids.contains(&k.stream));
        self.reports
            .retain(|k, _| k.audio || k.stream == 0 || ids.contains(&k.stream));
        self.recount();
    }
    fn recount(&mut self) {
        self.memory = self
            .groups
            .values()
            .flat_map(|g| g.shards.iter().flatten())
            .map(Vec::len)
            .sum();
    }
    pub fn expire(&mut self, now: Instant) {
        if self
            .last_expiry
            .is_some_and(|at| now.duration_since(at) < Duration::from_millis(1))
        {
            return;
        }
        self.last_expiry = Some(now);
        let mut ordered: Vec<_> = self.groups.keys().copied().collect();
        ordered.sort_unstable();
        let mut blocked = None;
        for id in ordered {
            if blocked == Some(id.0) {
                continue;
            }
            if self.groups[&id].expired(now) {
                let g = &self.groups[&id];
                if g.key.audio && g.parity_bytes == 0 && g.originals.iter().any(|v| !v) {
                    // The sender may close a short block after a capture pause.
                    // Its next base counts only packets actually emitted. Keep
                    // statistics pending until that count is known; playback and
                    // repair still stop at the original 20 ms deadline.
                    let next = self
                        .groups
                        .keys()
                        .filter(|(key, base)| *key == id.0 && *base > id.1)
                        .map(|(_, base)| *base)
                        .min();
                    let Some(next) = next else {
                        blocked = Some(id.0);
                        continue;
                    };
                    let g = self.groups.get_mut(&id).unwrap();
                    g.k = g.k.min((next - id.1) as usize);
                    g.originals.truncate(g.k);
                    g.delivered.truncate(g.k);
                }
                self.finish(id);
            } else {
                blocked = Some(id.0);
            }
        }
        self.recount();
    }
    fn finish(&mut self, id: (Key, u64)) {
        let Some(g) = self.groups.remove(&id) else {
            return;
        };
        let end = self.retired.entry(g.key).or_default();
        let gap = g.base.saturating_sub(*end).min(1_000_000);
        *end = (*end).max(g.base.saturating_add(g.k as u64));
        let f = self
            .reports
            .entry(g.key)
            .or_insert_with(|| pb::FecFeedback {
                stream_id: g.key.stream,
                generation: g.key.generation,
                audio: g.key.audio,
                ..Default::default()
            });
        let received = g.originals.iter().filter(|v| **v).count() as u64;
        // A recovered original may arrive before expiry; count it as received,
        // and never also claim it as an FEC repair in the same window.
        let recovered = g
            .delivered
            .iter()
            .zip(&g.originals)
            .filter(|(d, o)| **d && !**o)
            .count() as u64;
        f.expected += g.k as u64 + gap;
        f.received += received;
        f.recovered += recovered;
        f.residual += g.k as u64 - received - recovered + gap;
        f.parity_bytes += g.parity_bytes;
        f.ratio_permille = g.ratio;
    }
    pub fn take_reports(&mut self, stream: Option<u32>) -> Vec<pb::FecFeedback> {
        self.expire(Instant::now());
        let keys: Vec<_> = self
            .reports
            .keys()
            .filter(|k| match stream {
                Some(id) => !k.audio && k.stream == id,
                None => k.audio,
            })
            .copied()
            .collect();
        keys.into_iter()
            .filter_map(|k| self.reports.remove(&k))
            .collect()
    }
    pub fn push(&mut self, packet: &[u8], session: u32, now: Instant) -> Vec<Vec<u8>> {
        self.expire(now);
        if packet.len() < HEADER + 2
            || packet.len() > MAX_PACKET
            || packet[10] != FLAG
            || packet[15] != VERSION as u8
            || packet[16] > 1
        {
            return Vec::new();
        }
        let Some((h, _)) = FragHeader::decode(packet) else {
            return Vec::new();
        };
        if h.session_id != session
            || h.frag_count != 1
            || h.frag_idx != 0
            || h.frame_seq != 0
            || h.pts_ms != 0
        {
            return Vec::new();
        }
        let key = Key {
            stream: u32::from_be_bytes(packet[17..21].try_into().unwrap()),
            generation: u32::from_be_bytes(packet[21..25].try_into().unwrap()),
            audio: packet[16] == 1,
        };
        let base = u64::from_be_bytes(packet[25..33].try_into().unwrap());
        let i = packet[33] as usize;
        let k = packet[34] as usize;
        let m = packet[35] as usize;
        let ratio = u16::from_be_bytes(packet[36..38].try_into().unwrap()) as u32;
        if base.checked_add(k as u64).is_none()
            || k == 0
            || k > 32
            || m > 10
            || i >= k + m
            || ratio > 300
            || key.stream > 128
            || (!key.audio && ((key.stream == 0) != (key.generation == 0)))
            || (key.audio && (key.stream != 0 || key.generation == 0 || k > 3 || m > 1))
        {
            return Vec::new();
        }
        let body = &packet[HEADER..];
        if i < k && original(body, session, key).is_none() {
            return Vec::new();
        }
        // Generations never share coding state. Older datagrams cannot resurrect it.
        if self.groups.keys().any(|(old, _)| {
            old.stream == key.stream && old.audio == key.audio && old.generation > key.generation
        }) || self.retired.keys().any(|old| {
            old.stream == key.stream && old.audio == key.audio && old.generation > key.generation
        }) {
            return Vec::new();
        }
        if self.retired.get(&key).is_some_and(|end| base < *end) {
            // A source packet created at the end of an audio block is still
            // useful even when that block's repair deadline has passed. The
            // playback jitter buffer owns its final age/sequence decision.
            return if key.audio && i < k {
                self.audio_output(
                    vec![original(body, session, key).unwrap().to_vec()],
                    session,
                )
            } else {
                Vec::new()
            };
        }
        self.groups.retain(|(old, _), _| {
            old.stream != key.stream || old.audio != key.audio || old.generation == key.generation
        });
        self.retired.retain(|old, _| {
            old.stream != key.stream || old.audio != key.audio || old.generation == key.generation
        });
        self.reports.retain(|old, _| {
            old.stream != key.stream || old.audio != key.audio || old.generation == key.generation
        });
        if !self.groups.contains_key(&(key, base))
            && (self.groups.len() >= MAX_GROUPS || self.memory + body.len() > MAX_MEMORY)
        {
            return Vec::new();
        }
        let g = self.groups.entry((key, base)).or_insert_with(|| Group {
            key,
            base,
            k,
            m,
            ratio,
            shards: vec![None; k + m],
            originals: vec![false; k],
            delivered: vec![false; k],
            created: now,
            last: now,
            parity_bytes: 0,
        });
        if g.k != k
            || g.m != m
            || g.ratio != ratio
            || g.shards[i].is_some()
            || self.memory + body.len() > MAX_MEMORY
            || (g.key.audio && g.expired(now) && i >= k)
        {
            return Vec::new();
        }
        if g.shards
            .iter()
            .skip(k)
            .flatten()
            .any(|p| p.len() != body.len())
            && i >= k
        {
            return Vec::new();
        }
        if i < k {
            g.originals[i] = true;
        } else {
            g.parity_bytes += packet.len() as u64;
        }
        g.shards[i] = Some(body.to_vec());
        g.last = now;
        self.memory += body.len();
        let mut out = Vec::new();
        if i < k && !g.delivered[i] {
            g.delivered[i] = true;
            out.push(original(body, session, key).unwrap().to_vec());
        }
        if m > 0
            && (!g.key.audio || !g.expired(now))
            && g.shards.iter().filter(|s| s.is_some()).count() >= k
            && g.delivered.iter().any(|v| !*v)
        {
            let size = g.shards.iter().flatten().map(Vec::len).max().unwrap();
            if g.shards.iter().skip(k).flatten().all(|p| p.len() == size) {
                let mut shards = g.shards.clone();
                for s in shards.iter_mut().flatten() {
                    s.resize(size, 0);
                }
                if codec(k, m).reconstruct_data(&mut shards).is_ok() {
                    for (j, s) in shards.iter().take(k).enumerate() {
                        if !g.delivered[j] {
                            if let Some(p) = s.as_deref().and_then(|s| original(s, session, key)) {
                                out.push(p.to_vec());
                                g.delivered[j] = true;
                            }
                        }
                    }
                }
            }
        }
        if key.audio {
            self.audio_output(out, session)
        } else {
            out
        }
    }

    fn audio_output(&mut self, packets: Vec<Vec<u8>>, session: u32) -> Vec<Vec<u8>> {
        packets
            .into_iter()
            .filter(|p| {
                let Some(frame) = vnt_proto::audio::decode_datagram(p, session) else {
                    return false;
                };
                let id = (frame.generation, frame.timestamp_samples);
                if self.audio_delivered.contains(&id) {
                    return false;
                }
                if self
                    .audio_delivered
                    .iter()
                    .any(|(generation, _)| *generation > frame.generation)
                {
                    return false;
                }
                self.audio_delivered
                    .retain(|(generation, _)| *generation == frame.generation);
                if self.audio_delivered.front().is_some_and(|(_, timestamp)| {
                    frame
                        .timestamp_samples
                        .saturating_add(128 * vnt_proto::audio::FRAME_SAMPLES)
                        < *timestamp
                }) {
                    return false;
                }
                self.audio_delivered.push_back(id);
                if self.audio_delivered.len() > 128 {
                    self.audio_delivered.pop_front();
                }
                true
            })
            .collect()
    }
}
fn original(body: &[u8], session: u32, key: Key) -> Option<&[u8]> {
    let len = u16::from_be_bytes(body.get(..2)?.try_into().ok()?) as usize;
    let p = body.get(2..2 + len)?;
    let (h, _) = FragHeader::decode(p)?;
    if h.session_id != session || h.flags & FLAG != 0 {
        return None;
    }
    if key.audio {
        let a = vnt_proto::audio::decode_datagram(p, session)?;
        if a.generation != key.generation {
            return None;
        }
    } else if key.stream != 0 {
        if p.len() < 23
            || h.flags & 0x04 == 0
            || u32::from_be_bytes(p[15..19].try_into().ok()?) != key.stream
            || u32::from_be_bytes(p[19..23].try_into().ok()?) != key.generation
        {
            return None;
        }
    } else if h.flags & 0x06 != 0 {
        return None;
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packets(seq: u16, size: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        vnt_proto::udp::fragment_frame(7, seq, true, 123, &vec![seq as u8; size], 100, &mut out)
            .unwrap();
        out
    }
    fn sender(key: Key, ratio: f64) -> Sender {
        let s = Sender::default();
        s.0.lock()
            .unwrap()
            .lanes
            .entry(key)
            .or_default()
            .adapt
            .ratio = ratio;
        s
    }
    fn report(sequence: u64, received: u64, recovered: u64, rtt: u32) -> pb::LinkFeedback {
        pb::LinkFeedback {
            sequence,
            interval_ms: 1000,
            rtt_ms: Some(rtt),
            fec: vec![pb::FecFeedback {
                expected: 100,
                received,
                recovered,
                residual: 100 - received - recovered,
                ratio_permille: 50,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn restores_multiple_erasures_tail_and_wrapping_video_sequence_without_duplicates() {
        let key = Key::default();
        let s = sender(key, 0.3);
        let now = Instant::now();
        let mut d = Decoder::default();
        for seq in [u16::MAX, 0] {
            let originals = packets(seq, 3137);
            let coded = s.video(7, key, originals.clone());
            assert!(coded.iter().all(|p| p.len() <= MAX_PACKET));
            assert!(coded.len() > originals.len() + 3);
            let mut output = Vec::new();
            // Two adjacent erasures, and the short final fragment. Reverse order
            // also delivers parity before originals and exercises deduplication.
            for p in coded.iter().rev().filter(|p| ![2, 3, 31].contains(&p[33])) {
                output.extend(d.push(p, 7, now));
                assert!(d.push(p, 7, now).is_empty());
            }
            output.sort();
            let mut expected = originals;
            expected.sort();
            assert_eq!(output, expected);
        }
        d.expire(now + Duration::from_millis(21));
        let f = d.take_reports(Some(0));
        assert_eq!(f[0].expected, 64);
        assert_eq!(f[0].received, 58);
        assert_eq!(f[0].recovered, 6);
        assert_eq!(f[0].residual, 0);
    }
    #[test]
    fn late_original_is_counted_after_reorder_window_and_is_never_delivered_twice() {
        let key = Key::default();
        let s = sender(key, 0.3);
        let now = Instant::now();
        let mut d = Decoder::default();
        let coded = s.video(7, key, packets(1, 3200));
        for p in coded.iter().filter(|p| p[33] != 0) {
            d.push(p, 7, now);
        }
        assert!(d
            .push(&coded[0], 7, now + Duration::from_millis(10))
            .is_empty());
        assert!(d.take_reports(Some(0)).is_empty());
        d.expire(now + Duration::from_millis(31));
        let f = d.take_reports(Some(0));
        assert_eq!((f[0].received, f[0].recovered), (32, 0));
        assert!(d
            .push(&coded[0], 7, now + Duration::from_millis(32))
            .is_empty());
    }
    #[test]
    fn unrecoverable_and_entirely_missing_blocks_remain_visible_in_raw_loss() {
        let key = Key::default();
        let s = sender(key, 0.3);
        let mut d = Decoder::default();
        let now = Instant::now();
        let first = s.video(7, key, packets(1, 3200));
        let _missing = s.video(7, key, packets(2, 3200));
        let next = s.video(7, key, packets(3, 3200));
        for p in first.iter().filter(|p| p[33] >= 20).chain(next.iter()) {
            d.push(p, 7, now);
        }
        d.expire(now + Duration::from_millis(21));
        let f = d.take_reports(Some(0));
        assert_eq!(f[0].expected, 96);
        assert_eq!(f[0].received, 44);
        assert_eq!(f[0].residual, 52);
    }
    #[test]
    fn adaptive_ratio_rises_on_raw_loss_even_when_recovered_and_respects_pressure_and_freshness() {
        let key = Key::default();
        let s = sender(key, 0.05);
        let now = Instant::now();
        s.feedback_at(&report(1, 80, 20, 10), now);
        assert!(s.ratio(key) > 0.09);
        let previous = s.ratio(key);
        s.feedback_at(&report(2, 20, 80, 200), now + Duration::from_secs(1));
        assert_eq!(s.ratio(key), previous);
        s.0.lock().unwrap().pressure = Some(now + Duration::from_secs(2));
        s.feedback_at(&report(3, 10, 90, 10), now + Duration::from_secs(2));
        assert_eq!(s.ratio(key), previous);
        for i in 3..10 {
            s.feedback_at(&report(i + 1, 10, 90, 10), now + Duration::from_secs(i));
        }
        assert_eq!(s.ratio(key), 0.3);
        for i in 10..15 {
            s.feedback_at(&report(i + 1, 100, 0, 10), now + Duration::from_secs(i));
        }
        assert_eq!(s.ratio(key), 0.3);
        // A long feedback gap must restart the healthy streak.
        s.feedback_at(&report(20, 100, 0, 10), now + Duration::from_secs(30));
        assert_eq!(s.ratio(key), 0.3);
        for i in 31..60 {
            s.feedback_at(&report(i, 100, 0, 10), now + Duration::from_secs(i));
        }
        assert_eq!(s.ratio(key), 0.0);
    }
    #[test]
    fn fractional_budget_caps_actual_parity_bytes_for_tiny_and_variable_blocks() {
        let key = Key::default();
        let s = sender(key, 0.3);
        let mut source = 0usize;
        let mut parity = 0usize;
        for i in 0..1000 {
            let p = packets(i, if i % 3 == 0 { 31 } else { 3000 });
            source += p.iter().map(Vec::len).sum::<usize>();
            for p in s.video(7, key, p) {
                if p[33] >= p[34] {
                    parity += p.len();
                }
            }
            assert!(parity as f64 <= source as f64 * 0.3 + 0.01);
        }
        assert!(parity > source / 4);
    }
    fn audio(seq: u32, generation: u32) -> Vec<u8> {
        vnt_proto::audio::encode_datagram(
            7,
            &pb::AudioFrame {
                generation,
                sequence: seq,
                timestamp_samples: seq as u64 * 480,
                data: vec![seq as u8; 100],
            },
        )
        .unwrap()
    }
    #[test]
    fn audio_is_immediate_repairs_within_twenty_ms_and_rejects_expired_repair_and_old_generations()
    {
        let key = Key {
            audio: true,
            generation: 1,
            ..Default::default()
        };
        let s = sender(key, 0.3);
        s.0.lock().unwrap().lanes.get_mut(&key).unwrap().credit = MAX_PACKET as f64;
        let now = Instant::now();
        let mut d = Decoder::default();
        let mut all = Vec::new();
        for i in 0..2 {
            let out = s.audio_at(
                7,
                1,
                audio(i, 1),
                now + Duration::from_millis(i as u64 * 10),
            );
            assert_eq!(out.len(), if i == 1 { 2 } else { 1 });
            all.extend(out);
        }
        assert_eq!(d.push(&all[0], 7, now), vec![audio(0, 1)]);
        assert_eq!(
            d.push(&all[2], 7, now + Duration::from_millis(19)),
            vec![audio(1, 1)]
        );
        let mut expired = Decoder::default();
        expired.push(&all[0], 7, now);
        assert!(expired
            .push(&all[2], 7, now + Duration::from_millis(21))
            .is_empty());
        let new = s.audio_at(7, 2, audio(0, 2), now + Duration::from_millis(22));
        assert_eq!(
            d.push(&new[0], 7, now + Duration::from_millis(22)),
            vec![audio(0, 2)]
        );
        assert!(d
            .push(&all[1], 7, now + Duration::from_millis(23))
            .is_empty());
    }
    #[test]
    fn malformed_foreign_and_oversized_datagrams_are_bounded_and_do_not_poison_a_group() {
        let s = sender(Key::default(), 0.3);
        let coded = s.video(7, Key::default(), packets(1, 3200));
        let now = Instant::now();
        let mut d = Decoder::default();
        for (at, value) in [
            (15, 2),
            (16, 3),
            (33, 255),
            (34, 0),
            (34, 33),
            (35, 11),
            (36, 255),
        ] {
            let mut invalid = coded[0].clone();
            invalid[at] = value;
            assert!(d.push(&invalid, 7, now).is_empty());
        }
        assert!(d.push(&coded[0], 8, now).is_empty());
        let mut big = coded[0].clone();
        big.resize(MAX_PACKET + 1, 0);
        assert!(d.push(&big, 7, now).is_empty());
        assert_eq!(d.push(&coded[0], 7, now).len(), 1);
        assert!(d.memory <= MAX_MEMORY);
        assert!(Sender::negotiated(1, false).is_none());
        assert!(Sender::negotiated(0, true).is_none());
        assert!(Sender::negotiated(2, true).is_none());
    }

    #[test]
    fn seeded_random_loss_and_jitter_preserve_payloads_and_raw_loss_statistics() {
        let s = sender(Key::default(), 0.3);
        let mut d = Decoder::default();
        let now = Instant::now();
        let mut seed = 42u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 32
        };
        let mut repairs = 0;
        for seq in 0..128 {
            let originals = packets(seq, 3173);
            let mut coded = s.video(7, Key::default(), originals.clone());
            coded.retain(|_| random() % 100 >= 5);
            for i in (1..coded.len()).rev() {
                let j = random() as usize % (i + 1);
                coded.swap(i, j);
            }
            let mut received = Vec::new();
            for p in &coded {
                received.extend(d.push(p, 7, now));
            }
            repairs += originals
                .len()
                .saturating_sub(coded.iter().filter(|p| p[33] < p[34]).count());
            received.sort();
            let mut expected = originals;
            expected.sort();
            assert_eq!(received, expected);
        }
        d.expire(now + Duration::from_millis(21));
        let f = d.take_reports(Some(0));
        assert!(repairs > 0);
        assert_eq!(f[0].recovered, repairs as u64);
        assert_eq!(f[0].residual, 0);
    }

    #[test]
    fn audio_parity_byte_budget_is_bounded_for_variable_opus_sizes() {
        let s = Sender::default();
        let now = Instant::now();
        let mut source = 0usize;
        let mut parity = 0usize;
        for seq in 0..1200 {
            let frame = pb::AudioFrame {
                generation: 1,
                sequence: seq,
                timestamp_samples: seq as u64 * 480,
                data: vec![42; if seq % 7 == 0 { 1275 } else { 80 }],
            };
            let packet = vnt_proto::audio::encode_datagram(7, &frame).unwrap();
            source += packet.len();
            for p in s.audio_at(7, 1, packet, now + Duration::from_millis(seq as u64 * 10)) {
                assert!(p.len() <= MAX_PACKET);
                if p[33] >= p[34] {
                    parity += p.len();
                }
            }
            assert!(parity as f64 <= source as f64 * 0.05 + 0.01);
        }
        assert!(parity > 0);
    }

    #[test]
    fn normal_audio_cadence_and_capture_pauses_never_drop_originals_or_count_unsent_slots_as_loss()
    {
        let s = Sender::default();
        let now = Instant::now();
        let mut d = Decoder::default();
        // The first block is intentionally short. Audio resumes after a pause,
        // then a late source packet reaches the repair deadline exactly.
        for (sequence, ms, received) in [(0, 0, 0), (4, 40, 40), (5, 50, 60)] {
            let packet = audio(sequence, 1);
            let coded = s.audio_at(7, 1, packet.clone(), now + Duration::from_millis(ms));
            let mut out = Vec::new();
            for p in &coded {
                out.extend(d.push(p, 7, now + Duration::from_millis(received)));
            }
            assert_eq!(out, vec![packet]);
            assert!(d
                .push(&coded[0], 7, now + Duration::from_millis(received))
                .is_empty());
        }
        d.expire(now + Duration::from_millis(81));
        let f = d.take_reports(None);
        assert_eq!((f[0].expected, f[0].received, f[0].residual), (3, 3, 0));
    }

    #[test]
    fn incomplete_group_flood_and_generation_churn_keep_state_bounded() {
        let s = sender(Key::default(), 0.3);
        let template = s.video(7, Key::default(), packets(1, 3200))[0].clone();
        let now = Instant::now();
        let mut d = Decoder::default();
        for i in 0u64..2048 {
            let mut p = template.clone();
            p[25..33].copy_from_slice(&(i * 32).to_be_bytes());
            d.push(&p, 7, now);
        }
        assert_eq!(d.groups.len(), MAX_GROUPS);
        assert!(d.memory <= MAX_MEMORY);
        d.expire(now + Duration::from_millis(21));
        assert!(d.groups.is_empty());
        assert_eq!(d.memory, 0);
        for generation in 1..100 {
            let p = s.audio_at(
                7,
                generation,
                audio(0, generation),
                now + Duration::from_millis(30),
            );
            d.push(&p[0], 7, now + Duration::from_millis(30));
            d.expire(now + Duration::from_millis(51));
        }
        assert!(d.reports.len() <= 2);
        assert!(d.retired.len() <= 2);
    }
    #[tokio::test]
    async fn actual_udp_relay_recovers_bursts_duplicates_and_reordering_on_two_screens() {
        use crate::datagram::Datagram as UdpSocket;
        use vnt_proto::{control::ControlMessage as Msg, screen};
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut rx = crate::udp::UdpVideoReceiver::bind(relay.local_addr().unwrap())
            .await
            .unwrap();
        rx.set_session(7);
        rx.set_fec(true);
        rx.retain_screens(&[1, 2]);
        let target = std::net::SocketAddr::from(([127, 0, 0, 1], rx.local_addr().unwrap().port()));
        let relay_addr = relay.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let mut buf = vec![0; MAX_PACKET];
            let mut block = Vec::new();
            loop {
                let (len, _) = relay.recv_from(&mut buf).await.unwrap();
                let p = &buf[..len];
                let (i, k, m) = (p[33] as usize, p[34] as usize, p[35] as usize);
                if m < 2 || i >= 2 {
                    block.push(p.to_vec());
                }
                if i + 1 == k + m {
                    for p in block.drain(..).rev() {
                        relay.send_to(&p, target).await.unwrap();
                        relay.send_to(&p, target).await.unwrap();
                    }
                }
            }
        });
        let fec = Sender::default();
        for id in [1, 2] {
            fec.0
                .lock()
                .unwrap()
                .lanes
                .entry(Key {
                    stream: id,
                    generation: 1,
                    audio: false,
                })
                .or_default()
                .adapt
                .ratio = 0.3;
        }
        let mut sender = crate::udp::UdpFrameSender::default();
        sender.set_fec(Some(fec));
        for id in [1, 2] {
            let data = vec![id as u8; 40_000];
            let Msg::ScreenPacket(packet) = screen::wrap(
                id,
                1,
                Msg::VideoFrame(pb::VideoFrame {
                    dependencies: None,
                    keyframe: true,
                    pts_ms: 10,
                    data: data.clone(),
                }),
            ) else {
                unreachable!()
            };
            sender
                .send_screen_frame(&tx, relay_addr, 7, 0, &packet, 1_000_000)
                .await
                .unwrap();
            let (got, generation, frame) =
                tokio::time::timeout(Duration::from_secs(1), rx.recv_screen_frame())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            assert_eq!((got, generation), (id, 1));
            assert_eq!(frame.data, data);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        for id in [1, 2] {
            let reports = rx.fec_reports(Some(id));
            assert!(reports[0].recovered >= 2);
            assert_eq!(reports[0].residual, 0);
            assert!(reports[0].received < reports[0].expected);
        }
        proxy.abort();
    }
}
