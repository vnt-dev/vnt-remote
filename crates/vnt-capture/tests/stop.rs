//! Cancellation and bounded teardown must also work with stalled consumers.
use std::time::{Duration, Instant};
use vnt_capture::{CaptureSubscription, SharedCapture};

#[test]
fn invalid_monitor_fails_before_starting_a_capture_thread() {
    let capture = SharedCapture::monitor(u32::MAX);
    assert!(capture.subscribe(30).is_err());
}

fn stalled_capture() -> (SharedCapture, CaptureSubscription) {
    let capture = SharedCapture::synthetic(320, 240);
    let subscription = capture.subscribe(60).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    (capture, subscription)
}

#[test]
fn source_cancel_with_stalled_subscriber() {
    let (capture, mut subscription) = stalled_capture();
    let started = Instant::now();
    capture.cancel();
    assert!(subscription.recv().is_none());
    drop(subscription);
    drop(capture);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn last_subscription_drop_stops_capture_promptly() {
    let (capture, subscription) = stalled_capture();
    let started = Instant::now();
    drop(subscription);
    drop(capture);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn cancel_handle_works_with_a_live_subscription_and_allows_reconnect() {
    let (capture, mut subscription) = stalled_capture();
    let cancel = subscription.cancel_handle();
    cancel.cancel();
    cancel.cancel();
    assert!(cancel.is_cancelled());
    assert!(subscription.recv().is_none());
    let mut fresh = capture.subscribe(30).unwrap();
    drop(subscription);
    assert!(fresh.recv().is_some());
    capture.cancel();
}

#[test]
fn cancelling_a_low_rate_source_interrupts_its_pacing_wait() {
    let capture = SharedCapture::synthetic(16, 16);
    let mut subscription = capture.subscribe(1).unwrap();
    assert!(subscription.recv().is_some());
    let started = Instant::now();
    subscription.cancel_handle().cancel();
    drop(subscription);
    drop(capture);
    assert!(started.elapsed() < Duration::from_millis(500));
}
