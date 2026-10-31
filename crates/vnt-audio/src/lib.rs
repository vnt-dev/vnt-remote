//! Low latency system audio. Native COM objects and codec states stay on their owner threads.
mod format;
mod jitter;
mod opus;
mod playback;
#[cfg(windows)]
mod win;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod win;
pub use playback::{Playback, PlaybackSender, PlaybackStatus};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Only active render endpoints are selectable; never enumerates microphones.
pub fn list_playback_devices() -> Result<Vec<PlaybackDevice>, String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        win::list_playback_devices()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};
use vnt_proto::pb;

#[derive(Debug)]
pub struct EncodedFrame {
    pub packet: Arc<pb::AudioFrame>,
    pub captured_at: Instant,
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
struct Source {
    _worker: Worker,
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    state: watch::Receiver<pb::AudioState>,
    subscribers: usize,
}
struct Manager {
    source: Option<Source>,
    stopped: bool,
    synthetic: bool,
}

#[derive(Clone)]
pub struct SharedAudio(Arc<Mutex<Manager>>);
pub struct Subscription {
    pub frames: broadcast::Receiver<Arc<EncodedFrame>>,
    pub state: watch::Receiver<pb::AudioState>,
    owner: SharedAudio,
}
impl SharedAudio {
    pub fn new(synthetic: bool) -> Self {
        Self(Arc::new(Mutex::new(Manager {
            source: None,
            stopped: false,
            synthetic,
        })))
    }
    pub fn subscribe(&self) -> Result<Subscription, String> {
        let mut manager = self.0.lock().unwrap();
        if manager.stopped {
            return Err("audio source stopped".into());
        }
        if manager.source.is_none() {
            let (frames, _) = broadcast::channel(6);
            let (state_tx, state) = watch::channel(pb::AudioState {
                reason: "正在初始化系统音频".into(),
                ..Default::default()
            });
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread_frames = frames.clone();
            let synthetic = manager.synthetic;
            let thread = std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || {
                    capture_worker(synthetic, &thread_stop, &thread_frames, &state_tx);
                })
                .map_err(|e| e.to_string())?;
            manager.source = Some(Source {
                _worker: Worker {
                    stop,
                    thread: Some(thread),
                },
                frames,
                state,
                subscribers: 0,
            });
        }
        let source = manager.source.as_mut().unwrap();
        source.subscribers += 1;
        Ok(Subscription {
            frames: source.frames.subscribe(),
            state: source.state.clone(),
            owner: self.clone(),
        })
    }
    pub fn stop(&self) {
        let mut manager = self.0.lock().unwrap();
        manager.stopped = true;
        manager.source.take();
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        let mut manager = self.owner.0.lock().unwrap();
        if let Some(source) = manager.source.as_mut() {
            source.subscribers -= 1;
            if source.subscribers == 0 {
                manager.source.take();
            }
        }
    }
}

fn capture_worker(
    synthetic: bool,
    stop: &AtomicBool,
    frames: &broadcast::Sender<Arc<EncodedFrame>>,
    state: &watch::Sender<pb::AudioState>,
) {
    let mut generation = 0u32;
    while !stop.load(Ordering::Acquire) {
        generation = generation.wrapping_add(1).max(1);
        let result = capture_run(synthetic, stop, frames, state, generation);
        if let Err(error) = result {
            let reason: String = format!("系统声音不可用：{error}")
                .chars()
                .take(250)
                .collect();
            if state.borrow().reason != reason {
                log::warn!("system audio capture unavailable: {error}");
            }
            state.send_replace(pb::AudioState {
                generation,
                available: false,
                reason,
            });
            std::thread::park_timeout(Duration::from_secs(1));
        }
    }
}
fn capture_run(
    synthetic: bool,
    stop: &AtomicBool,
    frames: &broadcast::Sender<Arc<EncodedFrame>>,
    state: &watch::Sender<pb::AudioState>,
    generation: u32,
) -> Result<(), String> {
    let mut encoder = opus::Encoder::new()?;
    let mut timestamp = 0u64;
    if synthetic {
        state.send_replace(pb::AudioState {
            generation,
            available: true,
            reason: String::new(),
        });
        let mut due = Instant::now();
        while !stop.load(Ordering::Acquire) {
            let pcm: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let x = ((timestamp + i) as f32 * 440.0 * std::f32::consts::TAU / 48000.0)
                        .sin()
                        * 0.1;
                    [x, x]
                })
                .collect();
            publish(&mut encoder, &pcm, frames, generation, &mut timestamp)?;
            due += Duration::from_millis(10);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::park_timeout(wait);
            } else {
                due = Instant::now();
            }
        }
        return Ok(());
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let mut capture = win::Capture::open()?;
        let mut resampler = format::Resampler::new(capture.rate(), 48000);
        let mut pcm = Vec::with_capacity(960);
        state.send_replace(pb::AudioState {
            generation,
            available: true,
            reason: String::new(),
        });
        while !stop.load(Ordering::Acquire) {
            let samples = capture.read()?;
            resampler.push(&samples);
            while let Some(sample) = resampler.pop() {
                pcm.extend_from_slice(&sample);
                if pcm.len() == 960 {
                    publish(&mut encoder, &pcm, frames, generation, &mut timestamp)?;
                    pcm.clear();
                }
            }
        }
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err("system audio requires Windows".into())
    }
}
fn publish(
    encoder: &mut opus::Encoder,
    pcm: &[f32],
    frames: &broadcast::Sender<Arc<EncodedFrame>>,
    generation: u32,
    timestamp: &mut u64,
) -> Result<(), String> {
    let data = encoder.encode(pcm)?;
    let packet = pb::AudioFrame {
        generation,
        sequence: (*timestamp / 480) as u32,
        timestamp_samples: *timestamp,
        data,
    };
    *timestamp = timestamp
        .checked_add(480)
        .ok_or("audio sample clock overflow")?;
    let _ = frames.send(Arc::new(EncodedFrame {
        packet: Arc::new(packet),
        captured_at: Instant::now(),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn source_is_lazy_shared_and_stops_after_last_subscription() {
        let shared = SharedAudio::new(true);
        assert!(shared.0.lock().unwrap().source.is_none());
        let mut a = shared.subscribe().unwrap();
        let mut b = shared.subscribe().unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), a.frames.recv())
            .await
            .unwrap()
            .unwrap();
        let other = b.frames.recv().await.unwrap();
        let mut first = first;
        while first.packet.timestamp_samples < other.packet.timestamp_samples {
            first = a.frames.recv().await.unwrap();
        }
        assert!(Arc::ptr_eq(&first, &other));
        // A slow subscriber drops its own backlog without blocking the source.
        for _ in 0..20 {
            a.frames.recv().await.unwrap();
        }
        assert!(matches!(
            b.frames.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        drop(a);
        assert_eq!(
            shared
                .0
                .lock()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .subscribers,
            1
        );
        drop(b);
        assert!(shared.0.lock().unwrap().source.is_none());
        shared.stop();
        assert!(shared.subscribe().is_err());
    }
}
