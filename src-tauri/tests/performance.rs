//! Explicit real-desktop acceptance runs; excluded from unattended unit tests.
//! cargo test -p vnt-remote --release --test performance -- --ignored --nocapture --test-threads=1
//!
//! `throughput_matrix` is the capture → encode → transport throughput benchmark
//! used for memory/throughput comparisons. It runs the full loopback pipeline
//! (shared capture source → encoder → UDP/TCP → client reassembly → WebSocket
//! bridge) at several bitrates and samples process private memory during the
//! run. See docs/performance.md for the measurement procedure.
use futures_util::StreamExt;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::Message;
use vnt_proto::{control::ControlMessage, Transport};
use vnt_remote_lib::{client, server, EventSink, NoopEventSink};

#[cfg(windows)]
#[path = "../../crates/vnt-capture/tests/support/motion.rs"]
mod motion;

#[derive(Default)]
struct Samples(Mutex<Vec<serde_json::Value>>, Mutex<Vec<f64>>);

#[cfg(windows)]
fn private_mib() -> f64 {
    #[repr(C)]
    struct MemoryCounters {
        cb: u32,
        faults: u32,
        sizes: [usize; 9],
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: isize, counters: *mut MemoryCounters, size: u32) -> i32;
    }
    let mut memory = MemoryCounters {
        cb: std::mem::size_of::<MemoryCounters>() as u32,
        faults: 0,
        sizes: [0; 9],
    };
    assert_ne!(
        unsafe { GetProcessMemoryInfo(-1, &mut memory, memory.cb) },
        0
    );
    memory.sizes[8] as f64 / (1024.0 * 1024.0)
}

#[cfg(not(windows))]
fn private_mib() -> f64 {
    0.0
}
impl EventSink for Samples {
    fn emit_json(&self, event: &str, payload: String) {
        if event == "client://traffic" {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) {
                if let Some(rate) = value["downloadBytesPerSecond"].as_f64() {
                    self.1.lock().unwrap().push(rate);
                }
            }
        }
        if event == "client://stats" {
            self.0
                .lock()
                .unwrap()
                .push(serde_json::from_str(&payload).unwrap());
        }
    }
}

