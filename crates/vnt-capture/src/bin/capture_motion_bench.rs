//! Explicit interactive benchmark: owns a checkerboard window moving at 60 Hz.
//! capture-motion-bench [frames=180] [fps=30] [graphics|dxgi|gdi|all]
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use screencap::{CaptureType, Monitor, PixelFormat, ScreenGrabber};
    use std::time::{Duration, Instant};
    let args: Vec<_> = std::env::args().collect();
    let frames = args
        .get(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(180)
        .max(1);
    let fps = args
        .get(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(30)
        .clamp(1, 120);
    let selected = args.get(3).map(String::as_str).unwrap_or("all");
    let _timer = motion::Timer::new();
    let _motion = motion::Stimulus::new()?;
    let monitor = Monitor::primary()?;
    let (w, h) = monitor.size()?;
    for (name, backend) in [
        ("graphics", CaptureType::Graphics),
        ("dxgi", CaptureType::Dxgi),
        ("gdi", CaptureType::Gdi),
    ] {
        if selected != "all" && selected != name {
            continue;
        }
        let mut grabber = match ScreenGrabber::new(&monitor, backend) {
            Ok(g) => g,
            Err(e) => {
                println!("{name}: unavailable: {e}");
                continue;
            }
        };
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        let mut timings = Vec::new();
        let mut gaps = Vec::new();
        let mut previous = Instant::now();
        let mut start = previous;
        for i in 0..frames + 30 {
            let tick = Instant::now();
            // Match the production path's per-frame allocation and readback.
            let mut pixels = vec![0; w as usize * h as usize * 4];
            loop {
                match grabber.next_frame_region_format(&mut pixels, None, PixelFormat::BGRA) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            let now = Instant::now();
            if i == 29 {
                start = now;
            }
            if i >= 30 {
                timings.push(tick.elapsed().as_secs_f64() * 1000.0);
                gaps.push(now.duration_since(previous).as_secs_f64() * 1000.0);
            }
            previous = now;
            if let Some(wait) = interval.checked_sub(tick.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        let stalls = gaps.iter().filter(|v| **v > 80.0).count();
        timings.sort_by(f64::total_cmp);
        gaps.sort_by(f64::total_cmp);
        println!("MOTION {name} {w}x{h} requested={fps} fps={:.2} grab_p50={:.2}ms grab_p95={:.2}ms gap_p95={:.2}ms gap_max={:.2}ms stalls_over_80ms={stalls}",
            frames as f64 / elapsed, timings[frames/2], timings[frames*95/100], gaps[frames*95/100], gaps[frames-1]);
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This interactive capture benchmark requires Windows.");
}

#[cfg(windows)]
#[path = "../../tests/support/motion.rs"]
mod motion;
