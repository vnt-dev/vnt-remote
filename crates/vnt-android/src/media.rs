//! Encoded frames remain in native memory. Only decoded GPU surfaces cross into Android UI.
use crate::queue::{Frame, PtsClock, VideoQueue};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use vnt_client::bridge::{BridgeMsg, MediaSink};
use vnt_proto::EventSink;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecoderChoice {
    pub codec: String,
    pub name: String,
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub codec: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub csd: Vec<u8>,
    pub generation: u32,
}
#[cfg(target_os = "android")]
pub struct Window(pub *mut ndk_sys::ANativeWindow);
#[cfg(not(target_os = "android"))]
pub struct Window(pub usize);
// ANativeWindow is reference counted and supports access from the decoder thread.
unsafe impl Send for Window {}
unsafe impl Sync for Window {}
impl Clone for Window {
    fn clone(&self) -> Self {
        #[cfg(target_os = "android")]
        unsafe {
            ndk_sys::ANativeWindow_acquire(self.0);
        }
        Self(self.0)
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        #[cfg(target_os = "android")]
        unsafe {
            ndk_sys::ANativeWindow_release(self.0);
        }
    }
}
struct State {
    epoch: u64,
    active: u32,
    configs: HashMap<u32, Config>,
    choices: Vec<DecoderChoice>,
    queue: VideoQueue,
    window: Option<Window>,
    visible: bool,
    revision: u64,
    stop: bool,
    event: Option<Arc<dyn EventSink>>,
    rendered: u32,
    frames_total: u64,
    decode_us: u64,
    queue_us: u64,
    failed: bool,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    keyframe: AtomicBool,
    sdk: u32,
}
pub struct Renderer {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
pub struct Sink {
    pub renderer: Arc<Renderer>,
    pub epoch: u64,
}
impl Renderer {
    pub fn new(sdk: u32) -> Arc<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                epoch: 0,
                active: 0,
                configs: HashMap::new(),
                choices: Vec::new(),
                queue: VideoQueue::default(),
                window: None,
                visible: true,
                revision: 0,
                stop: false,
                event: None,
                rendered: 0,
                frames_total: 0,
                decode_us: 0,
                queue_us: 0,
                failed: false,
            }),
            wake: Condvar::new(),
            keyframe: AtomicBool::new(false),
            sdk,
        });
        let owned = shared.clone();
        let worker = std::thread::Builder::new()
            .name("vnt-video".into())
            .spawn(move || run(owned))
            .expect("decoder thread");
        Arc::new(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub fn begin(&self, epoch: u64, choices: Vec<DecoderChoice>, event: Arc<dyn EventSink>) {
        let mut s = self.shared.state.lock().unwrap();
        s.epoch = epoch;
        s.rendered = 0;
        s.frames_total = 0;
        s.queue_us = 0;
        s.decode_us = 0;
        s.choices = choices;
        s.event = Some(event);
        s.configs.clear();
        s.active = 0;
        s.queue.reset();
        s.failed = false;
        s.revision += 1;
        self.shared.wake.notify_all();
    }
    #[cfg(any(target_os = "android", test))]
    pub fn surface(&self, window: Option<Window>) {
        let mut s = self.shared.state.lock().unwrap();
        // A SurfaceView layout/zoom callback can refer to the same native window.
        // Drop the newly acquired duplicate reference without resetting the decoder.
        if s.window.as_ref().map(|w| w.0) == window.as_ref().map(|w| w.0) {
            return;
        }
        s.window = window;
        s.queue.reset();
        s.failed = false;
        s.revision += 1;
        self.shared.keyframe.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
    pub fn foreground(&self, visible: bool) {
        let mut s = self.shared.state.lock().unwrap();
        s.visible = visible;
        s.queue.reset();
        s.revision += 1;
        self.shared.keyframe.store(visible, Ordering::Release);
        self.shared.wake.notify_all();
    }
    pub fn reset(&self) {
        let mut s = self.shared.state.lock().unwrap();
        s.queue.reset();
        s.revision += 1;
        self.shared.keyframe.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
    pub fn keyframe_needed(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::AcqRel)
    }
    pub fn stats(&self) -> serde_json::Value {
        let mut s = self.shared.state.lock().unwrap();
        let count = s.rendered.max(1) as u64;
        let v = serde_json::json!({"renderFps":s.rendered,"framesTotal":s.frames_total,"decodeMs":s.decode_us as f64/count as f64/1000.0,"queueMs":s.queue_us as f64/count as f64/1000.0,"queueLength":s.queue.len(),"queueResets":s.queue.resets});
        s.rendered = 0;
        s.decode_us = 0;
        s.queue_us = 0;
        v
    }
    pub fn end(&self) {
        let mut s = self.shared.state.lock().unwrap();
        s.epoch = 0;
        s.active = 0;
        s.queue.reset();
        s.configs.clear();
        s.event = None;
        s.revision += 1;
        self.shared.wake.notify_all();
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().stop = true;
        self.shared.wake.notify_all();
        if let Some(w) = self.worker.get_mut().unwrap().take() {
            let _ = w.join();
        }
    }
}
impl MediaSink for Sink {
    fn send(&self, message: BridgeMsg) {
        let mut s = self.renderer.shared.state.lock().unwrap();
        if s.epoch != self.epoch {
            return;
        }
        match message {
            BridgeMsg::Video {
                stream_id,
                generation,
                keyframe,
                pts_ms,
                data,
            } => {
                if !s.visible
                    || s.window.is_none()
                    || s.active != stream_id
                    || s.failed
                    || !s
                        .configs
                        .get(&stream_id)
                        .is_some_and(|c| c.generation == generation)
                {
                    return;
                }
                if s.queue.push(Frame {
                    keyframe,
                    pts_ms,
                    data,
                    received: Instant::now(),
                }) {
                    s.revision += 1;
                    self.renderer.shared.keyframe.store(true, Ordering::Release);
                }
            }
            BridgeMsg::Text(text) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    match v["type"].as_str().unwrap_or("") {
                        "screensState" => {
                            let active = v["activeStreamId"].as_u64().unwrap_or(0) as u32;
                            if s.active != active {
                                s.active = active;
                                s.queue.reset();
                                s.failed = false;
                                s.revision += 1;
                                self.renderer.shared.keyframe.store(true, Ordering::Release);
                            }
                            if let Some(screens) = v["screens"].as_array() {
                                for screen in screens {
                                    update_config(&mut s, screen);
                                }
                            }
                        }
                        "streamInfo" => {
                            update_config(&mut s, &v);
                        }
                        "transportState" => {
                            if v["kind"] == 2 || v["kind"] == 0 {
                                s.queue.reset();
                                s.revision += 1;
                                self.renderer.shared.keyframe.store(true, Ordering::Release);
                            }
                        }
                        "desktopState" => {
                            s.queue.reset();
                            s.revision += 1;
                            self.renderer.shared.keyframe.store(true, Ordering::Release);
                        }
                        _ => {}
                    }
                }
                let event = s.event.clone();
                drop(s);
                if let Some(event) = event {
                    event.emit_json("client://media", text);
                }
                self.renderer.shared.wake.notify_all();
                return;
            }
        }
        self.renderer.shared.wake.notify_all();
    }
}
fn update_config(s: &mut State, v: &serde_json::Value) {
    let id = v["streamId"].as_u64().unwrap_or(0) as u32;
    let width = v["width"].as_u64().unwrap_or(0) as u32;
    let height = v["height"].as_u64().unwrap_or(0) as u32;
    if id == 0 || !vnt_proto::validation::valid_video_size(width, height) {
        return;
    }
    let codec = v["codec"].as_str().unwrap_or("h264");
    let fps = v["fps"].as_u64().unwrap_or(60) as u32;
    let choice = s
        .choices
        .iter()
        .filter(|d| {
            d.codec == codec && d.max_width >= width && d.max_height >= height && d.max_fps >= fps
        })
        .max_by_key(|d| d.hardware);
    let name = choice.map(|d| d.name.clone()).unwrap_or_default();
    let generation = v["generation"].as_u64().unwrap_or(1) as u32;
    let csd = v["decoderConfig"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_u64().filter(|n| *n < 256).map(|n| n as u8))
                .collect()
        })
        .unwrap_or_else(|| {
            s.configs
                .get(&id)
                .filter(|c| c.generation == generation && c.codec == codec)
                .map(|c| c.csd.clone())
                .unwrap_or_default()
        });
    let config = Config {
        codec: codec.into(),
        name,
        width,
        height,
        fps,
        csd,
        generation,
    };
    if s.configs.get(&id) != Some(&config) {
        s.configs.insert(id, config);
        if s.active == id {
            s.queue.reset();
            s.failed = false;
            s.revision += 1;
        }
    }
}
fn run(shared: Arc<Shared>) {
    let mut decoder: Option<crate::decoder::Decoder> = None;
    let mut revision = u64::MAX;
    let mut clock = PtsClock::default();
    let mut pending: Option<Frame> = None;
    let mut submitted = HashMap::<i64, (Instant, Instant)>::new();
    let mut stalls = 0u32;
    loop {
        let mut s = shared.state.lock().unwrap();
        if s.stop {
            break;
        }
        if s.revision != revision {
            revision = s.revision;
            pending = None;
            submitted.clear();
            clock = PtsClock::default();
            let config = s.configs.get(&s.active).cloned();
            let window = if s.visible && !s.failed {
                s.window.clone()
            } else {
                None
            };
            let event = s.event.clone();
            drop(s);
            decoder = None;
            if let (Some(config), Some(window)) = (config, window) {
                match crate::decoder::Decoder::new(&config, window, shared.sdk) {
                    Ok(d) => {
                        decoder = Some(d);
                        if let Some(e) = event {
                            e.emit_json("client://decoder",serde_json::json!({"name":config.name,"codec":config.codec,"width":config.width,"height":config.height}).to_string());
                        }
                    }
                    Err(reason) => {
                        let mut s = shared.state.lock().unwrap();
                        if s.revision == revision {
                            s.failed = true;
                            s.queue.reset();
                            drop(s);
                            if let Some(e) = event {
                                e.emit_json("client://decoderFailed",serde_json::json!({"name":config.name,"codec":config.codec,"reason":reason}).to_string());
                            }
                        }
                    }
                }
            }
            continue;
        }
        if decoder.is_none() {
            drop(shared.wake.wait(s).unwrap());
            continue;
        }
        if pending.is_none() {
            pending = s.queue.pop();
        }
        drop(s);
        let d = decoder.as_mut().unwrap();
        let mut failure = None;
        let mut fatal = false;
        if let Some(frame) = pending.as_ref() {
            if frame.received.elapsed() > Duration::from_millis(100) {
                failure = Some("解码输入积压".to_string());
            } else {
                match d.input(
                    &frame.data,
                    frame.keyframe,
                    clock_value(&clock, frame.pts_ms),
                ) {
                    Ok(true) => {
                        let frame = pending.take().unwrap();
                        let pts = clock.micros(frame.pts_ms);
                        submitted.insert(pts, (frame.received, Instant::now()));
                    }
                    Ok(false) => {}
                    Err(e) => {
                        failure = Some(e);
                        fatal = true;
                    }
                }
            }
        }
        match d.drain() {
            Ok(timestamps) => {
                let mut s = shared.state.lock().unwrap();
                if s.revision != revision {
                    continue;
                }
                if !timestamps.is_empty() {
                    stalls = 0;
                }
                for pts in timestamps {
                    if let Some((received, input)) = submitted.remove(&pts) {
                        s.queue_us += input.saturating_duration_since(received).as_micros() as u64;
                        s.decode_us += input.elapsed().as_micros() as u64;
                    }
                    s.rendered += 1;
                    s.frames_total += 1;
                }
            }
            Err(e) => {
                failure = Some(e);
                fatal = true;
            }
        }
        if submitted.len() > 120 {
            failure = Some("解码输出停滞".into());
        }
        if let Some(reason) = failure {
            stalls += 1;
            fatal |= stalls >= 3;
            let mut s = shared.state.lock().unwrap();
            s.queue.reset();
            s.revision += 1;
            let event = s.event.clone();
            let config = s.configs.get(&s.active).cloned();
            if fatal {
                s.failed = true;
            } else {
                shared.keyframe.store(true, Ordering::Release);
            }
            drop(s);
            log::warn!("Native decoder recovery: {reason}; replace={fatal}");
            if let Some(event) = event {
                if fatal {
                    if let Some(c) = config {
                        event.emit_json(
                            "client://decoderFailed",
                            serde_json::json!({"name":c.name,"codec":c.codec,"reason":reason})
                                .to_string(),
                        );
                    }
                } else {
                    event.emit_json(
                        "client://videoReset",
                        serde_json::json!({"reason":reason}).to_string(),
                    );
                }
            }
        }
        let s = shared.state.lock().unwrap();
        if s.queue.len() == 0 && pending.is_none() {
            let _ = shared
                .wake
                .wait_timeout(s, Duration::from_millis(2))
                .unwrap();
        }
    }
}
// Peek without advancing the clock when the codec has not accepted the input.
fn clock_value(clock: &PtsClock, pts: u32) -> i64 {
    let mut c = clock.clone();
    c.micros(pts)
}

#[cfg(all(test, not(target_os = "android")))]
mod surface_tests {
    use super::*;
    #[test]
    fn resizing_the_same_window_does_not_reset_the_decoder_revision() {
        let renderer = Renderer::new(30);
        renderer.surface(Some(Window(1)));
        let revision = renderer.shared.state.lock().unwrap().revision;
        renderer.surface(Some(Window(1)));
        assert_eq!(renderer.shared.state.lock().unwrap().revision, revision);
        renderer.surface(Some(Window(2)));
        assert_eq!(renderer.shared.state.lock().unwrap().revision, revision + 1);
        renderer.surface(None);
        assert_eq!(renderer.shared.state.lock().unwrap().revision, revision + 2);
    }
}