async fn measure(
    transport: Transport,
    samples: Arc<Samples>,
    cycles: usize,
    synthetic: bool,
) -> f64 {
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
    let cfg = server::ServerConfig {
        region: None,
        password: "local-performance-test".into(),
        port: 0,
        monitor_index: 0,
        fps: 30,
        bitrate_kbps: 8000,
        hardware_encode: true,
        audio: false,
        view_only: true,
        clipboard: clipboard.clone(),
        synthetic,
    };
    let server = if synthetic {
        server::start_with_capture(
            Arc::new(NoopEventSink),
            cfg,
            vnt_capture::SharedCapture::synthetic(3840, 2160),
        )
        .await
    } else {
        server::start(Arc::new(NoopEventSink), cfg).await
    }
    .unwrap();
    let mut fps = 0.0;
    let mut disconnected_memory = Vec::new();
    for cycle in 0..cycles {
        let (client, info) = client::connect_split(
            samples.clone(),
            client::ConnectParams {
                clipboard_enabled: true,
                server_config_id: None,
                server_node_id: None,
                server_network_code: None,
                hardware_codecs: Vec::new(),
                color_depth: 24,
                show_wallpaper: true,
                drag_full_windows: false,
                fec_enabled: false,
                host: "127.0.0.1".into(),
                port: server.port,
                password: "local-performance-test".into(),
                transport,
                codecs: vec!["h264".into()],
                fps: 30,
                bitrate_kbps: 8000,
                resolution_mode: if cycles > 1 && cycle % 2 == 1 {
                    "scale"
                } else {
                    "none"
                }
                .into(),
                target_width: 1920,
                target_height: 1080,
                audio_device_id: None,
                audio_enabled: false,
            },
            clipboard.clone(),
        )
        .await
        .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/?t={}",
            info.ws_port, info.token
        ))
        .await
        .unwrap();
        client
            .control_tx
            .send(ControlMessage::RequestKeyframe)
            .await
            .unwrap();
        let warmup = if cycles == 1 { 5 } else { 1 };
        let duration = if cycles == 1 { 60 } else { 2 };
        let started = tokio::time::Instant::now();
        let mut frames = 0u64;
        let mut resets = 0u32;
        let mut dimensions = (info.width, info.height);
        samples.0.lock().unwrap().clear();
        while started.elapsed() < Duration::from_secs(warmup + duration) {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(Message::Binary(_))))
                    if started.elapsed() >= Duration::from_secs(warmup) =>
                {
                    frames += 1
                }
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                        if msg["type"] == "streamInfo" || msg["type"] == "videoReset" {
                            if let (Some(width), Some(height)) =
                                (msg["width"].as_u64(), msg["height"].as_u64())
                            {
                                dimensions = (width as u32, height as u32);
                            }
                            resets += 1;
                            client
                                .control_tx
                                .send(ControlMessage::RequestKeyframe)
                                .await
                                .unwrap();
                        }
                    }
                }
                Ok(None) | Ok(Some(Err(_))) => panic!("video bridge disconnected"),
                _ => {}
            }
        }
        fps = frames as f64 / duration as f64;
        let last = samples
            .0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        eprintln!("PERFORMANCE transport={transport:?} synthetic={synthetic} cycle={cycle} resolution={}x{} bridge_fps={fps:.2} resets={resets} private_mib={:.1} last_stats={last}", dimensions.0, dimensions.1, private_mib());
        if synthetic && !(cycles > 1 && cycle % 2 == 1) {
            assert_eq!(dimensions, (3840, 2160));
        }
        if synthetic {
            assert_eq!(
                last["appliedFps"], 30,
                "lossless local video must keep the requested frame rate"
            );
            assert_eq!(
                last["appliedBitrateKbps"], 8000,
                "intentional UDP pacing is not network congestion"
            );
        }
        client.disconnect();
        drop(ws);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !server.active_sessions().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("disconnect must release the session");
        // MF/D3D destruction may finish asynchronously after the session slot
        // is released. Measure the idle process after driver cleanup settles.
        tokio::time::sleep(Duration::from_secs(2)).await;
        disconnected_memory.push(private_mib());
        eprintln!(
            "DISCONNECTED cycle={cycle} private_mib={:.1}",
            private_mib()
        );
    }
    server.stop().await;
    if cycles >= 10 {
        let stable = &disconnected_memory[3..];
        let baseline = stable.iter().copied().fold(f64::INFINITY, f64::min);
        // A one-off driver/allocator peak which is freed in later cycles is
        // not a leak. Require the final three cycles to return to the plateau.
        let final_cycles = &disconnected_memory[cycles - 3..];
        assert!(
            final_cycles.iter().all(|&sample| sample <= baseline + 64.0),
            "private memory did not plateau: {disconnected_memory:?}"
        );
    }
    fps
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an unlocked desktop displaying continuous motion; 130 seconds"]
async fn desktop_tcp_and_hybrid_keep_comparable_throughput() {
    let tcp = measure(Transport::Tcp, Arc::default(), 1, false).await;
    let hybrid = measure(Transport::Hybrid, Arc::default(), 1, false).await;
    assert!(
        tcp >= 10.0,
        "desktop must display continuous motion: TCP {tcp:.2} fps"
    );
    assert!(
        hybrid >= tcp * 0.9,
        "hybrid {hybrid:.2} fps vs TCP {tcp:.2} fps"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real capture/encoder lifecycle soak; requires an unlocked desktop"]
async fn ten_reconnections_release_each_session() {
    measure(Transport::Hybrid, Arc::default(), 10, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "4K hardware encoder throughput benchmark; 130 seconds"]
async fn synthetic_4k_tcp_and_hybrid_keep_comparable_throughput() {
    let tcp = measure(Transport::Tcp, Arc::default(), 1, true).await;
    let hybrid = measure(Transport::Hybrid, Arc::default(), 1, true).await;
    assert!(
        tcp >= 10.0,
        "hardware encoder failed to sustain video: {tcp:.2}"
    );
    assert!(
        hybrid >= tcp * 0.9,
        "hybrid {hybrid:.2} fps vs TCP {tcp:.2} fps"
    );
}

// ---------------------------------------------------------------------------
// Capture → encode → transport throughput matrix
//
// Loopback (self-to-self) benchmark over the real pipeline: shared capture
// source → encoder → UDP/TCP → client reassembly → WebSocket bridge. Each case
// runs at a fixed requested fps/bitrate for a warmup plus a measurement window
// and samples the whole test process (server + client + bridge) private commit
// during the run. Results are appended to the file named by BENCH_OUT (default
// target/throughput-matrix.txt) so before/after builds can be diffed.

/// (private commit MiB, working set MiB) of this process.
#[cfg(windows)]
fn mem_sample() -> (f64, f64) {
    #[repr(C)]
    struct MemoryCounters {
        cb: u32,
        faults: u32,
        sizes: [usize; 9],
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: isize, counters: *mut MemoryCounters, size: u32) -> i32;
    }
    let mut memory = MemoryCounters {
        cb: std::mem::size_of::<MemoryCounters>() as u32,
        faults: 0,
        sizes: [0; 9],
    };
    assert_ne!(
        unsafe { GetProcessMemoryInfo(-1, &mut memory, memory.cb) },
        0
    );
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    (mib(memory.sizes[8]), mib(memory.sizes[1]))
}

#[cfg(not(windows))]
fn mem_sample() -> (f64, f64) {
    (0.0, 0.0)
}

/// Background sampler: one (private, working set) pair every 100 ms.
struct MemSampler {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Vec<(Instant, f64, f64)>>>,
}

impl MemSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                let (private, working) = mem_sample();
                samples.push((Instant::now(), private, working));
                std::thread::sleep(Duration::from_millis(100));
            }
            samples
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) -> Vec<(Instant, f64, f64)> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap()
    }
}

