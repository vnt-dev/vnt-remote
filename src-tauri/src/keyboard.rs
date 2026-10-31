//! Capture physical keyboard input before Windows handles Alt+Tab/Win shortcuts.
//! Each guard belongs to one hovered control surface and one connection.

use crate::commands::KeyboardCaptureArea;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;
use vnt_proto::{control::ControlMessage, pb};
use windows::Win32::{
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::ScreenToClient,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX},
    UI::WindowsAndMessaging::*,
};

struct Capture {
    screens: Option<crate::client::screens::State>,
    target: Option<crate::client::ScreenInputTarget>,
    hwnd: usize,
    area: Arc<Mutex<KeyboardCaptureArea>>,
    active: Arc<AtomicBool>,
    tx: mpsc::Sender<ControlMessage>,
    connected: Arc<AtomicBool>,
    #[cfg(test)]
    pointer_override: Option<usize>,
    pressed: BTreeSet<(u32, bool)>,
    failed: bool,
}

impl Capture {
    fn pointer_window(&self) -> usize {
        #[cfg(test)]
        if let Some(window) = self.pointer_override {
            return window;
        }
        unsafe {
            let mut point = POINT::default();
            if self.hwnd == 0 || GetCursorPos(&mut point).is_err() {
                return usize::MAX;
            }
            let hwnd = HWND(self.hwnd as *mut _);
            if GetAncestor(WindowFromPoint(point), GA_ROOT) != hwnd
                || !ScreenToClient(hwnd, &mut point).as_bool()
            {
                return usize::MAX;
            }
            let area = *self.area.lock().unwrap();
            if point.x >= area.left
                && point.x < area.right
                && point.y >= area.top
                && point.y < area.bottom
            {
                self.hwnd
            } else {
                usize::MAX
            }
        }
    }

    fn send(&mut self, scancode: u32, extended: bool, down: bool) {
        if !self.connected.load(Ordering::Acquire) {
            return;
        }
        let event = pb::InputEvent {
            ev: Some(pb::input_event::Ev::Key(pb::KeyEvent {
                scancode,
                extended,
                down,
            })),
        };
        // Never block the low-level hook. Losing reliable input terminates the
        // session so the server releases all held keys.
        let message = if let Some(screens) = &self.screens {
            let route = {
                let state = screens.lock().unwrap();
                if self.target.is_some_and(|target| {
                    state.active_stream_id != target.stream_id
                        || !state.screens.iter().any(|s| {
                            s.stream_id == target.stream_id
                                && s.generation == target.generation
                                && s.available
                        })
                }) {
                    return;
                }
                state
                    .screens
                    .iter()
                    .find(|screen| screen.stream_id == state.active_stream_id)
                    .map(|screen| (screen.stream_id, screen.generation, state.revision))
            };
            crate::client::screens::input(screens, ControlMessage::InputEvent(event), route)
        } else {
            Some(ControlMessage::InputEvent(event))
        };
        let Some(message) = message else {
            return;
        };
        if self.tx.try_send(message).is_err() {
            self.failed = true;
            self.active.store(false, Ordering::Release);
            unsafe { PostQuitMessage(0) };
        }
    }

    fn release(&mut self) {
        for (scancode, extended) in std::mem::take(&mut self.pressed) {
            self.send(scancode, extended, false);
        }
    }

