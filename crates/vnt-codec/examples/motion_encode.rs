//! Explicit native moving-window capture + hardware encoding benchmark.
//! cargo run --release -p vnt-codec --example motion_encode -- [frames=180] [h264|hevc] [kbps=16000]
#[cfg(windows)]
#[path = "../../vnt-capture/tests/support/motion.rs"]
mod motion;

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{collections::HashMap, time::Instant};
    let frames = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(180)
        .max(1);
    let codec = match std::env::args().nth(2).as_deref() {
        Some("hevc" | "h265") => vnt_proto::Codec::Hevc,
        _ => vnt_proto::Codec::H264,
    };
    let kbps = std::env::args()
        .nth(3)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(16000);
    let _timer = motion::Timer::new();
    let _motion = motion::Stimulus::new()?;
    for fps in [30, 50, 60] {
        let capture = vnt_capture::SharedCapture::monitor(0);
        let mut source = capture.subscribe(fps)?;
        let mut encoder = vnt_codec::create_encoder(codec, true)?;
        let mut configured = false;
        let mut times = Vec::new();
        let mut gaps = Vec::new();
        let mut ages = Vec::new();
        let mut pending = HashMap::new();
        let mut previous = Instant::now();
        let mut started = previous;
        let mut outputs = 0;
        let mut i = 0;
        while i < frames + 30 {
            let mut packets = Vec::new();
            let frame = source.recv().ok_or("capture stopped")?;
            if !configured {
                encoder.configure(&vnt_codec::EncodeParams::new(
                    frame.width,
                    frame.height,
                    fps,
                    kbps,
                ))?;
                configured = true;
            }
            let submitted = Instant::now();
            let measured = i >= 30;
            pending.insert(frame.pts_ms, (frame.captured_at, measured));
            encoder.encode(&frame.bgra, frame.pts_ms, &mut packets)?;
            if i >= 30 {
                times.push(submitted.elapsed().as_secs_f64() * 1000.0);
            }
            i += 1;
            if i == 30 {
                started = Instant::now();
            }
            let now = Instant::now();
            for p in packets {
                if let Some((captured, measured)) = pending.remove(&p.pts_ms) {
                    if measured {
                        ages.push(now.duration_since(captured).as_secs_f64() * 1000.0);
                        outputs += 1;
                        gaps.push(now.duration_since(previous).as_secs_f64() * 1000.0);
                    }
                }
                previous = now;
            }
        }
        times.sort_by(f64::total_cmp);
        gaps.sort_by(f64::total_cmp);
        ages.sort_by(f64::total_cmp);
        let percentile =
            |v: &[f64], percent: usize| v.get(v.len() * percent / 100).copied().unwrap_or(0.0);
        println!("MOTION_ENCODE codec={codec:?} kbps={kbps} requested={fps} fps={:.2} submit_p50={:.2}ms submit_p95={:.2}ms capture_to_output_p95={:.2}ms gap_p95={:.2}ms gap_max={:.2}ms raw_dropped={}",
            outputs as f64/started.elapsed().as_secs_f64(), percentile(&times,50), percentile(&times,95), percentile(&ages,95), percentile(&gaps,95), gaps.last().unwrap_or(&0.0), source.dropped_frames());
        capture.cancel();
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This interactive benchmark requires Windows.");
}
