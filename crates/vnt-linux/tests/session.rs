#![cfg(target_os = "linux")]
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
struct ScreenSaver(Arc<AtomicBool>);
#[zbus::interface(name = "org.gnome.ScreenSaver")]
impl ScreenSaver {
    fn get_active(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
fn wait_for(expected: bool) {
    let due = Instant::now() + Duration::from_secs(4);
    while vnt_linux::session::unlocked() != expected {
        assert!(Instant::now() < due, "expected unlocked={expected}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
#[ignore = "requires an isolated D-Bus session"]
fn lock_unlock_and_provider_loss_fail_closed() {
    let active = Arc::new(AtomicBool::new(false));
    let service = zbus::blocking::connection::Builder::session()
        .unwrap()
        .name("org.gnome.ScreenSaver")
        .unwrap()
        .serve_at("/org/gnome/ScreenSaver", ScreenSaver(active.clone()))
        .unwrap()
        .build()
        .unwrap();
    wait_for(true);
    active.store(true, Ordering::Release);
    service
        .emit_signal(
            None::<&str>,
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
            "ActiveChanged",
            &(true,),
        )
        .unwrap();
    wait_for(false);
    active.store(false, Ordering::Release);
    wait_for(true);
    drop(service);
    wait_for(false);
}
