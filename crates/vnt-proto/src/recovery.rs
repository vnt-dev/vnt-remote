//! Protocol-v1 video repair. All time enters explicitly so deadlines can be
//! tested without sleeping. A transport loss is distinct from a broken chain.
use crate::{
    pb,
    udp::{FragHeader, ReassembledFrame, MAX_FRAGS, MAX_FRAME_BYTES},
};
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

pub const FLAG_DEPENDENCIES: u8 = 0x10;
pub const FLAG_REPAIR: u8 = 0x20;
pub const MAX_REFERENCES: usize = 32;
pub const MAX_METADATA: usize = 384;
pub const MAX_MEMORY: usize = 32 * 1024 * 1024;
pub const MAX_PENDING: usize = 16;
pub const POLL: Duration = Duration::from_millis(2);

/// Namespace encoder-local epochs across Broker worker restarts. Reference IDs
/// stay encoder-local; only the epoch changes when a producer is replaced.
#[derive(Default)]
pub struct EpochMapper {
    source: Option<(u64, u64)>,
    previous: u64,
    epoch: u64,
}
impl EpochMapper {
    pub fn map(&mut self, producer: u64, d: &mut pb::VideoDependencies) {
        let source = (producer, d.epoch);
        if self.source != Some(source) || d.frame_id <= self.previous {
            self.epoch = self.epoch.saturating_add(1);
            self.source = Some(source);
        }
        self.previous = d.frame_id;
        d.epoch = self.epoch;
    }
}

