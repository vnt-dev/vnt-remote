//! 抓屏基准工具：列出显示器，抓取 N 帧输出耗时统计。
//! 用法: capture-bench [monitor_index] [frames]

use std::time::Instant;
use vnt_capture::{list_monitors, CaptureError, SharedCapture};

fn main() -> Result<(), CaptureError> {
    let monitors = list_monitors()?;
    println!("显示器列表:");
    for m in &monitors {
        println!(
            "  [{}] {}x{}{} {m:?}",
            m.index,
            m.width,
            m.height,
            if m.primary { " (主)" } else { "" }
        );
    }

    let index: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let frames: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    println!("\n抓取 {frames} 帧 (显示器 {index})...");
    let capture = SharedCapture::monitor(index);
    let mut subscription = capture.subscribe(60)?;

    let mut count = 0u32;
    let mut total_ms = 0u64;
    let mut total_bytes = 0usize;
    let mut last_dim = (0u32, 0u32);
    let t0 = Instant::now();
    while count < frames {
        let f = subscription.recv().ok_or(CaptureError::Stopped)?;
        total_ms += f.capture_ms as u64;
        total_bytes += f.bgra.len();
        last_dim = (f.width, f.height);
        count += 1;
        if count.is_multiple_of(30) {
            println!("  {count}/{frames} 帧");
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "\n结果: 分辨率 {}x{}, 平均捕获 {:.2}ms/帧, 实际 {wall:.1}s => {:.1} fps, {:.1} MB/s",
        last_dim.0,
        last_dim.1,
        total_ms as f64 / count as f64,
        count as f64 / wall,
        total_bytes as f64 / 1024.0 / 1024.0 / wall,
    );
    Ok(())
}