struct BenchOutcome {
    cpu_percent: f64,
    wire_mbps: f64,
    codec: String,
    hardware: bool,
    transport: Transport,
    requested_fps: u32,
    requested_kbps: u32,
    resolution: (u32, u32),
    bridge_fps: f64,
    payload_mbps: f64,
    keyframes: u64,
    stalls: u64,
    reconnects: u32,
    applied_fps: Option<u64>,
    applied_kbps: Option<u64>,
    loss_permille: Option<u64>,
    steady_private_mib: f64,
    steady_working_mib: f64,
    peak_private_mib: f64,
    idle_private_mib: f64,
    gap_p95_ms: f64,
    gap_max_ms: f64,
    jitter_p95_ms: f64,
    minimum_fps: u32,
    fps_stddev: f64,
}

#[cfg(windows)]
fn cpu_seconds() -> f64 {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetProcessTimes(
            handle: isize,
            created: *mut FileTime,
            exited: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let (mut c, mut e, mut k, mut u) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    assert_ne!(
        unsafe { GetProcessTimes(-1, &mut c, &mut e, &mut k, &mut u) },
        0
    );
    (((k.high as u64) << 32 | k.low as u64) + ((u.high as u64) << 32 | u.low as u64)) as f64 / 1e7
}
#[cfg(not(windows))]
fn cpu_seconds() -> f64 {
    0.0
}

#[allow(clippy::too_many_arguments)]
async fn bench_config(
    codecs: Vec<String>,
    hardware_encode: bool,
    transport: Transport,
    fps: u32,
    bitrate_kbps: u32,
    synthetic: Option<(u32, u32)>,
    warmup_secs: u64,
    duration_secs: u64,
) -> BenchOutcome {
    bench_config_with_fec(
        codecs,
        hardware_encode,
        transport,
        fps,
        bitrate_kbps,
        synthetic,
        warmup_secs,
        duration_secs,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn bench_config_with_fec(
    codecs: Vec<String>,
    hardware_encode: bool,
    transport: Transport,
    fps: u32,
    bitrate_kbps: u32,
    synthetic: Option<(u32, u32)>,
    warmup_secs: u64,
    duration_secs: u64,
    fec_enabled: bool,
) -> BenchOutcome {
    let clipboard = vnt_remote_lib::clipboard::ClipboardService::with_writer(Arc::new(|_, _| {}));
    let cfg = server::ServerConfig {
        region: None,
        password: "local-performance-test".into(),
        port: 0,
        monitor_index: 0,
        fps,
        bitrate_kbps,
        hardware_encode,
        audio: false,
        view_only: true,
        clipboard: clipboard.clone(),
        synthetic: synthetic.is_some(),
    };
    let native_directory = tempfile::tempdir().unwrap();
    let server = match synthetic {
        Some((width, height)) => {
            server::start_with_capture(
                Arc::new(NoopEventSink),
                cfg,
                vnt_capture::SharedCapture::synthetic(width, height),
            )
            .await
        }
        None => {
            // Exercise the same single-selected-screen route as the desktop app.
            let primary = vnt_capture::list_monitors()
                .unwrap()
                .into_iter()
                .find(|m| m.primary)
                .unwrap();
            let saved = vnt_host::config::AppConfig {
                password: cfg.password.clone(),
                monitor_index: primary.index,
                hardware_encode,
                audio: false,
                clipboard: false,
                allow_control: false,
                selected_targets: vec![vnt_host::config::CaptureTarget::Monitor {
                    name: primary.name,
                }],
                ..Default::default()
            };
            let path = native_directory.path().join("config.json");
            vnt_host::store::save_at(&path, &saved).unwrap();
            server::start_portable(Arc::new(NoopEventSink), cfg, path).await
        }
    }
    .unwrap();

    let samples = Arc::new(Samples::default());
    let sampler = MemSampler::start();
    let (client, info) = client::connect_split(
        samples.clone(),
        client::ConnectParams {
            clipboard_enabled: true,
            server_config_id: None,
            server_node_id: None,
            server_network_code: None,
            hardware_codecs: Vec::new(),
            color_depth: 24,
            show_wallpaper: true,
            drag_full_windows: false,
            fec_enabled,
            host: "127.0.0.1".into(),
            port: server.port,
            password: "local-performance-test".into(),
            transport,
            codecs,
            fps,
            bitrate_kbps,
            resolution_mode: "none".into(),
            target_width: 0,
            target_height: 0,
            audio_device_id: None,
            audio_enabled: false,
        },
        clipboard.clone(),
    )
    .await
    .unwrap();
    let mut ws = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/?t={}",
        info.ws_port, info.token
    ))
    .await
    .unwrap()
    .0;
    client
        .control_tx
        .send(ControlMessage::RequestKeyframe)
        .await
        .unwrap();

    let window_start = Instant::now();
    let warmup = Duration::from_secs(warmup_secs);
    let window = Duration::from_secs(duration_secs);
    let mut frames = 0u64;
    let mut payload_bytes = 0u64;
    let mut keyframes = 0u64;
    let mut stalls = 0u64;
    let mut reconnects = 0u32;
    let mut last_frame = Instant::now();
    let mut gaps = Vec::new();
    let mut offsets = Vec::new();
    let mut buckets = vec![0u32; duration_secs as usize];
    let mut screen_stats = None;
    // Welcome clamps synthetic sources to a fixed size; the encoded stream
    // follows the capture source, so track the real dimensions from streamInfo.
    let mut dimensions = (info.width, info.height);
    let cpu_start = cpu_seconds();
    while window_start.elapsed() < warmup + window {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(Message::Binary(bin)))) => {
                let header = 8;
                assert!(bin.len() > header + 9, "video frame too short");
                let keyframe = bin[header] != 0;
                let len =
                    u32::from_be_bytes(bin[header + 5..header + 9].try_into().unwrap()) as usize;
                let pts = u32::from_be_bytes(bin[header + 1..header + 5].try_into().unwrap());
                assert_eq!(header + 9 + len, bin.len(), "frame length header mismatch");
                let now = Instant::now();
                let elapsed = now.duration_since(window_start);
                if elapsed >= warmup && elapsed < warmup + window {
                    frames += 1;
                    payload_bytes += len as u64;
                    keyframes += u64::from(keyframe);
                    gaps.push(now.duration_since(last_frame).as_secs_f64() * 1000.0);
                    offsets.push(elapsed.as_secs_f64() * 1000.0 - pts as f64);
                    buckets[(elapsed - warmup).as_secs() as usize] += 1;
                }
                last_frame = now;
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                    if msg["type"] == "screenStats" {
                        screen_stats = Some(msg.clone());
                    }
                    if msg["type"] == "streamInfo" || msg["type"] == "videoReset" {
                        if let (Some(width), Some(height)) =
                            (msg["width"].as_u64(), msg["height"].as_u64())
                        {
                            dimensions = (width as u32, height as u32);
                            client
                                .control_tx
                                .send(ControlMessage::RequestKeyframe)
                                .await
                                .unwrap();
                        }
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(None) | Ok(Some(Err(_))) => {
                reconnects += 1;
                assert!(reconnects <= 5, "video bridge never recovered");
                ws = tokio_tungstenite::connect_async(format!(
                    "ws://127.0.0.1:{}/?t={}",
                    info.ws_port, info.token
                ))
                .await
                .expect("live remote session must retain its video bridge")
                .0;
                client
                    .control_tx
                    .send(ControlMessage::RequestKeyframe)
                    .await
                    .unwrap();
            }
            Err(_) => {
                stalls += 1;
                assert!(
                    last_frame.elapsed() < Duration::from_secs(10),
                    "pipeline produced no frame for 10 s"
                );
            }
        }
    }
    let measured = window_start.elapsed();
    let cpu_percent = (cpu_seconds() - cpu_start) / measured.as_secs_f64() * 100.0;
    let wire_samples = samples.1.lock().unwrap().clone();
    let wire_samples = &wire_samples[(warmup_secs as usize).min(wire_samples.len())..];
    let wire_mbps = wire_samples.iter().sum::<f64>() / wire_samples.len().max(1) as f64 * 8.0 / 1e6;
    let steady_start = window_start + warmup;
    client.disconnect();
    drop(ws);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !server.active_sessions().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("disconnect must release the session");
    // MF/D3D destruction may finish asynchronously after the session slot is
    // released; sample the idle process after driver cleanup settles.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let idle_private_mib = mem_sample().0;
    let samples_log = sampler.stop();
    let in_window: Vec<&(Instant, f64, f64)> = samples_log
        .iter()
        .filter(|(at, _, _)| *at >= steady_start && *at <= window_start + measured)
        .collect();
    let mut private: Vec<f64> = in_window.iter().map(|(_, p, _)| *p).collect();
    let mut working: Vec<f64> = in_window.iter().map(|(_, _, w)| *w).collect();
    private.sort_by(|a, b| a.total_cmp(b));
    working.sort_by(|a, b| a.total_cmp(b));
    let median = |values: &[f64]| {
        if values.is_empty() {
            return 0.0;
        }
        values[values.len() / 2]
    };
    let peak_private_mib = samples_log
        .iter()
        .map(|(_, p, _)| *p)
        .fold(f64::NEG_INFINITY, f64::max);
    let last_stats = screen_stats.unwrap_or_else(|| {
        samples
            .0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    });
    server.stop().await;
    gaps.sort_by(f64::total_cmp);
    offsets.sort_by(f64::total_cmp);
    let p95 = |v: &[f64]| v.get(v.len() * 95 / 100).copied().unwrap_or(0.0);
    let fps_mean = frames as f64 / duration_secs as f64;
    let fps_stddev = (buckets
        .iter()
        .map(|&n| (n as f64 - fps_mean).powi(2))
        .sum::<f64>()
        / duration_secs as f64)
        .sqrt();

    BenchOutcome {
        cpu_percent,
        wire_mbps,
        codec: info.codec,
        hardware: info.hardware,
        transport,
        requested_fps: fps,
        requested_kbps: bitrate_kbps,
        resolution: dimensions,
        bridge_fps: frames as f64 / duration_secs as f64,
        payload_mbps: payload_bytes as f64 * 8.0 / duration_secs as f64 / 1e6,
        keyframes,
        stalls,
        reconnects,
        applied_fps: last_stats["appliedFps"].as_u64(),
        applied_kbps: last_stats["appliedBitrateKbps"].as_u64(),
        loss_permille: last_stats["lossPermille"].as_u64(),
        steady_private_mib: median(&private),
        steady_working_mib: median(&working),
        peak_private_mib,
        idle_private_mib,
        gap_p95_ms: p95(&gaps),
        gap_max_ms: gaps.last().copied().unwrap_or(0.0),
        // Relative queue jitter only: wire timestamps have a different epoch.
        jitter_p95_ms: p95(&offsets) - offsets.first().copied().unwrap_or(0.0),
        minimum_fps: buckets.into_iter().min().unwrap_or(0),
        fps_stddev,
    }
}