pub fn valid_dependencies(d: &pb::VideoDependencies) -> bool {
    d.epoch != 0
        && d.frame_id != 0
        && d.references.len() <= MAX_REFERENCES
        && !(d.independent && (d.unknown || !d.references.is_empty()))
        && d.references.iter().all(|&r| r != 0 && r < d.frame_id)
        && d.references.windows(2).all(|w| w[0] < w[1])
}
pub fn valid_nack(n: &pb::VideoNack) -> bool {
    n.stream_id <= 128
        && ((n.stream_id == 0) == (n.generation == 0))
        && n.epoch != 0
        && n.frame_id != 0
        && n.ranges.len() <= 128
        && n.ranges
            .iter()
            .all(|r| r.first <= r.last && r.last < MAX_FRAGS as u32)
        && n.ranges.windows(2).all(|w| w[0].last < w[1].first)
}
pub fn encode_dependencies(d: &pb::VideoDependencies, out: &mut Vec<u8>) {
    let bytes = d.encode_to_vec();
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend(bytes);
}
pub fn decode_dependencies(bytes: &[u8]) -> Option<(pb::VideoDependencies, &[u8])> {
    let len = u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?) as usize;
    if len > MAX_METADATA {
        return None;
    }
    let d = pb::VideoDependencies::decode(bytes.get(2..2 + len)?).ok()?;
    valid_dependencies(&d).then_some((d, &bytes[2 + len..]))
}
pub fn budget(rtt: Option<u32>) -> Duration {
    Duration::from_millis(
        rtt.map_or(30, |r| r.saturating_mul(2).saturating_add(8).clamp(20, 50)) as u64,
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Route {
    pub stream: u32,
    pub generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BreakReason {
    MissingReference,
    UnknownDependencies,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub nack_requests: u64,
    /// Holes observed at the first NACK, retained even after repair succeeds.
    /// Entirely absent frames have an unknown fragment count and are separate.
    pub observed_missing_fragments: u64,
    pub observed_missing_whole_frames: u64,
    pub recovered_frames: u64,
    pub timed_out_frames: u64,
    pub lost_frames: u64,
    pub skipped_frames: u64,
    pub late_fragments: u64,
    pub idr_requests: u64,
    pub idr_missing_reference: u64,
    pub idr_unknown_dependencies: u64,
}

struct Pending {
    meta: Option<pb::VideoDependencies>,
    header: Option<FragHeader>,
    fragments: Vec<Option<Vec<u8>>>,
    stride: Option<usize>,
    bytes: usize,
    // Suspicion begins on a sequence gap, an observed fragment hole, or quiet
    // tail. New/duplicate fragments never move a recovery deadline.
    last_progress: Instant,
    suspected: Option<Instant>,
    deadline: Option<Instant>,
    next_nack: Instant,
    requests: u8,
}
impl Pending {
    fn missing(now: Instant, wait: Duration) -> Self {
        Self {
            meta: None,
            header: None,
            fragments: vec![],
            stride: None,
            bytes: 0,
            last_progress: now,
            suspected: Some(now),
            deadline: Some(now + wait),
            next_nack: now + Duration::from_millis(5),
            requests: 0,
        }
    }
    fn complete(&self) -> bool {
        !self.fragments.is_empty() && self.fragments.iter().all(Option::is_some)
    }
    fn suspect(&mut self, now: Instant, wait: Duration) {
        if self.suspected.is_none() {
            self.suspected = Some(now);
            self.deadline = Some(now + wait);
            self.next_nack = now + Duration::from_millis(5);
        }
    }
    fn allocation(&self) -> usize {
        self.bytes + self.fragments.len() * std::mem::size_of::<Option<Vec<u8>>>() + 512
    }
}

struct Stream {
    epoch: u64,
    retired: u64,
    anchor: u64,
    maximum: u64,
    // Only failed IDs need storage: everything else up to retired was delivered.
    failed: BTreeSet<u64>,
    history_overflow: bool,
    pending: BTreeMap<u64, Pending>,
    broken: Option<BreakReason>,
    last_idr: Option<Instant>,
    stats: Stats,
}
impl Stream {
    fn new(epoch: u64) -> Self {
        Self {
            epoch,
            retired: 0,
            anchor: 0,
            maximum: 0,
            failed: BTreeSet::new(),
            history_overflow: false,
            pending: BTreeMap::new(),
            broken: None,
            last_idr: None,
            stats: Stats::default(),
        }
    }
    fn fail(&mut self, id: u64) {
        self.retired = self.retired.max(id);
        if self.failed.len() < 4096 {
            self.failed.insert(id);
        } else {
            self.history_overflow = true;
        }
        self.stats.lost_frames += 1;
    }
}

#[derive(Default)]
pub struct Receiver {
    streams: BTreeMap<Route, Stream>,
    ready: VecDeque<(Route, ReassembledFrame)>,
    rtt: Option<u32>,
}
impl Receiver {
    pub fn set_rtt(&mut self, rtt: Option<u32>) {
        self.rtt = rtt;
    }
    pub fn clear(&mut self) {
        self.streams.clear();
        self.ready.clear();
    }
    pub fn retain(&mut self, ids: &[u32]) {
        self.streams
            .retain(|r, _| r.stream == 0 || ids.contains(&r.stream));
        self.ready
            .retain(|(r, _)| r.stream == 0 || ids.contains(&r.stream));
    }
    pub fn stats(&self, stream: u32) -> Stats {
        self.streams
            .iter()
            .find(|(r, _)| r.stream == stream)
            .map_or(Stats::default(), |(_, s)| s.stats)
    }
    pub fn diagnostics(&self) -> Vec<(Route, Stats)> {
        self.streams
            .iter()
            .map(|(route, stream)| (*route, stream.stats))
            .collect()
    }
    pub fn pop(&mut self) -> Option<(Route, ReassembledFrame)> {
        self.ready.pop_front()
    }

    pub fn push(
        &mut self,
        route: Route,
        h: FragHeader,
        meta: pb::VideoDependencies,
        data: &[u8],
        now: Instant,
    ) {
        if !valid_dependencies(&meta)
            || data.is_empty()
            || data.len() > crate::udp::DEFAULT_MTU
            || route.stream > 128
            || (route.stream == 0) != (route.generation == 0)
            || h.frag_count == 0
            || h.frag_idx >= h.frag_count
            || (meta.independent && !h.is_keyframe())
        {
            return;
        }
        // Expire before accepting a packet: a late last fragment must not turn
        // a frame whose hard deadline passed into a successful recovery.
        self.advance(now);
        // A screen generation supersedes every older epoch/repair for that screen.
        if self
            .streams
            .keys()
            .any(|r| r.stream == route.stream && r.generation > route.generation)
        {
            return;
        }
        let previous_stats = self.stats(route.stream);
        self.streams
            .retain(|r, _| r.stream != route.stream || r.generation == route.generation);
        self.ready
            .retain(|(r, _)| r.stream != route.stream || r.generation == route.generation);
        let s = self.streams.entry(route).or_insert_with(|| {
            let mut s = Stream::new(meta.epoch);
            s.stats = previous_stats;
            s
        });
        if meta.epoch < s.epoch {
            return;
        }
        if meta.epoch > s.epoch {
            self.ready.retain(|(ready_route, _)| *ready_route != route);
            let stats = s.stats;
            *s = Stream::new(meta.epoch);
            s.stats = stats;
        }
        let id = meta.frame_id;
        if id <= s.retired {
            s.stats.late_fragments += 1;
            return;
        }
        let wait = budget(self.rtt);
        // Joining at an independent frame does not imply loss of every frame
        // encoded before UDP was bound (or before a screen was opened).
        if s.maximum == 0 && meta.independent {
            s.maximum = id - 1;
            s.retired = id - 1;
        }
        if id > s.maximum {
            // Bound malicious jumps; unknown skipped history remains unsafe until
            // an independently decodable frame arrives.
            let start = (s.maximum + 1).max(id.saturating_sub(MAX_PENDING as u64));
            if start > s.maximum + 1 {
                s.history_overflow = true;
            }
            for missing in start..id {
                s.pending
                    .entry(missing)
                    .or_insert_with(|| Pending::missing(now, wait));
            }
            for (_, p) in s.pending.range_mut(..id) {
                if !p.complete() {
                    p.suspect(now, wait);
                }
            }
            s.maximum = id;
        }
        let p = s.pending.entry(id).or_insert_with(|| Pending {
            meta: None,
            header: None,
            fragments: vec![],
            stride: None,
            bytes: 0,
            last_progress: now,
            suspected: None,
            deadline: None,
            next_nack: now,
            requests: 0,
        });
        if let Some(previous) = &p.meta {
            if previous != &meta
                || p.header.is_some_and(|old| {
                    old.frag_count != h.frag_count
                        || old.pts_ms != h.pts_ms
                        || old.is_keyframe() != h.is_keyframe()
                        || old.frame_seq != h.frame_seq
                })
            {
                return;
            }
        } else {
            p.meta = Some(meta);
            p.header = Some(h);
            p.fragments.resize(h.frag_count as usize, None);
        }
        let idx = h.frag_idx as usize;
        if p.fragments.get(idx).is_none() || p.fragments[idx].is_some() {
            return;
        }
        if p.bytes + data.len() > MAX_FRAME_BYTES {
            return;
        }
        if idx + 1 < p.fragments.len() {
            if p.stride.is_some_and(|stride| stride != data.len())
                || p.fragments
                    .last()
                    .and_then(Option::as_ref)
                    .is_some_and(|tail| tail.len() > data.len())
            {
                return;
            }
            p.stride = Some(data.len());
        } else if p.stride.is_some_and(|stride| data.len() > stride) {
            return;
        }
        if p.fragments[..idx].iter().any(Option::is_none) {
            p.suspect(now, wait);
        }
        p.bytes += data.len();
        p.fragments[idx] = Some(data.to_vec());
        p.last_progress = now;
        self.enforce_limits();
        self.advance(now);
    }

    fn enforce_limits(&mut self) {
        loop {
            let count: usize = self.ready.len()
                + self
                    .streams
                    .values()
                    .map(|s| s.pending.len())
                    .sum::<usize>();
            let memory: usize = self
                .ready
                .iter()
                .map(|(_, f)| f.data.len() + 512)
                .sum::<usize>()
                + self
                    .streams
                    .values()
                    .flat_map(|s| s.pending.values())
                    .map(Pending::allocation)
                    .sum::<usize>();
            if count <= MAX_PENDING && memory <= MAX_MEMORY {
                break;
            }
            let oldest = self
                .streams
                .iter()
                .flat_map(|(r, s)| {
                    s.pending
                        .iter()
                        .map(move |(id, p)| (*r, *id, p.last_progress))
                })
                .min_by_key(|(_, _, at)| *at)
                .map(|(r, id, _)| (r, id));
            let Some((r, id)) = oldest else {
                break;
            };
            let s = self.streams.get_mut(&r).unwrap();
            // Retire in decode order, even if a newer pending frame happened to
            // have less recent progress than an older one.
            let ids: Vec<_> = s.pending.range(..=id).map(|(&i, _)| i).collect();
            for id in ids {
                s.pending.remove(&id);
                s.fail(id);
            }
        }
    }

    pub fn advance(&mut self, now: Instant) {
        let wait = budget(self.rtt);
        for (route, s) in &mut self.streams {
            // A complete reset supersedes any blocked older data. Never request
            // an IDR if one already in hand can restore the decoder.
            if let Some(id) = s
                .pending
                .iter()
                .find(|(_, p)| p.complete() && p.meta.as_ref().is_some_and(|d| d.independent))
                .map(|(&id, _)| id)
            {
                let older: Vec<_> = s.pending.range(..id).map(|(&id, _)| id).collect();
                for id in older {
                    if let Some(p) = s.pending.remove(&id) {
                        if !p.complete() {
                            s.fail(id);
                        }
                    }
                }
            }
            while let Some((&id, p)) = s.pending.first_key_value() {
                if !p.complete() {
                    let p = s.pending.get_mut(&id).unwrap();
                    if now.saturating_duration_since(p.last_progress) >= Duration::from_millis(20) {
                        p.suspect(now, wait);
                    }
                    if p.deadline.is_none_or(|deadline| now < deadline) {
                        break;
                    }
                    s.pending.remove(&id);
                    s.stats.timed_out_frames += 1;
                    s.fail(id);
                    continue;
                }
                let p = s.pending.remove(&id).unwrap();
                let d = p.meta.unwrap();
                let h = p.header.unwrap();
                let reason = if d.independent {
                    None
                } else if d.unknown
                    && (s.anchor == 0
                        || !s.failed.is_empty()
                        || s.history_overflow
                        || s.broken.is_some())
                {
                    Some(BreakReason::UnknownDependencies)
                } else if !d.unknown
                    && (s.anchor == 0
                        || s.history_overflow
                        || d.references
                            .iter()
                            .any(|r| *r < s.anchor || *r > s.retired || s.failed.contains(r)))
                {
                    Some(BreakReason::MissingReference)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    s.broken = Some(reason);
                    s.stats.skipped_frames += 1;
                    s.fail(id);
                    // The transport delivered this frame intact. Keep decoder
                    // suppression separate from the network-loss feedback.
                    s.stats.lost_frames -= 1;
                    continue;
                }
                if d.independent {
                    s.anchor = id;
                    s.failed.clear();
                    s.history_overflow = false;
                    s.broken = None;
                    s.last_idr = None;
                }
                if p.requests > 0 {
                    s.stats.recovered_frames += 1;
                }
                s.retired = id;
                self.ready.push_back((
                    *route,
                    ReassembledFrame {
                        keyframe: h.is_keyframe(),
                        pts_ms: h.pts_ms,
                        frame_seq: h.frame_seq,
                        data: p.fragments.into_iter().flatten().flatten().collect(),
                    },
                ));
            }
        }
    }

    pub fn nacks(&mut self, now: Instant) -> Vec<pb::VideoNack> {
        self.advance(now);
        let rtt = Duration::from_millis(self.rtt.unwrap_or(10).max(1) as u64);
        let mut out = Vec::new();
        for (route, s) in &mut self.streams {
            for (&id, p) in &mut s.pending {
                if p.complete()
                    || p.requests >= 2
                    || now < p.next_nack
                    || p.deadline.is_none_or(|end| now + rtt > end)
                {
                    continue;
                }
                let mut ranges: Vec<pb::VideoFragmentRange> = Vec::new();
                for (i, fragment) in p.fragments.iter().enumerate() {
                    if fragment.is_none() {
                        if let Some(last) = ranges.last_mut().filter(|r| r.last + 1 == i as u32) {
                            last.last = i as u32;
                        } else {
                            ranges.push(pb::VideoFragmentRange {
                                first: i as u32,
                                last: i as u32,
                            });
                        }
                    }
                }
                // Fragmented holes can exceed one bounded NACK; request the
                // whole frame instead of silently omitting later holes.
                if ranges.len() > 128 {
                    ranges.clear();
                }
                if p.requests == 0 {
                    if p.fragments.is_empty() {
                        s.stats.observed_missing_whole_frames += 1;
                    } else {
                        s.stats.observed_missing_fragments +=
                            p.fragments.iter().filter(|f| f.is_none()).count() as u64;
                    }
                }
                out.push(pb::VideoNack {
                    stream_id: route.stream,
                    generation: route.generation,
                    epoch: s.epoch,
                    frame_id: id,
                    ranges,
                });
                p.requests += 1;
                p.next_nack = now + rtt.max(Duration::from_millis(5));
                s.stats.nack_requests += 1;
            }
        }
        out
    }
    pub fn idrs(&mut self, now: Instant) -> Vec<(Route, BreakReason)> {
        self.advance(now);
        self.streams
            .iter_mut()
            .filter_map(|(r, s)| {
                let reason = s.broken?;
                if s.last_idr.is_some_and(|last| {
                    now.saturating_duration_since(last) < Duration::from_millis(500)
                }) {
                    return None;
                }
                s.last_idr = Some(now);
                s.stats.idr_requests += 1;
                match reason {
                    BreakReason::MissingReference => s.stats.idr_missing_reference += 1,
                    BreakReason::UnknownDependencies => s.stats.idr_unknown_dependencies += 1,
                }
                Some((*r, reason))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn meta(id: u64, refs: &[u64]) -> pb::VideoDependencies {
        pb::VideoDependencies {
            epoch: 1,
            frame_id: id,
            references: refs.to_vec(),
            independent: id == 1,
            unknown: false,
        }
    }
    fn fragment(r: &mut Receiver, id: u64, refs: &[u64], index: u16, count: u16, now: Instant) {
        r.push(
            Route::default(),
            FragHeader {
                session_id: 7,
                frame_seq: id as u16,
                frag_idx: index,
                frag_count: count,
                flags: u8::from(id == 1),
                pts_ms: id as u32,
            },
            meta(id, refs),
            &[index as u8; 16],
            now,
        );
    }
    fn started(now: Instant) -> Receiver {
        let mut r = Receiver::default();
        fragment(&mut r, 1, &[], 0, 1, now);
        assert!(r.pop().is_some());
        r
    }
    #[test]
    fn budgets_are_bounded() {
        assert_eq!(budget(None), Duration::from_millis(30));
        for (rtt, expected) in [(0, 20), (10, 28), (100, 50), (u32::MAX, 50)] {
            assert_eq!(budget(Some(rtt)), Duration::from_millis(expected));
        }
    }
    #[test]
    fn encoder_restarts_get_unique_epochs_and_metadata_and_nacks_are_validated() {
        let mut mapper = EpochMapper::default();
        let mut d = meta(1, &[]);
        mapper.map(1, &mut d);
        let first = d.epoch;
        let mut next = meta(2, &[1]);
        mapper.map(1, &mut next);
        assert_eq!(next.epoch, first);
        let mut restarted = meta(1, &[]);
        mapper.map(1, &mut restarted);
        assert!(restarted.epoch > first);
        let mut reconfigured = meta(2, &[1]);
        mapper.map(2, &mut reconfigured);
        assert!(reconfigured.epoch > restarted.epoch);
        let mut packet = vec![];
        encode_dependencies(&next, &mut packet);
        packet.extend_from_slice(&[7; 16]);
        assert_eq!(decode_dependencies(&packet), Some((next, &[7; 16][..])));
        assert!(decode_dependencies(&[255, 255]).is_none());
        let n = pb::VideoNack {
            stream_id: 2,
            generation: 3,
            epoch: 4,
            frame_id: 5,
            ranges: vec![pb::VideoFragmentRange { first: 1, last: 3 }],
        };
        let wire = crate::control::ControlMessage::VideoNack(n.clone()).encode_frame();
        let (kind, len) = crate::control::parse_header(wire[..5].try_into().unwrap()).unwrap();
        assert_eq!(len, wire.len() - 5);
        assert_eq!(
            crate::control::decode_message(kind, &wire[5..]).unwrap(),
            crate::control::ControlMessage::VideoNack(n.clone())
        );
        let mut bad = n;
        bad.ranges
            .push(pb::VideoFragmentRange { first: 2, last: 4 });
        assert!(crate::control::decode_message(kind, &bad.encode_to_vec()).is_err());
    }
    #[test]
    fn fragment_repair_wins_before_deadline_without_idr_or_loss() {
        let t = Instant::now();
        let mut r = started(t);
        fragment(&mut r, 2, &[1], 0, 3, t);
        fragment(&mut r, 2, &[1], 2, 3, t);
        assert!(r.nacks(t + Duration::from_millis(4)).is_empty());
        let nacks = r.nacks(t + Duration::from_millis(5));
        assert_eq!(nacks.len(), 1);
        assert_eq!((nacks[0].ranges[0].first, nacks[0].ranges[0].last), (1, 1));
        fragment(&mut r, 2, &[1], 1, 3, t + Duration::from_millis(12));
        assert_eq!(r.pop().unwrap().1.frame_seq, 2);
        assert!(r.idrs(t + Duration::from_secs(1)).is_empty());
        assert_eq!(r.stats(0).lost_frames, 0);
        assert_eq!(r.stats(0).recovered_frames, 1);
        assert_eq!(r.stats(0).observed_missing_fragments, 1);
    }
    #[test]
    fn joining_at_an_independent_frame_does_not_count_unsent_history_as_loss() {
        let t = Instant::now();
        let mut r = Receiver::default();
        let mut d = meta(500, &[]);
        d.independent = true;
        for index in [1, 0] {
            r.push(
                Route::default(),
                FragHeader {
                    session_id: 7,
                    frame_seq: 0,
                    frag_idx: index,
                    frag_count: 2,
                    flags: 1,
                    pts_ms: 0,
                },
                d.clone(),
                &[1; 16],
                t,
            );
        }
        assert!(r.pop().is_some());
        assert_eq!(r.stats(0).lost_frames, 0);
        assert!(r.nacks(t + Duration::from_millis(5)).is_empty());
        assert!(r.idrs(t + Duration::from_secs(1)).is_empty());
    }
    #[test]
    fn missing_nonreference_picture_does_not_poison_later_references() {
        let t = Instant::now();
        let mut r = started(t);
        fragment(&mut r, 3, &[1], 0, 1, t);
        assert_eq!(r.nacks(t + Duration::from_millis(5))[0].frame_id, 2);
        r.advance(t + Duration::from_millis(30));
        assert_eq!(r.pop().unwrap().1.frame_seq, 3);
        assert_eq!(r.stats(0).lost_frames, 1);
        assert!(r.idrs(t + Duration::from_secs(1)).is_empty());
    }
    #[test]
    fn broken_chain_is_gated_retried_and_reset_is_used_before_requesting_idr() {
        let t = Instant::now();
        let mut r = started(t);
        fragment(&mut r, 3, &[2], 0, 1, t);
        assert!(r.idrs(t + Duration::from_millis(29)).is_empty());
        assert_eq!(
            r.idrs(t + Duration::from_millis(30))[0].1,
            BreakReason::MissingReference
        );
        assert!(r.pop().is_none());
        fragment(&mut r, 4, &[3], 0, 1, t + Duration::from_millis(31));
        assert!(r.pop().is_none());
        assert!(r.idrs(t + Duration::from_millis(529)).is_empty());
        assert_eq!(r.idrs(t + Duration::from_millis(530)).len(), 1);
        let mut d = meta(5, &[]);
        d.independent = true;
        r.push(
            Route::default(),
            FragHeader {
                session_id: 7,
                frame_seq: 5,
                frag_idx: 0,
                frag_count: 1,
                flags: 1,
                pts_ms: 5,
            },
            d,
            &[5],
            t + Duration::from_millis(531),
        );
        assert_eq!(r.pop().unwrap().1.frame_seq, 5);
        assert!(r.idrs(t + Duration::from_secs(2)).is_empty());
        // An already complete reset must supersede a pending broken chain.
        let mut r = started(t);
        fragment(&mut r, 3, &[2], 0, 1, t);
        let mut d = meta(4, &[]);
        d.independent = true;
        r.push(
            Route::default(),
            FragHeader {
                session_id: 7,
                frame_seq: 4,
                frag_idx: 0,
                frag_count: 1,
                flags: 1,
                pts_ms: 4,
            },
            d,
            &[4],
            t,
        );
        assert_eq!(r.pop().unwrap().1.frame_seq, 4);
        assert!(r.idrs(t + Duration::from_secs(1)).is_empty());
    }
    #[test]
    fn duplicates_and_progress_do_not_extend_recovery_deadline() {
        let t = Instant::now();
        let mut r = started(t);
        fragment(&mut r, 2, &[1], 0, 4, t);
        fragment(&mut r, 2, &[1], 2, 4, t);
        for ms in [10, 20, 29] {
            fragment(&mut r, 2, &[1], 2, 4, t + Duration::from_millis(ms));
        }
        fragment(&mut r, 2, &[1], 1, 4, t + Duration::from_millis(29));
        r.advance(t + Duration::from_millis(30));
        assert_eq!(r.stats(0).timed_out_frames, 1);
        fragment(&mut r, 2, &[1], 3, 4, t + Duration::from_millis(31));
        assert!(r.pop().is_none());
        assert_eq!(r.stats(0).late_fragments, 1);
        assert!(r.nacks(t + Duration::from_millis(31)).is_empty());
    }
    #[test]
    fn late_last_fragment_is_expired_without_an_explicit_timer_tick() {
        let t = Instant::now();
        let mut r = started(t);
        fragment(&mut r, 2, &[1], 0, 3, t);
        fragment(&mut r, 2, &[1], 2, 3, t);
        fragment(&mut r, 2, &[1], 1, 3, t + Duration::from_millis(31));
        assert!(r.pop().is_none());
        assert_eq!(r.stats(0).lost_frames, 1);
        assert_eq!(r.stats(0).late_fragments, 1);
    }
    #[test]
    fn a_broken_screen_does_not_block_another_and_new_generations_reject_old_repairs() {
        let t = Instant::now();
        let mut r = Receiver::default();
        let push = |r: &mut Receiver, route, id, refs: &[u64], now| {
            let d = meta(id, refs);
            r.push(
                route,
                FragHeader {
                    session_id: 7,
                    frame_seq: id as u16,
                    frag_idx: 0,
                    frag_count: 1,
                    flags: u8::from(id == 1),
                    pts_ms: id as u32,
                },
                d,
                &[id as u8],
                now,
            );
        };
        let a = Route {
            stream: 1,
            generation: 1,
        };
        let b = Route {
            stream: 2,
            generation: 1,
        };
        push(&mut r, a, 1, &[], t);
        push(&mut r, b, 1, &[], t);
        assert!(r.pop().is_some());
        assert!(r.pop().is_some());
        push(&mut r, a, 3, &[2], t);
        push(&mut r, b, 2, &[1], t);
        assert_eq!(r.pop().unwrap().0, b);
        assert_eq!(r.idrs(t + Duration::from_millis(30))[0].0, a);
        let new = Route {
            stream: 1,
            generation: 2,
        };
        push(&mut r, new, 1, &[], t + Duration::from_millis(31));
        assert_eq!(r.pop().unwrap().0, new);
        push(&mut r, a, 4, &[1], t + Duration::from_millis(32));
        assert!(r.pop().is_none());
        assert!(r.idrs(t + Duration::from_secs(1)).is_empty());
    }
    #[test]
    fn quiet_tail_and_unknown_metadata_fail_closed() {
        let t = Instant::now();
        let mut r = started(t);
        fragment(&mut r, 2, &[1], 0, 2, t);
        assert!(r.nacks(t + Duration::from_millis(20)).is_empty());
        assert_eq!(r.nacks(t + Duration::from_millis(25)).len(), 1);
        r.advance(t + Duration::from_millis(50));
        let mut d = meta(3, &[]);
        d.unknown = true;
        r.push(
            Route::default(),
            FragHeader {
                session_id: 7,
                frame_seq: 3,
                frag_idx: 0,
                frag_count: 1,
                flags: 0,
                pts_ms: 3,
            },
            d,
            &[3],
            t + Duration::from_millis(51),
        );
        assert_eq!(
            r.idrs(t + Duration::from_millis(51))[0].1,
            BreakReason::UnknownDependencies
        );
    }
    #[test]
    fn rtt_too_large_skips_futile_requests_and_retries_are_bounded() {
        let t = Instant::now();
        let mut r = started(t);
        r.set_rtt(Some(100));
        fragment(&mut r, 3, &[2], 0, 1, t);
        assert!(r.nacks(t + Duration::from_millis(5)).is_empty());
        assert_eq!(r.idrs(t + Duration::from_millis(50)).len(), 1);
        let mut r = started(t);
        fragment(&mut r, 3, &[1], 0, 1, t);
        assert_eq!(r.nacks(t + Duration::from_millis(5)).len(), 1);
        assert_eq!(r.nacks(t + Duration::from_millis(15)).len(), 1);
        assert!(r.nacks(t + Duration::from_millis(20)).is_empty());
    }
    #[test]
    fn routes_epochs_metadata_validation_and_memory_are_bounded() {
        let t = Instant::now();
        let mut r = Receiver::default();
        for stream in 1..=20 {
            let d = meta(1, &[]);
            r.push(
                Route {
                    stream,
                    generation: 2,
                },
                FragHeader {
                    session_id: 7,
                    frame_seq: 65535,
                    frag_idx: 0,
                    frag_count: 65535,
                    flags: 1,
                    pts_ms: 1,
                },
                d,
                &[1],
                t,
            );
        }
        assert!(r.streams.values().map(|s| s.pending.len()).sum::<usize>() <= MAX_PENDING);
        assert!(
            r.streams
                .values()
                .flat_map(|s| s.pending.values())
                .map(Pending::allocation)
                .sum::<usize>()
                <= MAX_MEMORY
        );
        let mut d = meta(2, &[1]);
        d.epoch = 2;
        r.push(
            Route {
                stream: 1,
                generation: 3,
            },
            FragHeader {
                session_id: 7,
                frame_seq: 0,
                frag_idx: 0,
                frag_count: 1,
                flags: 0,
                pts_ms: 2,
            },
            d,
            &[2],
            t,
        );
        assert!(!r.streams.contains_key(&Route {
            stream: 1,
            generation: 2
        }));
        r.retain(&[1]);
        assert_eq!(r.streams.len(), 1);
        assert!(!valid_dependencies(&meta(2, &[2])));
        let n = pb::VideoNack {
            stream_id: 1,
            generation: 0,
            epoch: 1,
            frame_id: 1,
            ranges: vec![],
        };
        assert!(!valid_nack(&n));
    }
}
