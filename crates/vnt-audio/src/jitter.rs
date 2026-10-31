use crate::opus::Decoder;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vnt_proto::{
    audio::{valid_frame, FRAME_SAMPLES},
    pb,
};

pub const MAX_BUFFER_FRAMES: usize = 6;

pub struct JitterBuffer {
    generation: u32,
    packets: BTreeMap<u64, (Instant, Arc<pb::AudioFrame>)>,
    next: Option<u64>,
    target: usize,
    missing: usize,
    stable_since: Instant,
    decoder: Decoder,
}
impl JitterBuffer {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            generation: 0,
            packets: BTreeMap::new(),
            next: None,
            target: 2,
            missing: 0,
            stable_since: Instant::now(),
            decoder: Decoder::new()?,
        })
    }
    pub fn reset(&mut self, generation: u32) -> Result<(), String> {
        self.generation = generation;
        self.packets.clear();
        self.next = None;
        self.missing = 0;
        self.target = 2;
        self.stable_since = Instant::now();
        self.decoder = Decoder::new()?;
        Ok(())
    }
    pub fn push(&mut self, packet: Arc<pb::AudioFrame>, now: Instant) {
        if packet.generation != self.generation
            || !valid_frame(&packet)
            || self
                .next
                .is_some_and(|next| packet.timestamp_samples < next)
        {
            return;
        }
        self.packets
            .entry(packet.timestamp_samples)
            .or_insert((now, packet));
        while self.packets.len() > MAX_BUFFER_FRAMES {
            self.packets.pop_first();
        }
        if let (Some(next), Some((&first, _))) = (self.next, self.packets.first_key_value()) {
            if first.saturating_sub(next) > FRAME_SAMPLES * MAX_BUFFER_FRAMES as u64 {
                self.next = Some(first); // congestion recovery must not play a long PLC backlog
            }
        }
    }
    pub fn queued_frames(&self) -> usize {
        self.packets.len()
    }
    pub fn target_frames(&self) -> usize {
        self.target
    }
    pub fn pop(&mut self, now: Instant) -> Result<Vec<[f32; 2]>, String> {
        // Trim by arrival age as well as count: a stalled render thread cannot
        // replay a previously full queue when the OS schedules it again.
        self.packets
            .retain(|_, (at, _)| now.saturating_duration_since(*at) <= Duration::from_millis(60));
        if self.next.is_none() {
            let Some((&first, (at, _))) = self.packets.first_key_value() else {
                return Ok(vec![[0.0; 2]; 480]);
            };
            if self.packets.len() < self.target
                && now.saturating_duration_since(*at)
                    < Duration::from_millis(self.target as u64 * 10)
            {
                return Ok(vec![[0.0; 2]; 480]);
            }
            self.next = Some(first);
        }
        let timestamp = self.next.unwrap();
        self.next = timestamp.checked_add(FRAME_SAMPLES);
        if let Some((_, packet)) = self.packets.remove(&timestamp) {
            self.missing = 0;
            if now.saturating_duration_since(self.stable_since) >= Duration::from_secs(5) {
                self.target = self.target.saturating_sub(1).max(2);
                self.stable_since = now;
            }
            // Corrupt payload is treated as one lost packet, not a device error.
            return self
                .decoder
                .decode(Some(&packet.data))
                .or_else(|_| self.decoder.decode(None));
        }
        self.missing += 1;
        self.stable_since = now;
        self.target = (self.target + 1).min(MAX_BUFFER_FRAMES);
        if self.missing <= 3 {
            self.decoder.decode(None)
        } else {
            self.next = None;
            Ok(vec![[0.0; 2]; 480])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::Encoder;
    fn packet(generation: u32, timestamp: u64) -> Arc<pb::AudioFrame> {
        Arc::new(pb::AudioFrame {
            generation,
            sequence: (timestamp / 480) as u32,
            timestamp_samples: timestamp,
            data: Encoder::new().unwrap().encode(&[0.0; 960]).unwrap(),
        })
    }
    #[test]
    fn reorders_deduplicates_conceals_and_isolates_generations() {
        let now = Instant::now();
        let mut j = JitterBuffer::new().unwrap();
        j.reset(1).unwrap();
        j.push(packet(1, 480), now);
        j.push(packet(1, 0), now);
        j.push(packet(1, 480), now);
        assert_eq!(j.queued_frames(), 2);
        assert_eq!(j.pop(now).unwrap().len(), 480);
        assert_eq!(j.next, Some(480));
        j.pop(now).unwrap();
        j.pop(now).unwrap(); // lost frame uses PLC
        j.push(packet(1, 0), now);
        assert_eq!(j.queued_frames(), 0); // late
        j.reset(2).unwrap();
        j.push(packet(1, 960), now);
        assert_eq!(j.queued_frames(), 0);
        j.push(packet(2, 0), now);
        j.push(packet(2, 480), now);
        j.pop(now).unwrap();
        assert_eq!(j.next, Some(480));
    }
    #[test]
    fn overflow_and_stalls_never_replay_unbounded_backlogs() {
        let now = Instant::now();
        let mut j = JitterBuffer::new().unwrap();
        j.reset(1).unwrap();
        for i in 0..100 {
            j.push(packet(1, i * 480), now);
        }
        assert_eq!(j.queued_frames(), 6);
        j.pop(now).unwrap();
        assert_eq!(j.next, Some(95 * 480));
        j.pop(now + Duration::from_millis(100)).unwrap();
        assert_eq!(j.queued_frames(), 0);
        for _ in 0..100 {
            j.pop(now + Duration::from_millis(100)).unwrap();
        }
        assert!(j.next.is_none());
        assert_eq!(j.target, 6);
    }
    #[test]
    fn sample_clock_orders_across_sequence_wrap() {
        let now = Instant::now();
        let mut j = JitterBuffer::new().unwrap();
        j.reset(1).unwrap();
        let timestamp = u32::MAX as u64 * 480;
        j.push(packet(1, timestamp + 480), now);
        j.push(packet(1, timestamp), now);
        j.pop(now).unwrap();
        j.pop(now).unwrap();
        assert_eq!(j.next, Some(timestamp + 960));
    }
}
