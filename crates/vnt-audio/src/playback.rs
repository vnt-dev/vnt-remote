use crate::{
    format::Resampler,
    jitter::{JitterBuffer, MAX_BUFFER_FRAMES},
    Worker,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use vnt_proto::pb;

#[derive(Clone, Debug)]
pub struct PlaybackStatus {
    pub available: bool,
    pub reason: String,
    pub buffer_ms: u32,
}
struct Input {
    remote: pb::AudioState,
    revision: u64,
    packets: VecDeque<(Instant, Arc<pb::AudioFrame>)>,
    local_ready: bool,
    local_error: String,
    local_notice: String,
    buffer_ms: u32,
}
#[derive(Clone)]
pub struct PlaybackSender(Arc<Mutex<Input>>);
impl PlaybackSender {
    pub fn set_state(&self, state: pb::AudioState) {
        let mut input = self.0.lock().unwrap();
        if input.remote != state {
            input.remote = state;
            input.revision = input.revision.wrapping_add(1);
            input.packets.clear();
            input.buffer_ms = 0;
        }
    }
    /// Network tasks must never wait for the audio device or a decoder.
    pub fn send(&self, packet: pb::AudioFrame) {
        if !vnt_proto::audio::valid_frame(&packet) {
            return;
        }
        let mut input = self.0.lock().unwrap();
        if !input.remote.available || input.remote.generation != packet.generation {
            return;
        }
        if input.packets.len() >= MAX_BUFFER_FRAMES {
            input.packets.pop_front();
        }
        input.packets.push_back((Instant::now(), Arc::new(packet)));
    }
    pub fn status(&self) -> PlaybackStatus {
        let input = self.0.lock().unwrap();
        PlaybackStatus {
            available: input.remote.available && input.local_ready,
            reason: if !input.remote.available {
                input.remote.reason.clone()
            } else if !input.local_error.is_empty() {
                input.local_error.clone()
            } else if !input.local_ready {
                "正在初始化本机音频设备".into()
            } else {
                input.local_notice.clone()
            },
            buffer_ms: input.buffer_ms,
        }
    }
}
pub struct Playback {
    sender: PlaybackSender,
    _worker: Worker,
}
impl Playback {
    pub fn start() -> Result<Self, String> {
        Self::start_on_device(None)
    }
    pub fn start_on_device(device_id: Option<String>) -> Result<Self, String> {
        let input = Arc::new(Mutex::new(Input {
            remote: pb::AudioState {
                reason: "正在等待被控端音频设备".into(),
                ..Default::default()
            },
            revision: 0,
            packets: VecDeque::new(),
            local_ready: false,
            local_error: String::new(),
            local_notice: String::new(),
            buffer_ms: 0,
        }));
        let sender = PlaybackSender(input.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("audio-playback".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    if !input.lock().unwrap().remote.available {
                        std::thread::park_timeout(Duration::from_millis(20));
                        continue;
                    }
                    if let Err(error) = render_run(&input, &thread_stop, device_id.as_deref()) {
                        {
                            let mut input = input.lock().unwrap();
                            let reason: String = format!("本机播放不可用：{error}")
                                .chars()
                                .take(250)
                                .collect();
                            if input.local_error != reason {
                                log::warn!("native audio playback unavailable: {error}");
                            }
                            input.local_ready = false;
                            input.local_error = reason;
                            input.packets.clear();
                            input.buffer_ms = 0;
                        }
                        std::thread::park_timeout(Duration::from_secs(1));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            sender,
            _worker: Worker {
                stop,
                thread: Some(thread),
            },
        })
    }
    pub fn sender(&self) -> PlaybackSender {
        self.sender.clone()
    }
}
fn render_run(
    input: &Arc<Mutex<Input>>,
    stop: &AtomicBool,
    device_id: Option<&str>,
) -> Result<(), String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let mut render = crate::win::Render::open_on_device(device_id)?;
        let mut player = Player::new(input.clone(), render.rate())?;
        {
            let mut input = input.lock().unwrap();
            input.local_ready = true;
            input.local_error.clear();
            input.local_notice = if render.is_fallback() {
                "所选播放设备不可用，已改用系统默认设备".into()
            } else {
                String::new()
            };
        }
        while !stop.load(Ordering::Acquire) {
            if !input.lock().unwrap().remote.available {
                break;
            }
            render.write(&mut player)?;
        }
        input.lock().unwrap().local_ready = false;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (input, stop, device_id);
        Err("native playback requires Windows".into())
    }
}

pub(crate) struct Player {
    input: Arc<Mutex<Input>>,
    revision: u64,
    jitter: JitterBuffer,
    resampler: Resampler,
    rate: u32,
}