    fn route(&mut self, key: &KBDLLHOOKSTRUCT, down: bool, pointer_window: usize) -> bool {
        if !self.connected.load(Ordering::Acquire) {
            self.pressed.clear();
            return false;
        }
        let owns_screen = self.target.is_none_or(|target| {
            self.screens.as_ref().is_some_and(|screens| {
                let state = screens.lock().unwrap();
                state.active_stream_id == target.stream_id
                    && state.screens.iter().any(|s| {
                        s.stream_id == target.stream_id
                            && s.generation == target.generation
                            && s.available
                    })
            })
        });
        if !self.active.load(Ordering::Acquire) || pointer_window != self.hwnd || !owns_screen {
            self.release();
            return false;
        }
        // Ignore only our own injection. Blanket LLKHF_INJECTED filtering
        // breaks system shortcuts when the controller itself is used via RDP
        // or another input-injecting application. Consume our own tagged keys
        // here without forwarding them to prevent a webview feedback loop.
        if key.flags.contains(LLKHF_INJECTED) && key.dwExtraInfo == vnt_input::INPUT_EXTRA_INFO {
            return true;
        }
        let Some(scan) = scan_code(key) else {
            return false;
        };
        if down {
            self.pressed.insert(scan);
            self.send(scan.0, scan.1, true);
        } else if self.pressed.remove(&scan) {
            self.send(scan.0, scan.1, false);
        } else {
            // A down may have reached the webview before capture was enabled,
            // or after Windows skipped/removed the hook. Let its matching up
            // use that same DOM path instead of swallowing the only release.
            return false;
        }
        true
    }
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

// F11 stays in the webview as the local fullscreen shortcut. Pause uses an E1
// sequence which the wire protocol cannot represent; leave it to the DOM hint.
fn capturable(key: &KBDLLHOOKSTRUCT) -> bool {
    !matches!(key.vkCode, 0x7a | 0x13) && scan_code(key).is_some()
}

fn scan_code(key: &KBDLLHOOKSTRUCT) -> Option<(u32, bool)> {
    let scan = if key.scanCode == 0 {
        // Some input-injecting applications supply only a virtual key.
        unsafe { MapVirtualKeyW(key.vkCode, MAPVK_VK_TO_VSC_EX) }
    } else if key.scanCode <= 255 {
        key.scanCode
    } else {
        return None;
    };
    let code = scan & 0xff;
    (code != 0).then_some((
        code,
        key.flags.contains(LLKHF_EXTENDED) || scan & 0xff00 == 0xe000,
    ))
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let swallowed = CAPTURE.with_borrow_mut(|capture| {
            let Some(capture) = capture else { return false };
            if !capturable(key) {
                return false;
            }
            let down = match wparam.0 as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => true,
                WM_KEYUP | WM_SYSKEYUP => false,
                _ => return false,
            };
            capture.route(key, down, capture.pointer_window())
        });
        if swallowed {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

pub struct KeyboardCapture {
    thread_id: u32,
    active: Arc<AtomicBool>,
    area: Arc<Mutex<KeyboardCaptureArea>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeyboardCapture {
    pub fn start(
        hwnd: HWND,
        area: KeyboardCaptureArea,
        tx: mpsc::Sender<ControlMessage>,
        connected: Arc<AtomicBool>,
        screens: crate::client::screens::State,
        target: Option<crate::client::ScreenInputTarget>,
        on_failure: impl FnOnce() + Send + 'static,
    ) -> Result<Self, String> {
        let hwnd = unsafe { GetAncestor(hwnd, GA_ROOT) }.0 as usize;
        let area = Arc::new(Mutex::new(area));
        let bounds = area.clone();
        let active = Arc::new(AtomicBool::new(true));
        let enabled = active.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("remote-keyboard".into())
            .spawn(move || unsafe {
                // Create the message queue before publishing the thread ID.
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                CAPTURE.with_borrow_mut(|slot| {
                    *slot = Some(Capture {
                        screens: Some(screens),
                        hwnd,
                        target,
                        area: bounds,
                        active: enabled,
                        tx,
                        connected,
                        #[cfg(test)]
                        pointer_override: None,
                        pressed: BTreeSet::new(),
                        failed: false,
                    })
                });
                let hook = match GetModuleHandleW(None).and_then(|module| {
                    SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(keyboard_hook),
                        Some(HINSTANCE(module.0)),
                        0,
                    )
                }) {
                    Ok(hook) => hook,
                    Err(error) => {
                        tracing::error!(%error, "键盘钩子安装失败");
                        let _ = ready_tx.send(Err(error.to_string()));
                        CAPTURE.with_borrow_mut(|slot| slot.take());
                        return;
                    }
                };
                // Release remote modifiers even if the webview is suspended
                // and no further keyboard event arrives after the cursor exits.
                let timer = SetTimer(None, 0, 20, None);
                let _ = ready_tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    if msg.message == WM_TIMER {
                        CAPTURE.with_borrow_mut(|slot| {
                            if let Some(capture) = slot {
                                if !capture.connected.load(Ordering::Acquire) {
                                    capture.pressed.clear();
                                }
                                if capture.pointer_window() != capture.hwnd {
                                    capture.release();
                                }
                            }
                        });
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if timer != 0 {
                    let _ = KillTimer(None, timer);
                }
                let _ = UnhookWindowsHookEx(hook);

                let failed = CAPTURE.with_borrow_mut(|slot| {
                    let mut capture = slot.take().unwrap();
                    capture.release();
                    capture.failed
                });
                if failed {
                    tracing::error!("远程键盘发送队列不可用，结束控制会话");
                    on_failure();
                }
            })
            .map_err(|error| error.to_string())?;
        let thread_id = ready_rx.recv().map_err(|error| error.to_string())??;
        Ok(Self {
            thread_id,
            active,
            area,
            thread: Some(thread),
        })
    }

    pub fn set_area(&self, area: KeyboardCaptureArea) {
        *self.area.lock().unwrap() = area;
    }
}

impl Drop for KeyboardCapture {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_area() -> KeyboardCaptureArea {
        KeyboardCaptureArea {
            left: i32::MIN,
            top: i32::MIN,
            right: i32::MAX,
            bottom: i32::MAX,
        }
    }

    #[test]
    fn child_capture_rejects_other_screens_and_changed_generations_without_misrouting_releases() {
        let (tx, mut rx) = mpsc::channel(16);
        let screens = Arc::new(Mutex::new(pb::ScreensState {
            revision: 7,
            active_stream_id: 2,
            screens: vec![pb::ScreenInfo {
                stream_id: 2,
                generation: 3,
                available: true,
                ..Default::default()
            }],
        }));
        let mut capture = Capture {
            screens: Some(screens.clone()),
            target: Some(crate::client::ScreenInputTarget {
                stream_id: 2,
                generation: 3,
                revision: 7,
            }),
            hwnd: 42,
            area: Arc::new(Mutex::new(test_area())),
            active: Arc::new(AtomicBool::new(true)),
            tx,
            connected: Arc::new(AtomicBool::new(true)),
            pointer_override: None,
            pressed: BTreeSet::new(),
            failed: false,
        };
        let key = KBDLLHOOKSTRUCT {
            vkCode: 0xa4,
            scanCode: 0x38,
            ..Default::default()
        };
        assert!(capture.route(&key, true, 42));
        let ControlMessage::ScreenPacket(packet) = rx.try_recv().unwrap() else {
            panic!("expected screen route");
        };
        assert_eq!(
            (packet.stream_id, packet.generation, packet.revision),
            (2, 3, 7)
        );
        {
            let mut state = screens.lock().unwrap();
            state.active_stream_id = 1;
            state.revision = 8;
        }
        assert!(!capture.route(&key, false, 42));
        assert!(capture.pressed.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "old keyup must not be stamped with the new screen route"
        );
        {
            let mut state = screens.lock().unwrap();
            state.active_stream_id = 2;
            state.screens[0].generation = 4;
        }
        assert!(!capture.route(&key, true, 42));
        assert!(
            rx.try_recv().is_err(),
            "a replacement capture generation must not inherit a stale hook"
        );
    }

    #[test]
    fn disconnected_control_does_not_swallow_or_replay_keys() {
        let (tx, mut rx) = mpsc::channel(8);
        let connected = Arc::new(AtomicBool::new(true));
        let mut capture = Capture {
            screens: None,
            target: None,
            hwnd: 42,
            area: Arc::new(Mutex::new(test_area())),
            active: Arc::new(AtomicBool::new(true)),
            tx,
            connected: connected.clone(),
            pointer_override: None,
            pressed: BTreeSet::new(),
            failed: false,
        };
        let key = KBDLLHOOKSTRUCT {
            vkCode: 0x41,
            scanCode: 0x1e,
            ..Default::default()
        };
        assert!(capture.route(&key, true, 42));
        assert!(rx.try_recv().is_ok());
        connected.store(false, Ordering::Release);
        assert!(!capture.route(&key, false, 42));
        assert!(capture.pressed.is_empty());
        connected.store(true, Ordering::Release);
        assert!(!capture.route(&key, false, 42));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn virtual_key_only_injection_maps_win_alt_and_tab_to_wire_scancodes() {
        for (vk, expected) in [
            (0x5b, (0x5b, true)),
            (0xa4, (0x38, false)),
            (0x09, (0x0f, false)),
        ] {
            let key = KBDLLHOOKSTRUCT {
                vkCode: vk,
                flags: LLKHF_INJECTED,
                ..Default::default()
            };
            assert!(capturable(&key));
            assert_eq!(scan_code(&key), Some(expected));
        }
    }

    #[test]
    fn forwards_other_software_injection_but_consumes_our_own_tag_without_feedback() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut capture = Capture {
            screens: None,
            target: None,
            hwnd: 42,
            area: Arc::new(Mutex::new(test_area())),
            active: Arc::new(AtomicBool::new(true)),
            tx,
            connected: Arc::new(AtomicBool::new(true)),
            pointer_override: None,
            pressed: BTreeSet::new(),
            failed: false,
        };
        // Nested remote-control software commonly injects the controller's
        // keyboard. These Win and Alt+Tab events are still remote input.
        for (vk, scan, flags) in [
            (0x5b, 0x5b, LLKHF_EXTENDED),
            (0xa4, 0x38, KBDLLHOOKSTRUCT_FLAGS(0)),
            (0x09, 0x0f, LLKHF_ALTDOWN),
        ] {
            let key = KBDLLHOOKSTRUCT {
                vkCode: vk,
                scanCode: scan,
                flags: flags | LLKHF_INJECTED,
                ..Default::default()
            };
            assert!(capturable(&key));
            for down in [true, false] {
                assert!(capture.route(&key, down, 42));
                let ControlMessage::InputEvent(pb::InputEvent {
                    ev: Some(pb::input_event::Ev::Key(received)),
                }) = rx.try_recv().unwrap()
                else {
                    panic!("expected key")
                };
                assert_eq!((received.scancode, received.down), (scan, down));
            }
            let own = KBDLLHOOKSTRUCT {
                dwExtraInfo: vnt_input::INPUT_EXTRA_INFO,
                ..key
            };
            assert!(capture.route(&own, true, 42));
            assert!(capture.route(&own, false, 42));
            assert!(rx.try_recv().is_err());
        }
        assert!(capture.pressed.is_empty());
    }

    #[test]
    fn installs_and_stops_a_real_windows_hook() {
        let (tx, _rx) = mpsc::channel(8);
        // No real window can match this handle: install the actual hook and
        // exercise its message pump without intercepting desktop input.
        let capture = KeyboardCapture::start(
            HWND::default(),
            test_area(),
            tx,
            Arc::new(AtomicBool::new(true)),
            Default::default(),
            None,
            || {},
        )
        .expect("Windows keyboard hook must install successfully");
        assert!(!capture.thread.as_ref().unwrap().is_finished());
        drop(capture);
    }

    #[test]
    fn keyup_without_a_native_keydown_passes_through_to_the_dom() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut capture = Capture {
            screens: None,
            target: None,
            hwnd: 42,
            area: Arc::new(Mutex::new(test_area())),
            active: Arc::new(AtomicBool::new(true)),
            tx,
            connected: Arc::new(AtomicBool::new(true)),
            pointer_override: None,
            pressed: BTreeSet::new(),
            failed: false,
        };
        let alt = KBDLLHOOKSTRUCT {
            scanCode: 0x38,
            ..Default::default()
        };
        assert!(!capture.route(&alt, false, 42));
        assert!(rx.try_recv().is_err());
        // Once native capture owns a down, it consumes exactly one matching
        // up. Another up belongs to the DOM fallback and must pass through.
        assert!(capture.route(&alt, true, 42));
        assert!(capture.route(&alt, false, 42));
        assert!(!capture.route(&alt, false, 42));
        for down in [true, false] {
            let ControlMessage::InputEvent(pb::InputEvent {
                ev: Some(pb::input_event::Ev::Key(key)),
            }) = rx.try_recv().unwrap()
            else {
                panic!("expected a key event")
            };
            assert_eq!((key.scancode, key.down), (0x38, down));
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn native_callback_swallows_and_forwards_a_physical_key_exactly_once() {
        let (tx, mut rx) = mpsc::channel(8);
        // Exercise the actual callback boundary without installing a global
        // hook, injecting input, or depending on the active desktop being accessible.
        // Inject only the pointer hit-test; all callback routing remains real.
        CAPTURE.with_borrow_mut(|slot| {
            *slot = Some(Capture {
                screens: None,
                target: None,
                hwnd: 42,
                area: Arc::new(Mutex::new(test_area())),
                active: Arc::new(AtomicBool::new(true)),
                tx,
                connected: Arc::new(AtomicBool::new(true)),
                pointer_override: Some(42),
                pressed: BTreeSet::new(),
                failed: false,
            });
        });
        let key = KBDLLHOOKSTRUCT {
            vkCode: 0x41,
            scanCode: 0x1e,
            ..Default::default()
        };
        for (message, down) in [(WM_KEYDOWN, true), (WM_KEYUP, false)] {
            let result = unsafe {
                keyboard_hook(
                    0,
                    WPARAM(message as usize),
                    LPARAM(&key as *const _ as isize),
                )
            };
            // Nonzero prevents this same key from producing a DOM event.
            assert_eq!(result.0, 1);
            let ControlMessage::InputEvent(pb::InputEvent {
                ev: Some(pb::input_event::Ev::Key(key)),
            }) = rx.try_recv().unwrap()
            else {
                panic!("expected a key event")
            };
            assert_eq!((key.scancode, key.down), (0x1e, down));
            assert!(rx.try_recv().is_err());
        }
        CAPTURE.with_borrow_mut(|slot| {
            slot.take();
        });
    }

    #[test]
    fn alt_tab_repeats_in_order_and_focus_loss_releases_the_modifier() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut capture = Capture {
            screens: None,
            target: None,
            hwnd: 42,
            area: Arc::new(Mutex::new(test_area())),
            active: Arc::new(AtomicBool::new(true)),
            tx,
            connected: Arc::new(AtomicBool::new(true)),
            pointer_override: None,
            pressed: BTreeSet::new(),
            failed: false,
        };
        let alt = KBDLLHOOKSTRUCT {
            scanCode: 0x38,
            ..Default::default()
        };
        let tab = KBDLLHOOKSTRUCT {
            scanCode: 0x0f,
            ..Default::default()
        };
        for (key, down) in [(&alt, true), (&tab, true), (&tab, true), (&tab, false)] {
            assert!(capture.route(key, down, 42));
        }
        // A switch to another local window must pass the next key through and
        // release remote Alt even when its physical keyup has not arrived.
        assert!(!capture.route(&tab, true, 99));
        let mut received = Vec::new();
        while let Ok(ControlMessage::InputEvent(pb::InputEvent {
            ev: Some(pb::input_event::Ev::Key(key)),
        })) = rx.try_recv()
        {
            received.push((key.scancode, key.down));
        }
        assert_eq!(
            received,
            [
                (0x38, true),
                (0x0f, true),
                (0x0f, true),
                (0x0f, false),
                (0x38, false)
            ]
        );
        assert!(capture.pressed.is_empty());
    }

    #[test]
    fn captures_system_shortcuts_but_leaves_fullscreen_and_pause_local() {
        for (vk, scan) in [
            (0xa4, 0x38),
            (0x09, 0x0f),
            (0xa0, 0x2a),
            (0x5b, 0x5b),
            (0x1b, 1),
        ] {
            assert!(capturable(&KBDLLHOOKSTRUCT {
                vkCode: vk,
                scanCode: scan,
                ..Default::default()
            }));
        }
        for key in [
            KBDLLHOOKSTRUCT {
                vkCode: 0x7a,
                scanCode: 0x57,
                ..Default::default()
            },
            KBDLLHOOKSTRUCT {
                vkCode: 0x13,
                scanCode: 0x45,
                ..Default::default()
            },
            KBDLLHOOKSTRUCT::default(),
        ] {
            assert!(!capturable(&key));
        }
    }

    #[test]
    fn releases_each_held_key_and_treats_congestion_as_failure() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut capture = Capture {
            screens: None,
            target: None,
            hwnd: 0,
            area: Arc::new(Mutex::new(test_area())),
            active: Arc::new(AtomicBool::new(true)),
            tx,
            connected: Arc::new(AtomicBool::new(true)),
            pointer_override: None,
            pressed: BTreeSet::from([(0x38, false), (0x0f, false), (0x1d, true)]),
            failed: false,
        };
        capture.release();
        for _ in 0..3 {
            assert!(matches!(
                rx.try_recv().unwrap(),
                ControlMessage::InputEvent(pb::InputEvent {
                    ev: Some(pb::input_event::Ev::Key(pb::KeyEvent { down: false, .. }))
                })
            ));
        }
        capture.release();
        assert!(rx.try_recv().is_err());
        drop(rx);
        capture.send(0x0f, false, true);
        assert!(capture.failed);
        assert!(!capture.active.load(Ordering::Acquire));
    }
}
