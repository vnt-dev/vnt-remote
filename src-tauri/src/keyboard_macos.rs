use crate::{
    client::{screens, ScreenInputTarget},
    commands::KeyboardCaptureArea,
};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::mpsc;
use vnt_proto::{control::ControlMessage, pb};
pub struct KeyboardCapture {
    active: Arc<AtomicBool>,
    area: Arc<Mutex<KeyboardCaptureArea>>,
}
impl KeyboardCapture {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        window: usize,
        area: KeyboardCaptureArea,
        tx: mpsc::Sender<ControlMessage>,
        connected: Arc<AtomicBool>,
        screens: screens::State,
        target: Option<ScreenInputTarget>,
        on_failure: impl FnOnce() + Send + 'static,
    ) -> Result<Self, String> {
        // Tauri supplied this NSWindow and it is live for this command. Native open retains it.
        let mut keyboard = unsafe { vnt_macos::Keyboard::open(window)? };
        let active = Arc::new(AtomicBool::new(true));
        let run = active.clone();
        let area = Arc::new(Mutex::new(area));
        let rect = area.clone();
        std::thread::Builder::new()
            .name("mac-keyboard".into())
            .spawn(move || {
                let mut held = BTreeSet::new();
                let send = |scan: u16, extended: bool, down: bool| -> Result<(), ()> {
                    let state = screens.lock().unwrap();
                    let stream = target.map(|t| (t.stream_id, t.generation)).or_else(|| {
                        state
                            .screens
                            .iter()
                            .find(|s| s.stream_id == state.active_stream_id)
                            .map(|s| (s.stream_id, s.generation))
                    });
                    let route = stream.map(|(id, g)| (id, g, state.revision));
                    drop(state);
                    let msg = ControlMessage::InputEvent(pb::InputEvent {
                        ev: Some(pb::input_event::Ev::Key(pb::KeyEvent {
                            scancode: scan as u32,
                            extended,
                            down,
                        })),
                    });
                    if let Some(msg) = screens::input(&screens, msg, route) {
                        tx.try_send(msg).map_err(|_| ())?;
                    }
                    Ok(())
                };
                let result = (|| -> Result<(), ()> {
                    while run.load(Ordering::Acquire) && connected.load(Ordering::Acquire) {
                        let r = *rect.lock().unwrap();
                        let value = keyboard
                            .read([r.left, r.top, r.right, r.bottom])
                            .map_err(|_| ())?;
                        if value["overflow"] == true {
                            return Err(());
                        }
                        if let Some(events) = value["events"].as_array() {
                            for event in events {
                                let key = event["key"].as_u64().ok_or(())? as u16;
                                let mapped = [false, true].into_iter().find_map(|extended| {
                                    (1..=0x7f)
                                        .find(|&scan| {
                                            vnt_input::mac_key(scan, extended) == Some(key)
                                        })
                                        .map(|scan| (scan, extended))
                                });
                                if let Some((scan, extended)) = mapped {
                                    let down = event["down"] == true;
                                    send(scan, extended, down)?;
                                    if down {
                                        held.insert((scan, extended));
                                    } else {
                                        held.remove(&(scan, extended));
                                    }
                                }
                            }
                        }
                        if value["active"] != true {
                            for (scan, e) in std::mem::take(&mut held) {
                                send(scan, e, false)?;
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    for (scan, e) in std::mem::take(&mut held) {
                        send(scan, e, false)?;
                    }
                    Ok(())
                })();
                if result.is_err() {
                    on_failure();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { active, area })
    }
    pub fn set_area(&self, area: KeyboardCaptureArea) {
        *self.area.lock().unwrap() = area;
    }
}
impl Drop for KeyboardCapture {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}