fn clock_ratio(queued: usize, target: usize) -> f64 {
    (1.0 + (queued as f64 - target as f64) * 0.001).clamp(0.995, 1.005)
}
impl Player {
    fn new(input: Arc<Mutex<Input>>, rate: u32) -> Result<Self, String> {
        Ok(Self {
            input,
            revision: u64::MAX,
            jitter: JitterBuffer::new()?,
            resampler: Resampler::new(48000, rate),
            rate,
        })
    }
    pub(crate) fn read(&mut self, count: usize) -> Result<Vec<[f32; 2]>, String> {
        let now = Instant::now();
        let (revision, generation, packets) = {
            let mut input = self.input.lock().unwrap();
            (
                input.revision,
                if input.remote.available {
                    input.remote.generation
                } else {
                    0
                },
                std::mem::take(&mut input.packets),
            )
        };
        // Device/codec reconfiguration happens outside the network inbox lock.
        if self.revision != revision {
            self.revision = revision;
            self.jitter.reset(generation)?;
            self.resampler = Resampler::new(48000, self.rate);
        }
        for (at, packet) in packets {
            self.jitter.push(packet, at);
        }
        // Occupancy feedback tracks clock skew without adding a growing queue.
        // Smooth, bounded +/-0.5% correction is inaudible at normal clock drift.
        self.resampler.set_ratio(clock_ratio(
            self.jitter.queued_frames(),
            self.jitter.target_frames(),
        ));
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            if let Some(sample) = self.resampler.pop() {
                out.push(sample);
            } else {
                self.resampler.push(&self.jitter.pop(now)?);
            }
        }
        self.input.lock().unwrap().buffer_ms = (self.jitter.queued_frames() * 10) as u32;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_playback_joins_its_worker_even_while_waiting_for_a_device() {
        let playback = Playback::start().unwrap();
        let sender = playback.sender();
        let started = Instant::now();
        drop(playback);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            Arc::strong_count(&sender.0),
            1,
            "worker retained its audio inbox"
        );
    }

    #[test]
    fn three_minutes_of_clock_skew_converges_without_buffer_growth_or_underrun() {
        for drift in [-0.001, 0.001] {
            let mut queued = 1440.0f64;
            let mut source = 0.0;
            let mut recent_ratio = 0.0;
            for tick in 0..180_000 {
                source += 48.0 * (1.0 + drift);
                if source >= 480.0 {
                    source -= 480.0;
                    queued += 480.0;
                }
                let ratio = clock_ratio((queued / 480.0) as usize, 2);
                queued -= 48.0 * ratio;
                assert!(
                    (0.0..2880.0).contains(&queued),
                    "clock drift escaped the short buffer: {queued}"
                );
                if tick >= 160_000 {
                    recent_ratio += ratio;
                }
            }
            assert!((recent_ratio / 20_000.0 - (1.0 + drift)).abs() < 0.0002);
        }
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    #[ignore = "requires actual Windows default audio devices; renders silence only"]
    fn default_device_capture_and_silent_render_probe() {
        let mut capture = crate::win::Capture::open().expect("WASAPI loopback device");
        let mut render = crate::win::Render::open_on_device(None).expect("WASAPI render device");
        let input = Arc::new(Mutex::new(Input {
            remote: pb::AudioState {
                generation: 1,
                available: true,
                reason: String::new(),
            },
            revision: 0,
            packets: VecDeque::new(),
            local_ready: true,
            local_error: String::new(),
            local_notice: String::new(),
            buffer_ms: 0,
        }));
        let mut player = Player::new(input, render.rate()).unwrap();
        for _ in 0..5 {
            capture.read().expect("loopback read");
            render.write(&mut player).expect("silent render");
        }
    }
    #[test]
    fn queued_input_is_bounded_and_device_rebuild_discards_old_generation() {
        let sender = PlaybackSender(Arc::new(Mutex::new(Input {
            remote: Default::default(),
            revision: 0,
            packets: VecDeque::new(),
            local_ready: true,
            local_error: String::new(),
            local_notice: String::new(),
            buffer_ms: 0,
        })));
        sender.set_state(pb::AudioState {
            generation: 1,
            available: true,
            reason: String::new(),
        });
        let mut encoder = crate::opus::Encoder::new().unwrap();
        for i in 0..100 {
            sender.send(pb::AudioFrame {
                generation: 1,
                sequence: i,
                timestamp_samples: i as u64 * 480,
                data: encoder.encode(&[0.1; 960]).unwrap(),
            });
        }
        assert_eq!(sender.0.lock().unwrap().packets.len(), 6);
        let mut player = Player::new(sender.0.clone(), 44100).unwrap();
        assert_eq!(player.read(441).unwrap().len(), 441);
        sender.set_state(pb::AudioState {
            generation: 2,
            available: false,
            reason: "设备已移除".into(),
        });
        assert!(!sender.status().available);
        assert!(sender.0.lock().unwrap().packets.is_empty());
        player.read(441).unwrap();
        assert_eq!(player.jitter.queued_frames(), 0);
    }
}
