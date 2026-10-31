use crate::datagram::Datagram as UdpSocket;
// Per-connection repair cache and wire budget, shared by all screen senders.
use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, RwLock};
use vnt_proto::{
    pb,
    recovery::{self, Route},
    udp::FragHeader,
};

const TTL: Duration = Duration::from_millis(150);
const MAX_BYTES: usize = 32 * 1024 * 1024;
type Key = (Route, u64, u64);
struct Request {
    nack: pb::VideoNack,
    at: tokio::time::Instant,
}
struct Cached {
    key: Key,
    created: Instant,
    target: SocketAddr,
    packets: BTreeMap<u16, (Vec<u8>, u8)>,
    bytes: usize,
    peak_kbps: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> VideoRepair {
        let (requests, _rx) = mpsc::channel(32);
        VideoRepair {
            state: Default::default(),
            requests,
        }
    }
    fn cache(r: &VideoRepair, id: u64, idx: u16) {
        let h = FragHeader {
            session_id: 7,
            frame_seq: id as u16,
            frag_idx: idx,
            frag_count: 3,
            flags: recovery::FLAG_DEPENDENCIES,
            pts_ms: 0,
        };
        let d = pb::VideoDependencies {
            epoch: 2,
            frame_id: id,
            references: vec![],
            independent: false,
            unknown: true,
        };
        let mut packet = vec![0; 1415];
        h.encode_to(&mut packet);
        r.cache(
            Route::default(),
            &d,
            h,
            &packet,
            "127.0.0.1:1234".parse().unwrap(),
            1000,
        );
    }
    fn nack() -> pb::VideoNack {
        pb::VideoNack {
            stream_id: 0,
            generation: 0,
            epoch: 2,
            frame_id: 1,
            ranges: vec![pb::VideoFragmentRange { first: 1, last: 1 }],
        }
    }
    #[test]
    fn cache_is_scoped_bounded_expires_and_limits_duplicate_requests() {
        let r = fixture();
        let peer = "127.0.0.1:1234".parse().unwrap();
        for idx in 0..3 {
            cache(&r, 1, idx);
        }
        let n = nack();
        let packets = r.take(&n, peer, Instant::now());
        assert_eq!(packets.len(), 1);
        assert_ne!(packets[0].0[10] & recovery::FLAG_REPAIR, 0);
        assert_eq!(FragHeader::decode(&packets[0].0).unwrap().0.frag_idx, 1);
        assert_eq!(r.take(&n, peer, Instant::now()).len(), 1);
        assert!(r.take(&n, peer, Instant::now()).is_empty());
        let mut n = nack();
        n.epoch = 3;
        assert!(r.take(&n, peer, Instant::now()).is_empty());
        n.epoch = 2;
        n.ranges.clear();
        assert!(r
            .take(&n, "127.0.0.1:5678".parse().unwrap(), Instant::now())
            .is_empty());
        assert_eq!(r.take(&n, peer, Instant::now()).len(), 2);
        assert!(r.take(&n, peer, Instant::now() + TTL).is_empty());
        assert_eq!(r.state.lock().unwrap().bytes, 0);
        for id in 1..=24000 {
            cache(&r, id, 0);
        }
        let mut state = r.state.lock().unwrap();
        assert!(state.bytes <= MAX_BYTES);
        state.expire(Instant::now() + TTL);
        assert!(state.frames.is_empty());
    }
}
#[derive(Default)]
struct State {
    frames: VecDeque<Cached>,
    bytes: usize,
    next: Option<tokio::time::Instant>,
    rates: BTreeMap<Route, (u32, Instant)>,
}
impl State {
    fn expire(&mut self, now: Instant) {
        self.rates
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < TTL);
        while self.frames.front().is_some_and(|f| {
            now.saturating_duration_since(f.created) >= TTL || self.bytes > MAX_BYTES
        }) {
            self.bytes -= self.frames.pop_front().unwrap().bytes;
        }
    }
}

