use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use windows::{
    core::w,
    Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*},
};
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(ms: u32) -> u32;
    fn timeEndPeriod(ms: u32) -> u32;
}
pub struct Timer(bool);
impl Timer {
    pub fn new() -> Self {
        Self(unsafe { timeBeginPeriod(1) } == 0)
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

pub struct Stimulus {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Stimulus {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || unsafe {
            let class = w!("VntMotionBenchmark");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(paint),
                lpszClassName: class,
                ..Default::default()
            });
            let window = match CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class,
                w!("VNT capture motion benchmark"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                1280,
                800,
                None,
                None,
                None,
                None,
            ) {
                Ok(w) => w,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = tx.send(Ok(()));
            let start = std::time::Instant::now();
            while !stopped.load(Ordering::Acquire) && IsWindow(Some(window)).as_bool() {
                let tick = std::time::Instant::now();
                let t = start.elapsed().as_secs_f64();
                let _ = SetWindowPos(
                    window,
                    None,
                    400 + (t * 5.0).sin().mul_add(350.0, 0.0) as i32,
                    250 + (t * 3.7).cos().mul_add(200.0, 0.0) as i32,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if let Some(wait) =
                    std::time::Duration::from_micros(16667).checked_sub(tick.elapsed())
                {
                    std::thread::sleep(wait);
                }
            }
            let _ = DestroyWindow(window);
            let _ = UnregisterClassW(class, None);
        });
        match rx.recv()? {
            Ok(()) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Err(e) => {
                let _ = thread.join();
                Err(e.into())
            }
        }
    }
}
impl Drop for Stimulus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
unsafe extern "system" fn paint(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut ps);
        let mut bounds = RECT::default();
        let _ = GetClientRect(window, &mut bounds);
        for y in (0..bounds.bottom).step_by(40) {
            for x in (0..bounds.right).step_by(40) {
                let brush = GetStockObject(if (x / 40 + y / 40) % 2 == 0 {
                    LTGRAY_BRUSH
                } else {
                    DKGRAY_BRUSH
                });
                FillRect(
                    dc,
                    &RECT {
                        left: x,
                        top: y,
                        right: x + 40,
                        bottom: y + 40,
                    },
                    HBRUSH(brush.0),
                );
            }
        }
        let _ = EndPaint(window, &ps);
        return LRESULT(0);
    }
    DefWindowProcW(window, message, wparam, lparam)
}
