//! Keep desktop attachment and SendInput on one windowless OS thread.
use super::native;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};
use std::time::Duration;
use vnt_input::{InputError, InputInjector, InputResult, MouseButton};
use windows::Win32::System::{StationsAndDesktops::*, Threading::GetCurrentThreadId};

fn physical_pixels() {
    use windows::Win32::UI::HiDpi::*;
    // Dedicated input threads must use the same physical coordinate space as capture.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

struct DesktopBinding {
    original: HDESK,
    _desktop: native::Desktop,
}
impl Drop for DesktopBinding {
    fn drop(&mut self) {
        // CloseDesktop cannot close the desktop currently assigned to this thread.
        unsafe {
            let _ = SetThreadDesktop(self.original);
        }
    }
}

#[derive(Clone, Copy)]
enum Event {
    Move(f32, f32),
    Wheel(f32, f32),
    Button(MouseButton, bool),
    Key(u16, bool, bool),
}
#[derive(Default)]
struct NativeHeld {
    keys: std::collections::HashSet<(u16, bool)>,
    buttons: std::collections::HashSet<MouseButton>,
}
impl NativeHeld {
    fn record(&mut self, event: Event) {
        match event {
            Event::Key(k, e, true) => {
                self.keys.insert((k, e));
            }
            Event::Key(k, e, false) => {
                self.keys.remove(&(k, e));
            }
            Event::Button(b, true) => {
                self.buttons.insert(b);
            }
            Event::Button(b, false) => {
                self.buttons.remove(&b);
            }
            _ => {}
        }
    }
    fn release(&mut self, input: &dyn InputInjector) {
        self.keys.retain(|&(k, e)| input.key(k, e, false).is_err());
        self.buttons
            .retain(|&b| input.mouse_button(b, false).is_err());
    }
}
struct Request {
    generation: u64,
    event: Event,
    reply: mpsc::SyncSender<InputResult<()>>,
}
pub struct DesktopInjector {
    ticket: Option<Arc<AtomicU64>>,
    recovery: Option<Arc<vnt_capture::DesktopCapture>>,
    requests: mpsc::SyncSender<Request>,
}

impl DesktopInjector {
    /// Stay on this process's desktop and never attach to Winlogon, even when
    /// the GUI was explicitly launched elevated.
    pub fn for_user_monitor(monitor: isize) -> InputResult<Self> {
        Self::spawn_guarded(
            move || {
                let injector = vnt_input::SendInputInjector::for_monitor(vnt_input::HMONITOR(
                    monitor as *mut _,
                ))
                .or_else(|_| vnt_input::SendInputInjector::primary())?;
                Ok((injector, ()))
            },
            true,
        )
    }

    pub fn for_user_region(region: vnt_capture::ScreenRegion) -> InputResult<Self> {
        Self::spawn_guarded(
            move || {
                let input = vnt_input::SendInputInjector::for_region_display(
                    region.monitor_name,
                    region.screen_width,
                    region.screen_height,
                    region.orientation,
                )?;
                Ok((input, ()))
            },
            true,
        )
    }

    pub fn for_monitor(monitor: isize) -> InputResult<Self> {
        Self::spawn(move || {
            let original = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
                .map_err(|e| InputError::Os(format!("读取输入线程原桌面失败：{e}")))?;
            let desktop = native::Desktop::input_for_injection()
                .and_then(|d| {
                    d.bind()?;
                    Ok(d)
                })
                .map_err(|e| InputError::Os(format!("输入线程绑定桌面失败：{e:#}")))?;
            let binding = DesktopBinding {
                original,
                _desktop: desktop,
            };
            let injector =
                vnt_input::SendInputInjector::for_monitor(vnt_input::HMONITOR(monitor as *mut _))
                    .or_else(|_| vnt_input::SendInputInjector::primary())?;
            Ok((injector, binding))
        })
    }

    /// Desktop binding and OS input stay on the same windowless thread.
    pub fn for_switching(
        desktop: Arc<vnt_capture::DesktopCapture>,
        ready: Arc<AtomicU64>,
        ticket: Arc<AtomicU64>,
    ) -> InputResult<Self> {
        let (requests, receive) = mpsc::sync_channel::<Request>(1);
        let recovery = desktop.clone();
        std::thread::Builder::new()
            .name("desktop-input".into())
            .spawn(move || {
                physical_pixels();
                let original = match unsafe { GetThreadDesktop(GetCurrentThreadId()) } {
                    Ok(h) => h,
                    Err(_) => return,
                };
                let mut generation = 0;
                let mut binding = None;
                let mut injector: Option<vnt_input::SendInputInjector> = None;
                let mut held = NativeHeld::default();
                loop {
                    let snapshot = desktop.snapshot();
                    if generation != snapshot.generation || !snapshot.capture_ready {
                        ready.store(0, Ordering::Release);
                        if let Some(input) = &injector {
                            held.release(input);
                        }
                        injector = None;
                        drop(binding.take());
                        generation = snapshot.generation;
                    }
                    if injector.is_none() && snapshot.capture_ready {
                        let result = (|| {
                            let bound = native::Desktop::input_for_injection()?;
                            anyhow::ensure!(
                                Some(&bound.1) == snapshot.desktop.as_ref(),
                                "desktop changed before input bind"
                            );
                            bound.bind()?;
                            let guard = DesktopBinding {
                                original,
                                _desktop: bound,
                            };
                            let monitor = snapshot
                                .monitor
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("monitor unavailable"))?;
                            let input = vnt_input::SendInputInjector::for_monitor(
                                vnt_input::HMONITOR(monitor.hmonitor as *mut _),
                            )
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                            Ok::<_, anyhow::Error>((input, guard))
                        })();
                        if let Ok((input, guard)) = result {
                            if desktop.accepts(generation) {
                                binding = Some(guard);
                                held.release(&input);
                                injector = Some(input);
                                ready.store(generation, Ordering::Release);
                                tracing::info!(
                                    generation,
                                    elapsed_ms = snapshot.changed_at.elapsed().as_millis(),
                                    "输入线程恢复"
                                );
                            }
                        }
                    }
                    let request = match receive.recv_timeout(Duration::from_millis(50)) {
                        Ok(r) => r,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let result = if request.generation != generation
                        || !desktop.accepts(generation)
                        || native::desktop_name().ok().as_ref() != snapshot.desktop.as_ref()
                    {
                        Err(InputError::Os("桌面已变化，丢弃过期输入".into()))
                    } else if let Some(input) = &injector {
                        match request.event {
                            Event::Move(x, y) => input.mouse_move(x, y),
                            Event::Wheel(x, y) => input.wheel(x, y),
                            Event::Button(b, d) => input.mouse_button(b, d),
                            Event::Key(k, e, d) => input.key(k, e, d),
                        }
                    } else {
                        Err(InputError::Os("输入桌面恢复中".into()))
                    };
                    if result.is_ok() {
                        held.record(request.event);
                    }
                    let _ = request.reply.send(result);
                }
                if let Some(input) = &injector {
                    held.release(input);
                }
                ready.store(0, Ordering::Release);
            })
            .map_err(|e| InputError::Os(e.to_string()))?;
        Ok(Self {
            requests,
            ticket: Some(ticket),
            recovery: Some(recovery),
        })
    }

    fn spawn<I: InputInjector + 'static, G: 'static>(
        initialize: impl FnOnce() -> InputResult<(I, G)> + Send + 'static,
    ) -> InputResult<Self> {
        Self::spawn_guarded(initialize, false)
    }

    fn spawn_guarded<I: InputInjector + 'static, G: 'static>(
        initialize: impl FnOnce() -> InputResult<(I, G)> + Send + 'static,
        normal_only: bool,
    ) -> InputResult<Self> {
        let (requests, receive) = mpsc::sync_channel::<Request>(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("desktop-input".into())
            .spawn(move || {
                physical_pixels();
                let (injector, _desktop) = match initialize() {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                let mut held = NativeHeld::default();
                let mut paused = false;
                loop {
                    let normal = !normal_only
                        || native::desktop_name().is_ok_and(|n| n.eq_ignore_ascii_case("default"));
                    if !normal {
                        held.release(&injector);
                        paused = true;
                    } else if paused {
                        held.release(&injector);
                        paused = false;
                    }
                    let request = match receive.recv_timeout(Duration::from_millis(50)) {
                        Ok(request) => request,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if normal_only
                        && !native::desktop_name()
                            .is_ok_and(|name| name.eq_ignore_ascii_case("default"))
                    {
                        let _ = request
                            .reply
                            .send(Err(InputError::Os("免安装版仅允许操作普通桌面".into())));
                        continue;
                    }
                    let result = match request.event {
                        Event::Move(x, y) => injector.mouse_move(x, y),
                        Event::Wheel(x, y) => injector.wheel(x, y),
                        Event::Button(button, down) => injector.mouse_button(button, down),
                        Event::Key(scan, extended, down) => injector.key(scan, extended, down),
                    };
                    if result.is_ok() {
                        held.record(request.event);
                    }
                    let _ = request.reply.send(result);
                }
                held.release(&injector);
            })
            .map_err(|e| InputError::Os(format!("创建输入线程失败：{e}")))?;
        initialized
            .recv()
            .map_err(|_| InputError::Os("输入线程初始化中断".into()))??;
        Ok(Self {
            requests,
            ticket: None,
            recovery: None,
        })
    }

    fn inject(&self, event: Event) -> InputResult<()> {
        let (reply, result) = mpsc::sync_channel(1);
        let request = Request {
            event,
            reply,
            generation: self
                .ticket
                .as_ref()
                .map_or(0, |t| t.load(Ordering::Acquire)),
        };
        if self.ticket.is_some() {
            self.requests
                .try_send(request)
                .map_err(|_| InputError::Os("输入线程忙或已退出".into()))?;
            result
                .recv_timeout(Duration::from_millis(250))
                .map_err(|_| {
                    if let Some(recovery) = &self.recovery {
                        recovery.request_restart();
                    }
                    InputError::Os("输入线程响应超时".into())
                })?
        } else {
            self.requests
                .send(request)
                .map_err(|_| InputError::Os("输入线程已退出".into()))?;
            result
                .recv()
                .map_err(|_| InputError::Os("输入线程未返回执行结果".into()))?
        }
    }
}
impl InputInjector for DesktopInjector {
    fn mouse_move(&self, x: f32, y: f32) -> InputResult<()> {
        self.inject(Event::Move(x, y))
    }
    fn wheel(&self, x: f32, y: f32) -> InputResult<()> {
        self.inject(Event::Wheel(x, y))
    }
    fn mouse_button(&self, button: MouseButton, down: bool) -> InputResult<()> {
        self.inject(Event::Button(button, down))
    }
    fn key(&self, scan: u16, extended: bool, down: bool) -> InputResult<()> {
        self.inject(Event::Key(scan, extended, down))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread::ThreadId};
    struct Recorder {
        thread: ThreadId,
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    impl InputInjector for Recorder {
        fn mouse_move(&self, _: f32, _: f32) -> InputResult<()> {
            assert_eq!(std::thread::current().id(), self.thread);
            self.calls.lock().unwrap().push("move");
            Ok(())
        }
        fn wheel(&self, _: f32, _: f32) -> InputResult<()> {
            assert_eq!(std::thread::current().id(), self.thread);
            self.calls.lock().unwrap().push("wheel");
            Ok(())
        }
        fn mouse_button(&self, _: MouseButton, down: bool) -> InputResult<()> {
            assert_eq!(std::thread::current().id(), self.thread);
            self.calls
                .lock()
                .unwrap()
                .push(if down { "down" } else { "up" });
            Ok(())
        }
        fn key(&self, _: u16, _: bool, _: bool) -> InputResult<()> {
            assert_eq!(std::thread::current().id(), self.thread);
            Err(InputError::Os("test failure".into()))
        }
    }
    #[test]
    fn all_input_runs_on_the_initializing_thread_and_returns_failures() {
        let caller = std::thread::current().id();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let input = DesktopInjector::spawn(move || {
            assert_ne!(caller, std::thread::current().id());
            Ok((
                Recorder {
                    thread: std::thread::current().id(),
                    calls: recorded,
                },
                (),
            ))
        })
        .unwrap();
        input.mouse_move(0.5, 0.5).unwrap();
        input.mouse_button(MouseButton::Left, true).unwrap();
        input.mouse_button(MouseButton::Left, false).unwrap();
        input.wheel(0.0, 1.0).unwrap();
        assert!(input.key(30, false, true).is_err());
        assert_eq!(*calls.lock().unwrap(), ["move", "down", "up", "wheel"]);
    }
    #[test]
    fn initialization_failure_is_reported_to_the_session() {
        let result =
            DesktopInjector::spawn::<Recorder, ()>(|| Err(InputError::Os("desktop denied".into())));
        assert!(matches!(result, Err(InputError::Os(error)) if error == "desktop denied"));
    }

    #[test]
    #[ignore = "sends one zero-distance relative mouse event on the active desktop"]
    fn native_sendinput_requires_journal_playback_access() {
        // No clicks, keys, focus changes, or cursor displacement. Kept opt-in
        // because even this inert event touches the interactive input stream.
        std::thread::spawn(|| unsafe {
            use windows::Win32::{
                Foundation::{SetLastError, ERROR_SUCCESS},
                UI::Input::KeyboardAndMouse::*,
            };
            let original = GetThreadDesktop(GetCurrentThreadId()).unwrap();
            let event = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dwFlags: MOUSEEVENTF_MOVE,
                        dwExtraInfo: vnt_input::INPUT_EXTRA_INFO,
                        ..Default::default()
                    },
                },
            };
            let restricted = native::Desktop::input().unwrap();
            restricted.bind().unwrap();
            let restricted_guard = DesktopBinding { original, _desktop: restricted };
            SetLastError(ERROR_SUCCESS);
            let without_access = SendInput(&[event], std::mem::size_of::<INPUT>() as i32);
            let without_error = std::io::Error::last_os_error();
            drop(restricted_guard);

            let authorized = native::Desktop::input_for_injection().unwrap();
            authorized.bind().unwrap();
            let authorized_guard = DesktopBinding { original, _desktop: authorized };
            SetLastError(ERROR_SUCCESS);
            let with_access = SendInput(&[event], std::mem::size_of::<INPUT>() as i32);
            let with_error = std::io::Error::last_os_error();
            drop(authorized_guard);
            assert_eq!(without_access, 0, "restricted desktop unexpectedly accepted input");
            assert_eq!(with_access, 1, "authorized desktop rejected input: {with_error}");
            eprintln!("capture-only desktop: {without_access}/1, {without_error}; injection desktop: {with_access}/1");
        }).join().unwrap();
    }
}

#[cfg(test)]
mod switching_tests {
    use super::*;
    #[test]
    fn stale_input_is_rejected_without_os_injection() {
        let d = vnt_capture::DesktopCapture::new(0, None);
        let ready = Arc::new(AtomicU64::new(0));
        let input =
            DesktopInjector::for_switching(d.clone(), ready.clone(), Arc::new(AtomicU64::new(1)))
                .unwrap();
        assert!(input.key(30, false, true).is_err());
        d.set_desktop(Some("unavailable-test-desktop".into()));
        assert!(input.mouse_button(MouseButton::Left, true).is_err());
        assert_eq!(ready.load(Ordering::Acquire), 0);
    }
}