fn bench_line(o: &BenchOutcome) -> String {
    format!(
        "{:<5} {:<2} {:<7} {:>4}fps {:>6}kbps {:>9}x{:<4} bridge={:>6.2}fps payload={:>7.2}Mbps idr={:>4} stalls={:>3} recon={} applied={}fps/{}kbps loss={}permille steady={:>7.1}MiB(ws {:>7.1}) peak={:>7.1}MiB idle={:>7.1}MiB gap_p95={:.2}ms gap_max={:.2}ms relative_jitter_p95={:.2}ms min_fps={} fps_stddev={:.2}",
        o.codec,
        if o.hardware { "hw" } else { "sw" },
        format!("{:?}", o.transport),
        o.requested_fps,
        o.requested_kbps,
        o.resolution.0,
        o.resolution.1,
        o.bridge_fps,
        o.payload_mbps,
        o.keyframes,
        o.stalls,
        o.reconnects,
        o.applied_fps.map(|v| v.to_string()).unwrap_or("-".into()),
        o.applied_kbps.map(|v| v.to_string()).unwrap_or("-".into()),
        o.loss_permille.map(|v| v.to_string()).unwrap_or("-".into()),
        o.steady_private_mib,
        o.steady_working_mib,
        o.peak_private_mib,
        o.idle_private_mib,
        o.gap_p95_ms, o.gap_max_ms, o.jitter_p95_ms, o.minimum_fps, o.fps_stddev,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "capture → encode → transport throughput matrix; about 4 minutes"]
async fn throughput_matrix() {
    let capture = (2560u32, 1440u32); // native resolution of the development host
    let fps = 60u32;
    let cases: Vec<(Vec<String>, bool, Transport, Vec<u32>)> = vec![
        (
            vec!["h264".into()],
            true,
            Transport::Hybrid,
            vec![2000, 4000, 8000, 16000, 32000],
        ),
        (
            vec!["h264".into()],
            false,
            Transport::Hybrid,
            vec![8000, 16000],
        ),
        (
            vec!["av1".into()],
            false,
            Transport::Hybrid,
            vec![8000, 16000],
        ),
        (vec!["hevc".into()], false, Transport::Hybrid, vec![8000]),
        (vec!["h264".into()], true, Transport::Tcp, vec![8000, 16000]),
    ];
    let mut lines = Vec::new();
    for (codecs, hardware, transport, bitrates) in cases {
        for bitrate_kbps in bitrates {
            let outcome = bench_config(
                codecs.clone(),
                hardware,
                transport,
                fps,
                bitrate_kbps,
                Some(capture),
                3,
                6,
            )
            .await;
            let line = bench_line(&outcome);
            eprintln!("THROUGHPUT {line}");
            lines.push(line);
        }
    }
    let path =
        std::env::var("BENCH_OUT").unwrap_or_else(|_| "target/throughput-matrix.txt".to_string());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut combined = std::fs::read_to_string(&path).unwrap_or_default();
    combined.push_str("\n=== run ===\n");
    for line in &lines {
        combined.push_str(line);
        combined.push('\n');
    }
    std::fs::write(&path, combined).expect("write benchmark results");
    eprintln!("THROUGHPUT results appended to {path}");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "interactive native H.265 loopback matrix; owns a moving window"]
async fn native_hevc_motion_matrix() {
    use std::io::Write;
    let list = |name: &str, fallback: &str| -> Vec<u32> {
        std::env::var(name)
            .unwrap_or_else(|_| fallback.into())
            .split(',')
            .map(|value| value.trim().parse().expect("invalid benchmark parameter"))
            .collect()
    };
    let fps_values = list("HEVC_BENCH_FPS", "30,50,60");
    let bitrates = list("HEVC_BENCH_KBPS", "8000,16000,24000");
    let seconds = std::env::var("HEVC_BENCH_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let transports = match std::env::var("HEVC_BENCH_TRANSPORT").as_deref() {
        Ok("tcp") => vec![Transport::Tcp],
        Ok("hybrid") => vec![Transport::Hybrid],
        _ => vec![Transport::Tcp, Transport::Hybrid],
    };
    let path = std::env::var("BENCH_OUT").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/hevc-motion-matrix.txt"
        )
        .into()
    });
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut results = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(results, "\n=== native HEVC motion; 2 s warmup / {seconds} s sample; capture→encode→real loopback→WebSocket; WebView excluded ===").unwrap();
    let _timer = motion::Timer::new();
    let _motion = motion::Stimulus::new().unwrap();
    for transport in transports {
        for &fps in &fps_values {
            for &kbps in &bitrates {
                let outcome = bench_config(
                    vec!["hevc".into()],
                    true,
                    transport,
                    fps,
                    kbps,
                    None,
                    2,
                    seconds,
                )
                .await;
                assert_eq!(outcome.codec, "hevc");
                assert!(
                    outcome.hardware,
                    "native HEVC comparison requires hardware encoding"
                );
                let line = bench_line(&outcome);
                eprintln!("HEVC_MOTION {line}");
                writeln!(results, "{line}").unwrap();
                results.flush().unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit FEC lossless CPU/memory/traffic comparison; about 20 seconds"]
async fn fec_lossless_comparison() {
    for enabled in [false, true] {
        let out = bench_config_with_fec(
            vec!["h264".into()],
            true,
            Transport::Hybrid,
            30,
            8000,
            Some((1280, 720)),
            2,
            4,
            enabled,
        )
        .await;
        eprintln!(
            "FEC_COMPARE enabled={enabled} cpu_one_core={:.1}% wire={:.3}Mbps {}",
            out.cpu_percent,
            out.wire_mbps,
            bench_line(&out)
        );
        assert!(
            out.bridge_fps >= 27.0,
            "lossless FEC comparison must sustain 30 fps: {}",
            out.bridge_fps
        );
        assert_eq!(out.loss_permille, Some(0));
    }
}