#[derive(Clone)]
pub struct VideoRepair {
    state: Arc<Mutex<State>>,
    requests: mpsc::Sender<Request>,
}
/// Own this guard for the lifetime of the authenticated connection.
pub struct RepairTask(tokio::task::JoinHandle<()>);
impl Drop for RepairTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl VideoRepair {
    pub fn start(
        socket: Arc<UdpSocket>,
        peer: Arc<RwLock<Option<SocketAddr>>>,
    ) -> (Self, RepairTask) {
        let (requests, mut rx) = mpsc::channel::<Request>(32);
        let repair = Self {
            state: Default::default(),
            requests,
        };
        let worker = repair.clone();
        let task = tokio::spawn(async move {
            let mut expiry = tokio::time::interval(Duration::from_millis(25));
            loop {
                let request = tokio::select! {
                    request = rx.recv() => { let Some(request) = request else { break; }; request },
                    _ = expiry.tick() => { worker.state.lock().unwrap().expire(Instant::now()); continue; },
                };
                let deadline = request.at + Duration::from_millis(50);
                if tokio::time::Instant::now() >= deadline {
                    continue;
                }
                let Some(target) = *peer.read().await else {
                    continue;
                };
                let packets = worker.take(&request.nack, target, Instant::now());
                let peak = packets.first().map_or(100, |(_, peak)| *peak);
                let mut batch = 0;
                for (packet, peak) in packets {
                    // No stale request may monopolize the wire or delay input.
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    let result = tokio::time::timeout_at(deadline, async {
                        let burst = (peak.max(100) as usize / 2).clamp(127, 128 * 1024);
                        if batch + packet.len() + 48 > burst {
                            worker.pace(batch, peak).await;
                            batch = 0;
                        }
                        // A rebind invalidates the old destination/cache entry.
                        if *peer.read().await == Some(target) {
                            socket.send_to(&packet, target).await?;
                            batch += packet.len() + 48;
                        }
                        Ok::<_, std::io::Error>(())
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        break;
                    }
                }
                // Charge short repairs as well; otherwise repeated one-packet
                // NACKs could consume bandwidth outside the shared budget.
                if batch > 0 {
                    let _ = tokio::time::timeout_at(deadline, worker.pace(batch, peak)).await;
                }
            }
        });
        (repair, RepairTask(task))
    }
    pub fn request(&self, nack: pb::VideoNack) {
        if recovery::valid_nack(&nack) {
            let _ = self.requests.try_send(Request {
                nack,
                at: tokio::time::Instant::now(),
            });
        }
    }
    pub(crate) async fn pace(&self, bytes: usize, peak: u32) {
        let at = {
            let mut s = self.state.lock().unwrap();
            let now = tokio::time::Instant::now();
            // Screens retain their own original pacing. The connection-wide
            // budget is their aggregate, with retransmissions charged to it.
            let peak = s
                .rates
                .values()
                .fold(0u32, |sum, (rate, _)| sum.saturating_add(*rate))
                .max(peak)
                .max(100);
            // Four ms of burst credit, shared across originals and repair.
            let next = s.next.unwrap_or(now).max(now - Duration::from_millis(4));
            s.next =
                Some(next + Duration::from_secs_f64(bytes as f64 * 8.0 / (peak as f64 * 1000.0)));
            next
        };
        tokio::time::sleep_until(at).await;
    }
    pub(crate) fn cache(
        &self,
        route: Route,
        d: &pb::VideoDependencies,
        h: FragHeader,
        packet: &[u8],
        target: SocketAddr,
        peak_kbps: u32,
    ) {
        let now = Instant::now();
        let key = (route, d.epoch, d.frame_id);
        let mut s = self.state.lock().unwrap();
        s.expire(now);
        s.rates
            .retain(|r, _| r.stream != route.stream || *r == route);
        s.rates.insert(route, (peak_kbps, now));
        // Remove obsolete encoder epochs and screen generations immediately.
        let mut removed = 0;
        s.frames.retain(|f| {
            let stale = f.key.0.stream == route.stream
                && (f.key.0 != route || f.key.1 != d.epoch || f.target != target);
            if stale {
                removed += f.bytes;
            }
            !stale
        });
        s.bytes -= removed;
        if !s.frames.iter().any(|f| f.key == key) {
            let overhead = std::mem::size_of::<Cached>() + 128;
            s.frames.push_back(Cached {
                key,
                created: now,
                target,
                packets: BTreeMap::new(),
                bytes: overhead,
                peak_kbps,
            });
            s.bytes += overhead;
        }
        let f = s.frames.iter_mut().find(|f| f.key == key).unwrap();
        if f.packets.contains_key(&h.frag_idx) {
            return;
        }
        // Include tree/vector overhead in the bound, not just media payload.
        let allocation = packet.len() + 128;
        f.packets.insert(h.frag_idx, (packet.to_vec(), 0));
        f.bytes += allocation;
        s.bytes += allocation;
        s.expire(now);
    }
    fn take(&self, nack: &pb::VideoNack, target: SocketAddr, now: Instant) -> Vec<(Vec<u8>, u32)> {
        if !recovery::valid_nack(nack) {
            return Vec::new();
        }
        let key = (
            Route {
                stream: nack.stream_id,
                generation: nack.generation,
            },
            nack.epoch,
            nack.frame_id,
        );
        let mut s = self.state.lock().unwrap();
        s.expire(now);
        let Some(f) = s
            .frames
            .iter_mut()
            .find(|f| f.key == key && f.target == target)
        else {
            return Vec::new();
        };
        f.packets
            .iter_mut()
            .filter_map(|(&idx, (packet, attempts))| {
                if *attempts >= 2
                    || !nack.ranges.is_empty()
                        && !nack
                            .ranges
                            .iter()
                            .any(|r| (r.first..=r.last).contains(&(idx as u32)))
                {
                    return None;
                }
                *attempts += 1;
                let mut packet = packet.clone();
                packet[10] |= recovery::FLAG_REPAIR;
                Some((packet, f.peak_kbps))
            })
            .collect()
    }
}
