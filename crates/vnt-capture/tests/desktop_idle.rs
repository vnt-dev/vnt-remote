//! Real-desktop regression: WGC must not wait forever when nothing changes.
use std::time::{Duration, Instant};
use vnt_capture::SharedCapture;

#[test]
#[ignore = "requires explicit interactive desktop capture"]
fn portable_capture_keeps_its_subscription_alive_and_reopens_after_desktop_pause() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let desktop = Arc::new(AtomicBool::new(true));
    let available = Arc::new(AtomicBool::new(false));
    let capture =
        vnt_capture::SharedCapture::resilient_monitor(0, desktop.clone(), available.clone());
    let mut subscription = capture.subscribe(30).unwrap();
    let cancel = subscription.cancel_handle();
    let (sent, frames) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        while let Some(frame) = subscription.recv() {
            if sent.send(frame.captured_at).is_err() {
                break;
            }
        }
    });
    let first = frames
        .recv_timeout(Duration::from_secs(5))
        .expect("initial portable frame");
    desktop.store(false, Ordering::Release);
    available.store(false, Ordering::Release);
    std::thread::sleep(Duration::from_millis(350));
    while frames.try_recv().is_ok() {}
    assert!(matches!(
        frames.recv_timeout(Duration::from_millis(150)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    desktop.store(true, Ordering::Release);
    let resumed = frames
        .recv_timeout(Duration::from_secs(5))
        .expect("capture must recover without replacing the subscriber");
    assert!(resumed > first);
    assert!(available.load(Ordering::Acquire));
    cancel.cancel();
    consumer.join().unwrap();
}

#[test]
#[ignore = "requires explicit interactive desktop capture"]
fn idle_desktop_keeps_frames_timestamped_and_stops_promptly() {
    capture_and_stop();
}

#[test]
#[ignore = "requires explicit interactive desktop capture"]
fn repeated_native_capture_teardown_does_not_unload_live_callback_code() {
    for _ in 0..6 {
        capture_and_stop();
    }
}

fn capture_and_stop() {
    let capture = SharedCapture::monitor(0);
    let mut subscription = capture.subscribe(30).expect("real desktop capture");
    let cancel = subscription.cancel_handle();
    // Bound this regression even if a future change restores the infinite wait.
    let (done, shutdown) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if shutdown.recv_timeout(Duration::from_secs(8)).is_err() {
            cancel.cancel();
        }
    });
    let mut timestamps = Vec::new();
    while timestamps.len() < 4 {
        let Some(frame) = subscription.recv() else {
            break;
        };
        assert_eq!(
            frame.bgra.len(),
            frame.width as usize * frame.height as usize * 4
        );
        timestamps.push(frame.pts_ms);
    }
    let stopping = Instant::now();
    capture.cancel();
    drop(subscription);
    drop(capture);
    let stop_time = stopping.elapsed();
    let _ = done.send(());
    watchdog.join().unwrap();
    assert_eq!(timestamps.len(), 4, "capture stalled on the idle desktop");
    assert!(timestamps.windows(2).all(|pair| pair[1] > pair[0]));
    assert!(
        stop_time < Duration::from_secs(1),
        "stop timed out instead of joining: {stop_time:?}"
    );
}
