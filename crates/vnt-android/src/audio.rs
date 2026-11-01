//! Opus and jitter state stay on one Rust worker. JVM pulls reusable 10ms PCM blocks.
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use vnt_client::platform::*;
use vnt_proto::pb;
struct State {
    remote: pb::AudioState,
    revision: u64,
    packets: VecDeque<Arc<pb::AudioFrame>>,
    pcm: VecDeque<[f32; 2]>,
    error: String,
}
pub struct Audio {
    state: Mutex<State>,
    wake: Condvar,
    visible: AtomicBool,
    serial: AtomicU64,
}
impl Audio {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                remote: Default::default(),
                revision: 0,
                packets: VecDeque::new(),
                pcm: VecDeque::new(),
                error: String::new(),
            }),
            wake: Condvar::new(),
            visible: AtomicBool::new(true),
            serial: AtomicU64::new(0),
        })
    }
    pub fn foreground(&self, visible: bool) {
        self.visible.store(visible, Ordering::Release);
        let mut s = self.state.lock().unwrap();
        s.pcm.clear();
        s.packets.clear();
        s.revision += 1;
        self.wake.notify_all();
    }
    pub fn read(&self, out: &mut [u8]) -> usize {
        let mut s = self.state.lock().unwrap();
        if !self.visible.load(Ordering::Acquire) {
            return 0;
        }
        let n = s.pcm.len().min(out.len() / 8).min(480);
        for target in out[..n * 8].as_chunks_mut::<8>().0 {
            let f = s.pcm.pop_front().unwrap();
            target[..4].copy_from_slice(&f[0].to_ne_bytes());
            target[4..].copy_from_slice(&f[1].to_ne_bytes());
        }
        n * 8
    }
}
struct Sender(Arc<Audio>, u64);
impl AudioSender for Sender {
    fn set_state(&self, state: pb::AudioState) {
        if self.0.serial.load(Ordering::Acquire) != self.1 {
            return;
        }
        let mut s = self.0.state.lock().unwrap();
        if s.remote != state {
            s.remote = state;
            s.revision += 1;
            s.packets.clear();
            s.pcm.clear();
        }
        self.0.wake.notify_all();
    }
    fn send(&self, frame: pb::AudioFrame) {
        if self.0.serial.load(Ordering::Acquire) != self.1 {
            return;
        }
        if !vnt_proto::audio::valid_frame(&frame) || !self.0.visible.load(Ordering::Acquire) {
            return;
        }
        let mut s = self.0.state.lock().unwrap();
        if s.remote.available && s.remote.generation == frame.generation {
            if s.packets.len() >= 6 {
                s.packets.pop_front();
            }
            s.packets.push_back(Arc::new(frame));
        }
        self.0.wake.notify_all();
    }
    fn status(&self) -> PlaybackStatus {
        let s = self.0.state.lock().unwrap();
        PlaybackStatus {
            available: s.remote.available && s.error.is_empty(),
            reason: if !s.error.is_empty() {
                s.error.clone()
            } else {
                s.remote.reason.clone()
            },
            buffer_ms: (s.pcm.len() / 48) as u32,
        }
    }
}
struct Playback {
    audio: Arc<Audio>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    serial: u64,
}
impl AudioPlayback for Playback {
    fn sender(&self) -> Arc<dyn AudioSender> {
        Arc::new(Sender(self.audio.clone(), self.serial))
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.audio.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if self.audio.serial.load(Ordering::Acquire) == self.serial {
            let mut s = self.audio.state.lock().unwrap();
            s.pcm.clear();
            s.packets.clear();
        }
    }
}
pub struct Factory(pub Arc<Audio>);
impl AudioFactory for Factory {
    fn start(&self, _: Option<String>) -> Result<Box<dyn AudioPlayback>, String> {
        let serial = self.0.serial.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut state = self.0.state.lock().unwrap();
            state.error.clear();
            state.pcm.clear();
            state.packets.clear();
            state.revision += 1;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let audio = self.0.clone();
        let worker = std::thread::Builder::new()
            .name("vnt-opus".into())
            .spawn(move || {
                let mut jitter = match vnt_audio::jitter::JitterBuffer::new() {
                    Ok(j) => j,
                    Err(e) => {
                        audio.state.lock().unwrap().error = e;
                        return;
                    }
                };
                let mut revision = u64::MAX;
                let mut next = Instant::now();
                while !stopped.load(Ordering::Acquire)
                    && audio.serial.load(Ordering::Acquire) == serial
                {
                    let mut s = audio.state.lock().unwrap();
                    if s.revision != revision {
                        revision = s.revision;
                        let generation = s.remote.generation;
                        drop(s);
                        if let Err(e) = jitter.reset(generation) {
                            audio.state.lock().unwrap().error = e;
                            return;
                        }
                        next = Instant::now();
                        s = audio.state.lock().unwrap();
                    }
                    if !s.remote.available || !audio.visible.load(Ordering::Acquire) {
                        let _ = audio
                            .wake
                            .wait_timeout(s, Duration::from_millis(20))
                            .unwrap();
                        continue;
                    }
                    let now = Instant::now();
                    for p in s.packets.drain(..) {
                        jitter.push(p, now);
                    }
                    drop(s);
                    if now < next {
                        let s = audio.state.lock().unwrap();
                        let _ = audio.wake.wait_timeout(s, next - now).unwrap();
                        continue;
                    }
                    next = now + Duration::from_millis(10);
                    match jitter.pop(now) {
                        Ok(pcm) => {
                            let mut s = audio.state.lock().unwrap();
                            if s.pcm.len() + pcm.len() > 2880 {
                                s.pcm.clear();
                            }
                            s.pcm.extend(pcm);
                        }
                        Err(e) => {
                            audio.state.lock().unwrap().error = e;
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Box::new(Playback {
            audio: self.0.clone(),
            stop,
            worker: Some(worker),
            serial,
        }))
    }
}
